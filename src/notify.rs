//! Outbound push notifications.
//!
//! Channels are stored in SQLite and managed from the panel; this module
//! validates their configuration, encrypts Bark payloads exactly the way the
//! Bark app expects, and delivers alert transitions.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing::{info, warn};

use crate::config::NotifyConfig;
use crate::http;
use crate::storage::Database;
use crate::types::*;

/// Which lifecycle transition produced a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertEvent {
    Triggered,
    Resolved,
}

impl AlertEvent {
    fn title_prefix(self) -> &'static str {
        match self {
            AlertEvent::Triggered => "[告警]",
            AlertEvent::Resolved => "[已恢复]",
        }
    }
}

#[derive(Clone)]
pub struct NotificationService {
    client: reqwest::Client,
    db: Arc<Database>,
    enabled: bool,
    /// Bark server root used when a channel does not carry its own.
    default_server_url: String,
}

impl NotificationService {
    pub fn new(db: Arc<Database>, config: &NotifyConfig) -> Result<Self> {
        let client = http::build_client(
            config.proxy.as_deref(),
            Duration::from_secs(config.timeout_secs.max(1)),
            &http::user_agent(),
        )?;

        let default_server_url = normalize_server_url(&config.server_url);
        if let Err(message) = validate_server_url(&default_server_url) {
            warn!("[notify] server_url is invalid ({message}); new channels need an explicit address");
        }

        Ok(Self {
            client,
            db,
            enabled: config.enabled,
            default_server_url,
        })
    }

    /// Bark server root configured in `[notify] server_url`; the panel prefills it
    /// for new channels and an empty channel address falls back to it.
    pub fn default_server_url(&self) -> &str {
        &self.default_server_url
    }

    /// Deliver an alert transition to every matching channel.
    ///
    /// Spawned so the alert loop never waits on the network; each channel is
    /// attempted exactly once.
    pub fn dispatch_alert(&self, alert: Alert, event: AlertEvent) {
        if !self.enabled {
            return;
        }

        let service = self.clone();
        tokio::spawn(async move {
            service.deliver_alert(&alert, event).await;
        });
    }

    /// Send a fixed test notification through `channel`, ignoring the global
    /// switch so an operator can verify a channel before enabling pushes.
    pub async fn send_test(&self, channel: &NotifyChannel) -> Result<(), String> {
        let params = vec![
            ("title".to_string(), "os-watcher 测试推送".to_string()),
            ("subtitle".to_string(), "渠道连通性测试".to_string()),
            (
                "body".to_string(),
                format!("如果你收到这条通知，说明「{}」配置可用。", channel.name),
            ),
            ("group".to_string(), "os-watcher".to_string()),
            ("level".to_string(), "active".to_string()),
            ("isArchive".to_string(), "1".to_string()),
        ];

        let result = self.send(channel, &params).await;
        self.record_result(channel, &result).await;
        result
    }

    async fn deliver_alert(&self, alert: &Alert, event: AlertEvent) {
        let channels = match self.db.list_notify_channels().await {
            Ok(channels) => channels,
            Err(error) => {
                warn!("Failed to load notification channels: {error}");
                return;
            }
        };

        let params = build_payload_params(alert, event);
        for channel in channels {
            if !channel.enabled || !channel.min_severity.allows(alert.severity) {
                continue;
            }

            let result = self.send(&channel, &params).await;
            self.record_result(&channel, &result).await;
            match &result {
                Ok(()) => info!(
                    "Pushed {:?} alert for {} to channel {}",
                    event, alert.hostname, channel.name
                ),
                Err(error) => warn!("Push to channel {} failed: {}", channel.name, error),
            }
        }
    }

    async fn record_result(&self, channel: &NotifyChannel, result: &Result<(), String>) {
        let error = result.as_ref().err().map(|message| message.as_str());
        if let Err(error) = self.db.record_notify_result(&channel.id, error).await {
            warn!("Failed to record push result for channel {}: {error}", channel.name);
        }
    }

    /// POST one notification. Never logs the device key or the ciphertext.
    async fn send(&self, channel: &NotifyChannel, params: &[(String, String)]) -> Result<(), String> {
        let ChannelConfig::Bark(bark) = &channel.config;
        validate_bark_config(bark)?;

        let url = format!(
            "{}/{}",
            normalize_server_url(&bark.server_url),
            bark.device_key.trim()
        );

        let form = match &bark.encryption {
            None => params.to_vec(),
            Some(encryption) => {
                let plaintext = serde_json::to_string(&payload_json(params))
                    .map_err(|error| format!("序列化推送参数失败：{error}"))?;
                let mut form = vec![("ciphertext".to_string(), encrypt(encryption, &plaintext)?)];
                if let Some(iv) = encryption.iv.as_deref().filter(|iv| !iv.is_empty()) {
                    form.push(("iv".to_string(), iv.to_string()));
                }
                form
            }
        };

        let response = self
            .client
            .post(&url)
            .form(&form)
            .send()
            .await
            .map_err(|error| format!("请求失败：{error}"))?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if !status.is_success() {
            return Err(format!("HTTP {}：{}", status.as_u16(), truncate(&body, 200)));
        }

        // Bark answers 200 with `{"code":...,"message":...}`; anything other
        // than code 200 means the push was rejected.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(code) = value.get("code") {
                if code.as_i64() != Some(200) {
                    let message = value.get("message").and_then(|m| m.as_str()).unwrap_or("");
                    return Err(format!("Bark 返回 code={code}：{message}"));
                }
            }
        }

        Ok(())
    }
}

/// Notification parameters as a JSON object — Bark decrypts the payload into
/// this same shape, so an encrypted push uses exactly the plaintext fields.
fn payload_json(params: &[(String, String)]) -> BTreeMap<&str, &str> {
    params
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect()
}

fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Message parameters for an alert transition.
pub(crate) fn build_payload_params(alert: &Alert, event: AlertEvent) -> Vec<(String, String)> {
    let body = match event {
        AlertEvent::Triggered => alert.message.clone(),
        AlertEvent::Resolved => {
            let reason = alert
                .resolved_reason
                .map(|reason| reason.label())
                .unwrap_or("已恢复");
            format!("{}（{reason}）", alert.message)
        }
    };

    vec![
        (
            "title".to_string(),
            format!("{} {}", event.title_prefix(), alert.hostname),
        ),
        ("subtitle".to_string(), alert.severity.label().to_string()),
        ("body".to_string(), body),
        ("group".to_string(), "os-watcher".to_string()),
        ("level".to_string(), level_for(alert.severity).to_string()),
        ("isArchive".to_string(), "1".to_string()),
    ]
}

fn level_for(severity: AlertSeverity) -> &'static str {
    match severity {
        AlertSeverity::Critical => "critical",
        AlertSeverity::Warning => "timeSensitive",
        AlertSeverity::Info => "passive",
    }
}

/// iOS CommonCrypto implements AES-GCM for 128/256-bit keys only.
const GCM_UNSUPPORTED_KEY_MESSAGE: &str = "GCM 模式仅支持 AES128 与 AES256 密钥";

/// Bark server root used when nothing else is configured.
pub const DEFAULT_SERVER_URL: &str = "https://api.day.app";

const SERVER_URL_MESSAGE: &str = "服务地址必须以 http:// 或 https:// 开头";

/// Trim a configured or submitted server root. Self-hosted instances may carry a
/// path prefix (`https://example.com/bark`), which is preserved; a trailing slash
/// would double up when the device key is appended, so it is dropped.
pub(crate) fn normalize_server_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

pub(crate) fn validate_server_url(url: &str) -> Result<(), String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err(SERVER_URL_MESSAGE.to_string())
    }
}

/// Validate a channel's Bark settings. Called before saving a channel and again
/// before every send, so bad data can never reach the network.
pub(crate) fn validate_bark_config(config: &BarkChannelConfig) -> Result<(), String> {
    validate_server_url(config.server_url.trim())?;

    if config.device_key.trim().is_empty() {
        return Err("设备 Key 不能为空".to_string());
    }

    if let Some(encryption) = &config.encryption {
        validate_bark_encryption(encryption)?;
    }

    Ok(())
}

fn validate_bark_encryption(encryption: &BarkEncryption) -> Result<(), String> {
    let expected_key_len = encryption.algorithm.key_len();
    if encryption.key.len() != expected_key_len {
        return Err(format!(
            "{} 密钥长度必须是 {} 个字符",
            encryption.algorithm.label(),
            expected_key_len
        ));
    }

    // iOS CommonCrypto only implements AES-GCM for 128/256-bit keys, so the Bark
    // app cannot decrypt an AES192+GCM payload even though the key length itself
    // is valid.
    if encryption.mode == BarkMode::Gcm && encryption.algorithm == BarkAlgorithm::Aes192 {
        return Err(GCM_UNSUPPORTED_KEY_MESSAGE.to_string());
    }

    let iv = encryption.iv.as_deref().unwrap_or("");
    match encryption.mode.iv_len() {
        Some(expected) => {
            if iv.len() != expected {
                return Err(format!(
                    "{} 模式必须提供 {} 个字符的 IV",
                    encryption.mode.label(),
                    expected
                ));
            }
        }
        None => {
            if !iv.is_empty() {
                return Err("ECB 模式不需要 IV".to_string());
            }
        }
    }

    Ok(())
}

/// Encrypt a Bark payload and return the base64 ciphertext.
///
/// Key and IV are the literal characters configured in the Bark app (CryptoSwift
/// `key.bytes`), not a hex encoding. CBC/ECB pad with PKCS7; GCM appends its
/// 16-byte tag to the ciphertext, matching CryptoSwift's `.combined` mode.
pub(crate) fn encrypt(encryption: &BarkEncryption, plaintext: &str) -> Result<String, String> {
    validate_bark_encryption(encryption)?;

    let key = encryption.key.as_bytes();
    let iv = encryption.iv.as_deref().unwrap_or("").as_bytes();

    let ciphertext = match encryption.mode {
        BarkMode::Cbc => encrypt_cbc(encryption.algorithm, key, iv, plaintext),
        BarkMode::Ecb => encrypt_ecb(encryption.algorithm, key, plaintext),
        BarkMode::Gcm => encrypt_gcm(encryption.algorithm, key, iv, plaintext)?,
    };

    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        ciphertext,
    ))
}

fn encrypt_cbc(algorithm: BarkAlgorithm, key: &[u8], iv: &[u8], plaintext: &str) -> Vec<u8> {
    use aes::cipher::{block_padding::Pkcs7, generic_array::GenericArray, BlockEncryptMut, KeyIvInit};

    let iv = GenericArray::from_slice(iv);
    let plaintext = plaintext.as_bytes();

    match algorithm {
        BarkAlgorithm::Aes128 => {
            cbc::Encryptor::<aes::Aes128>::new(GenericArray::from_slice(key), iv)
                .encrypt_padded_vec_mut::<Pkcs7>(plaintext)
        }
        BarkAlgorithm::Aes192 => {
            cbc::Encryptor::<aes::Aes192>::new(GenericArray::from_slice(key), iv)
                .encrypt_padded_vec_mut::<Pkcs7>(plaintext)
        }
        BarkAlgorithm::Aes256 => {
            cbc::Encryptor::<aes::Aes256>::new(GenericArray::from_slice(key), iv)
                .encrypt_padded_vec_mut::<Pkcs7>(plaintext)
        }
    }
}

fn encrypt_ecb(algorithm: BarkAlgorithm, key: &[u8], plaintext: &str) -> Vec<u8> {
    use aes::cipher::{block_padding::Pkcs7, generic_array::GenericArray, BlockEncryptMut, KeyInit};

    let plaintext = plaintext.as_bytes();

    match algorithm {
        BarkAlgorithm::Aes128 => {
            ecb::Encryptor::<aes::Aes128>::new(GenericArray::from_slice(key))
                .encrypt_padded_vec_mut::<Pkcs7>(plaintext)
        }
        BarkAlgorithm::Aes192 => {
            ecb::Encryptor::<aes::Aes192>::new(GenericArray::from_slice(key))
                .encrypt_padded_vec_mut::<Pkcs7>(plaintext)
        }
        BarkAlgorithm::Aes256 => {
            ecb::Encryptor::<aes::Aes256>::new(GenericArray::from_slice(key))
                .encrypt_padded_vec_mut::<Pkcs7>(plaintext)
        }
    }
}

fn encrypt_gcm(
    algorithm: BarkAlgorithm,
    key: &[u8],
    iv: &[u8],
    plaintext: &str,
) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::{generic_array::GenericArray, Aead, KeyInit};

    let nonce = GenericArray::from_slice(iv);
    let plaintext = plaintext.as_bytes();

    let ciphertext = match algorithm {
        BarkAlgorithm::Aes128 => {
            aes_gcm::Aes128Gcm::new(GenericArray::from_slice(key)).encrypt(nonce, plaintext)
        }
        BarkAlgorithm::Aes256 => {
            aes_gcm::Aes256Gcm::new(GenericArray::from_slice(key)).encrypt(nonce, plaintext)
        }
        BarkAlgorithm::Aes192 => return Err(GCM_UNSUPPORTED_KEY_MESSAGE.to_string()),
    };

    ciphertext.map_err(|_| "GCM 加密失败".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encryption(key: &str, iv: Option<&str>, algorithm: BarkAlgorithm, mode: BarkMode) -> BarkEncryption {
        BarkEncryption {
            algorithm,
            mode,
            key: key.to_string(),
            iv: iv.map(str::to_string),
        }
    }

    /// Golden vector from the Bark encryption documentation: AES128/CBC with
    /// ASCII key/IV `1234567890123456` over `{"body": "test", "sound": "birdsong"}`.
    #[test]
    fn aes128_cbc_matches_the_documented_vector() {
        let enc = encryption(
            "1234567890123456",
            Some("1234567890123456"),
            BarkAlgorithm::Aes128,
            BarkMode::Cbc,
        );

        let ciphertext = encrypt(&enc, r#"{"body": "test", "sound": "birdsong"}"#)
            .expect("documented vector must encrypt");

        assert_eq!(
            ciphertext,
            "+aPt5cwN9GbTLLSFri60l3h1X00u/9j1FENfWiTxhNHVLGU+XoJ15JJG5W/d/yf0"
        );
    }

    #[test]
    fn aes256_gcm_carries_the_tag_after_the_ciphertext() {
        use aes_gcm::aead::{generic_array::GenericArray, Aead, KeyInit};

        let key = "0123456789abcdef0123456789abcdef";
        let iv = "0123456789ab";
        let enc = encryption(key, Some(iv), BarkAlgorithm::Aes256, BarkMode::Gcm);
        let plaintext = r#"{"body":"test"}"#;

        let ciphertext = encrypt(&enc, plaintext).expect("GCM must encrypt");
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &ciphertext)
            .expect("output must be base64");

        assert_eq!(decoded.len(), plaintext.len() + 16, "GCM tag is appended");

        let recovered = aes_gcm::Aes256Gcm::new(GenericArray::from_slice(key.as_bytes()))
            .decrypt(GenericArray::from_slice(iv.as_bytes()), decoded.as_slice())
            .expect("decryption must round-trip");
        assert_eq!(String::from_utf8(recovered).expect("utf8"), plaintext);
    }

    #[test]
    fn gcm_rejects_wrong_key_and_iv_lengths() {
        let short_key = encryption(
            "0123456789abcdef0123456789abc",
            Some("0123456789ab"),
            BarkAlgorithm::Aes256,
            BarkMode::Gcm,
        );
        assert_eq!(
            encrypt(&short_key, "x").unwrap_err(),
            "AES256 密钥长度必须是 32 个字符"
        );

        let short_iv = encryption(
            "0123456789abcdef0123456789abcdef",
            Some("0123456789a"),
            BarkAlgorithm::Aes256,
            BarkMode::Gcm,
        );
        assert_eq!(
            encrypt(&short_iv, "x").unwrap_err(),
            "GCM 模式必须提供 12 个字符的 IV"
        );
    }

    #[test]
    fn cbc_requires_a_16_character_iv() {
        let enc = encryption(
            "1234567890123456",
            None,
            BarkAlgorithm::Aes128,
            BarkMode::Cbc,
        );
        assert_eq!(
            encrypt(&enc, "x").unwrap_err(),
            "CBC 模式必须提供 16 个字符的 IV"
        );
    }

    #[test]
    fn ecb_pads_to_a_block_multiple_and_rejects_an_iv() {
        let enc = encryption(
            "1234567890123456",
            None,
            BarkAlgorithm::Aes128,
            BarkMode::Ecb,
        );
        let ciphertext = encrypt(&enc, "short").expect("ECB must encrypt");
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &ciphertext)
            .expect("output must be base64");
        assert_eq!(decoded.len() % 16, 0);
        assert_eq!(decoded.len(), 16, "5 bytes plus PKCS7 padding fills one block");

        let with_iv = encryption(
            "1234567890123456",
            Some("1234567890123456"),
            BarkAlgorithm::Aes128,
            BarkMode::Ecb,
        );
        assert_eq!(encrypt(&with_iv, "x").unwrap_err(), "ECB 模式不需要 IV");
    }

    #[test]
    fn bark_config_validation_rejects_bad_server_and_missing_device_key() {
        let bad_server = BarkChannelConfig {
            server_url: "api.day.app".to_string(),
            device_key: "key".to_string(),
            encryption: None,
        };
        assert_eq!(
            validate_bark_config(&bad_server).unwrap_err(),
            "服务地址必须以 http:// 或 https:// 开头"
        );

        let no_device = BarkChannelConfig {
            server_url: "https://api.day.app/".to_string(),
            device_key: "  ".to_string(),
            encryption: None,
        };
        assert_eq!(
            validate_bark_config(&no_device).unwrap_err(),
            "设备 Key 不能为空"
        );
    }

    /// A self-hosted instance may live under a path prefix; only the trailing
    /// slash is dropped, so the device key never doubles up.
    #[test]
    fn self_hosted_server_urls_keep_their_path_prefix() {
        let self_hosted = BarkChannelConfig {
            server_url: " https://bark.example.com/bark/ ".to_string(),
            device_key: "device-key".to_string(),
            encryption: None,
        };
        assert!(validate_bark_config(&self_hosted).is_ok());

        let root = normalize_server_url(&self_hosted.server_url);
        assert_eq!(root, "https://bark.example.com/bark");
        assert_eq!(
            format!("{}/{}", root, self_hosted.device_key.trim()),
            "https://bark.example.com/bark/device-key"
        );

        assert_eq!(
            validate_server_url("bark.example.com").unwrap_err(),
            "服务地址必须以 http:// 或 https:// 开头"
        );
        assert!(validate_server_url("http://127.0.0.1:8080").is_ok());
    }

    #[test]
    fn encrypted_payload_serializes_the_same_parameter_table() {
        let params = vec![
            ("title".to_string(), "t".to_string()),
            ("body".to_string(), "b".to_string()),
        ];
        let json = serde_json::to_string(&payload_json(&params)).expect("serialize");
        assert_eq!(json, r#"{"body":"b","title":"t"}"#);
    }
}
