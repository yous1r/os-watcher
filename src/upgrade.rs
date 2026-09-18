use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{error, warn};

use crate::config::{PackageKind, UpgradeConfig};

#[derive(Debug, Clone, Deserialize)]
pub struct GithubRelease {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubAsset {
    pub name: String,
    pub browser_download_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpgradePhase {
    Idle,
    Checking,
    Downloading,
    BackingUp,
    Installing,
    Restarting,
    Succeeded,
    Failed,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeStatus {
    pub running: bool,
    pub phase: UpgradePhase,
    pub message: String,
    pub package: Option<PackageKind>,
    pub target_version: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl Default for UpgradeStatus {
    fn default() -> Self {
        Self {
            running: false,
            phase: UpgradePhase::Idle,
            message: "idle".to_string(),
            package: None,
            target_version: None,
            started_at: None,
            finished_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    pub current: String,
    pub latest: Option<String>,
    pub update_available: bool,
    pub checked_at: Option<DateTime<Utc>>,
    pub platform: String,
    pub package: PackageKind,
    pub upgrade: UpgradeStatus,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpgradeRequest {
    pub package: Option<PackageKind>,
    pub proxy: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpgradeHelperRequest {
    pub service_name: String,
    pub current_exe: PathBuf,
    pub install_dir: PathBuf,
    pub backup_dir: PathBuf,
    pub status_file: PathBuf,
    pub target_version: String,
    pub package: PackageKind,
}

#[derive(Debug, Default)]
struct VersionState {
    latest: Option<String>,
    checked_at: Option<DateTime<Utc>>,
    upgrade: UpgradeStatus,
}

#[derive(Clone)]
pub struct UpgradeManager {
    config: UpgradeConfig,
    current_version: String,
    client: reqwest::Client,
    state: Arc<RwLock<VersionState>>,
    status_file: Option<PathBuf>,
}

impl UpgradeManager {
    pub fn new(config: UpgradeConfig, current_version: impl Into<String>) -> Result<Self> {
        let client = crate::http::build_client(
            config.proxy.as_deref(),
            Duration::from_secs(120),
            &crate::http::user_agent(),
        )?;
        let current_version = current_version.into();
        let status_file = default_upgrade_status_file();
        let persisted_status = status_file
            .as_deref()
            .and_then(read_persisted_status)
            .map(|status| recover_persisted_status(status, &current_version));
        if let (Some(path), Some(status)) = (status_file.as_ref(), persisted_status.as_ref()) {
            if let Err(err) = write_persisted_status(path, status) {
                warn!(
                    "Failed to persist recovered upgrade status {}: {err}",
                    path.display()
                );
            }
        }
        let initial_state = VersionState {
            latest: persisted_status
                .as_ref()
                .and_then(|status| status.target_version.clone()),
            checked_at: persisted_status
                .as_ref()
                .and_then(|status| status.finished_at.or(status.started_at)),
            upgrade: persisted_status.unwrap_or_default(),
        };

        Ok(Self {
            config,
            current_version,
            client,
            state: Arc::new(RwLock::new(initial_state)),
            status_file,
        })
    }

    pub async fn version_info(&self) -> VersionInfo {
        let state = self.state.read().await;
        self.version_info_from_state(&state)
    }

    pub async fn upgrade_status(&self) -> UpgradeStatus {
        let state = self.state.read().await;
        state.upgrade.clone()
    }

    pub async fn refresh_latest(&self) -> Result<VersionInfo> {
        if !self.config.enabled {
            return Ok(self.version_info().await);
        }

        let release = self.fetch_latest_release(&self.client).await?;
        let mut state = self.state.write().await;
        state.latest = Some(release.tag_name);
        state.checked_at = Some(Utc::now());
        Ok(self.version_info_from_state(&state))
    }

    pub fn spawn_version_check_loop(&self) {
        if !self.config.enabled {
            return;
        }

        let manager = self.clone();
        tokio::spawn(async move {
            let interval = Duration::from_secs(manager.config.check_interval_secs.max(60));
            loop {
                if let Err(err) = manager.refresh_latest().await {
                    warn!("Failed to check latest os-watcher release: {err:#}");
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    pub async fn trigger_upgrade(&self, request: UpgradeRequest) -> Result<UpgradeStatus> {
        if !self.config.enabled {
            return Err(anyhow!("self-upgrade is disabled"));
        }

        let package = request.package.unwrap_or(self.config.package);
        let proxy = request.proxy.filter(|v| !v.trim().is_empty());

        let status = {
            let mut state = self.state.write().await;
            if state.upgrade.running {
                return Err(anyhow!("upgrade is already running"));
            }
            let status = UpgradeStatus {
                running: true,
                phase: UpgradePhase::Checking,
                message: "checking latest release".to_string(),
                package: Some(package),
                target_version: None,
                started_at: Some(Utc::now()),
                finished_at: None,
            };
            state.upgrade = status.clone();
            status
        };

        let manager = self.clone();
        tokio::spawn(async move {
            if let Err(err) = manager.run_upgrade(package, proxy).await {
                error!("Upgrade failed: {err:#}");
                manager
                    .finish_failed(format!("upgrade failed: {err:#}"))
                    .await;
            }
        });

        Ok(status)
    }

    async fn fetch_latest_release(&self, client: &reqwest::Client) -> Result<GithubRelease> {
        let url = format!(
            "https://api.github.com/repos/{}/releases/latest",
            self.config.github_repo
        );
        let resp = client
            .get(url)
            .send()
            .await
            .context("request GitHub latest release")?
            .error_for_status()
            .context("GitHub latest release returned an error")?;
        resp.json::<GithubRelease>()
            .await
            .context("parse GitHub latest release")
    }

    async fn run_upgrade(&self, package: PackageKind, proxy: Option<String>) -> Result<()> {
        self.set_status(
            UpgradePhase::Checking,
            true,
            "checking latest release",
            Some(package),
            None,
        )
        .await;

        let client = crate::http::build_client(
            proxy.as_deref().or(self.config.proxy.as_deref()),
            Duration::from_secs(120),
            &crate::http::user_agent(),
        )?;
        let release = self.fetch_latest_release(&client).await?;
        {
            let mut state = self.state.write().await;
            state.latest = Some(release.tag_name.clone());
            state.checked_at = Some(Utc::now());
            state.upgrade.target_version = Some(release.tag_name.clone());
        }

        let platform = current_platform();
        if platform == "unsupported" {
            return Err(anyhow!("unsupported upgrade platform"));
        }

        // The upgrade replaces the binary and restarts the service, so it can
        // only ever succeed when a service is registered. Check before
        // downloading so a hand-started process reports the real problem
        // instead of downloading, staging, and rolling back.
        ensure_service_registered(&self.config.service_name).await?;

        let asset = select_asset(&release.assets, platform, package)
            .ok_or_else(|| anyhow!("release asset not found for {platform}/{package}"))?
            .clone();

        let temp_root = std::env::temp_dir().join(format!(
            "os-watcher-upgrade-{}",
            Utc::now().timestamp_millis()
        ));
        let extract_dir = temp_root.join("extract");
        fs::create_dir_all(&extract_dir)
            .with_context(|| format!("create upgrade temp directory {}", extract_dir.display()))?;
        let archive_path = temp_root.join(&asset.name);

        self.set_status(
            UpgradePhase::Downloading,
            true,
            format!("downloading {}", asset.name),
            Some(package),
            Some(release.tag_name.clone()),
        )
        .await;
        download_with_retries(&client, &asset.browser_download_url, &archive_path).await?;

        let current_exe = std::env::current_exe().context("resolve current executable")?;
        let install_dir = current_exe
            .parent()
            .ok_or_else(|| anyhow!("current executable has no parent directory"))?
            .to_path_buf();
        let status_file = upgrade_status_file(&install_dir);
        let backup_dir = install_dir
            .join("backups")
            .join(format!("upgrade-{}", Utc::now().format("%Y%m%d%H%M%S")));

        self.set_status(
            UpgradePhase::BackingUp,
            true,
            "backing up current executable and config",
            Some(package),
            Some(release.tag_name.clone()),
        )
        .await;
        backup_current_install(&current_exe, &install_dir, &backup_dir)?;

        self.set_status(
            UpgradePhase::Installing,
            true,
            "extracting and installing package",
            Some(package),
            Some(release.tag_name.clone()),
        )
        .await;

        if let Err(err) = (async {
            extract_archive(&archive_path, &extract_dir).await?;
            let payload_root = find_payload_root(&extract_dir)?;
            install_payload(&payload_root, &install_dir, &current_exe)?;
            Result::<()>::Ok(())
        })
        .await
        {
            rollback_backup(&backup_dir, &install_dir, &current_exe)?;
            self.set_status(
                UpgradePhase::RolledBack,
                false,
                format!("install failed and backup was restored: {err:#}"),
                Some(package),
                Some(release.tag_name.clone()),
            )
            .await;
            return Err(err);
        }

        self.set_status(
            UpgradePhase::Restarting,
            true,
            format!("scheduling {} restart", self.config.service_name),
            Some(package),
            Some(release.tag_name.clone()),
        )
        .await;
        self.persist_current_status_to(&status_file).await?;

        let helper = UpgradeHelperRequest {
            service_name: self.config.service_name.clone(),
            current_exe: current_exe.clone(),
            install_dir: install_dir.clone(),
            backup_dir: backup_dir.clone(),
            status_file: status_file.clone(),
            target_version: release.tag_name.clone(),
            package,
        };

        if let Err(err) = schedule_service_restart(&helper).await {
            let message = match rollback_backup(&backup_dir, &install_dir, &current_exe) {
                Ok(()) => {
                    let message =
                        format!("restart scheduling failed and backup was restored: {err:#}");
                    self.set_status(
                        UpgradePhase::RolledBack,
                        false,
                        message.clone(),
                        Some(package),
                        Some(release.tag_name.clone()),
                    )
                    .await;
                    self.persist_current_status_to(&status_file).await?;
                    return Err(err);
                }
                Err(rollback_err) => {
                    format!("restart scheduling failed: {err:#}; rollback failed: {rollback_err:#}")
                }
            };
            self.set_status(
                UpgradePhase::Failed,
                false,
                message.clone(),
                Some(package),
                Some(release.tag_name.clone()),
            )
            .await;
            self.persist_current_status_to(&status_file).await?;
            return Err(anyhow!(message));
        }

        self.set_status(
            UpgradePhase::Restarting,
            true,
            format!(
                "upgrade to {} installed; service restart scheduled",
                release.tag_name
            ),
            Some(package),
            Some(release.tag_name.clone()),
        )
        .await;
        self.persist_current_status_to(&status_file).await?;

        if let Some(status) =
            wait_for_persisted_terminal_status(&status_file, Duration::from_secs(90)).await
        {
            let phase = status.phase.clone();
            let message = status.message.clone();
            self.replace_upgrade_status(status).await;
            if matches!(phase, UpgradePhase::Failed | UpgradePhase::RolledBack) {
                return Err(anyhow!(message));
            }
        } else {
            // The restart helper owns the outcome once it runs; if it dies without
            // writing a terminal status the upgrade silently looks successful, so
            // surface the stall instead of leaving the last `restarting` state.
            let message = format!(
                "restart helper did not report a terminal status within 90s; inspect {}",
                status_file.display()
            );
            self.set_status(
                UpgradePhase::Failed,
                false,
                message.clone(),
                Some(package),
                Some(release.tag_name.clone()),
            )
            .await;
            self.persist_current_status_to(&status_file).await?;
            return Err(anyhow!(message));
        }

        if let Err(err) = fs::remove_dir_all(&temp_root) {
            warn!(
                "Failed to remove upgrade temp directory {}: {err}",
                temp_root.display()
            );
        }

        Ok(())
    }

    async fn replace_upgrade_status(&self, status: UpgradeStatus) {
        let mut state = self.state.write().await;
        state.upgrade = status;
    }

    async fn set_status(
        &self,
        phase: UpgradePhase,
        running: bool,
        message: impl Into<String>,
        package: Option<PackageKind>,
        target_version: Option<String>,
    ) {
        let mut state = self.state.write().await;
        let started_at = state.upgrade.started_at.or_else(|| Some(Utc::now()));
        state.upgrade = UpgradeStatus {
            running,
            phase,
            message: message.into(),
            package,
            target_version,
            started_at,
            finished_at: if running { None } else { Some(Utc::now()) },
        };
    }

    async fn finish_failed(&self, message: String) {
        let mut state = self.state.write().await;
        if !state.upgrade.running {
            return;
        }
        state.upgrade.running = false;
        state.upgrade.phase = UpgradePhase::Failed;
        state.upgrade.message = message;
        state.upgrade.finished_at = Some(Utc::now());
        if let Some(status_file) = &self.status_file {
            if let Err(err) = write_persisted_status(status_file, &state.upgrade) {
                warn!(
                    "Failed to write upgrade status file {}: {err}",
                    status_file.display()
                );
            }
        }
    }

    fn version_info_from_state(&self, state: &VersionState) -> VersionInfo {
        let latest = state.latest.clone();
        VersionInfo {
            current: self.current_version.clone(),
            update_available: latest
                .as_deref()
                .is_some_and(|latest| is_newer_version(latest, &self.current_version)),
            latest,
            checked_at: state.checked_at,
            platform: current_platform().to_string(),
            package: self.config.package,
            upgrade: state.upgrade.clone(),
        }
    }

    async fn persist_current_status_to(&self, status_file: &Path) -> Result<()> {
        let status = self.upgrade_status().await;
        write_persisted_status(status_file, &status)
    }
}

fn default_upgrade_status_file() -> Option<PathBuf> {
    let current_exe = std::env::current_exe().ok()?;
    let install_dir = current_exe.parent()?;
    Some(upgrade_status_file(install_dir))
}

fn upgrade_status_file(install_dir: &Path) -> PathBuf {
    install_dir.join(".os-watcher-upgrade-status.json")
}

fn read_persisted_status(path: &Path) -> Option<UpgradeStatus> {
    let content = fs::read_to_string(path).ok()?;
    // PowerShell 5.1's `Set-Content -Encoding UTF8` emits a UTF-8 BOM, so status
    // files written by older builds start with one; strip it before parsing.
    serde_json::from_str(content.trim_start_matches('\u{feff}')).ok()
}

fn write_persisted_status(path: &Path, status: &UpgradeStatus) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create status directory {}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(status).context("serialize upgrade status")?;
    fs::write(path, content).with_context(|| format!("write upgrade status {}", path.display()))
}

fn recover_persisted_status(mut status: UpgradeStatus, current_version: &str) -> UpgradeStatus {
    if status.phase == UpgradePhase::Restarting {
        if status
            .target_version
            .as_deref()
            .is_some_and(|target| normalize_version(target) == normalize_version(current_version))
        {
            status.running = false;
            status.phase = UpgradePhase::Succeeded;
            status.message = format!("upgrade to {current_version} confirmed after restart");
            status.finished_at = Some(Utc::now());
        }
    }
    status
}

async fn wait_for_persisted_terminal_status(
    path: &Path,
    timeout: Duration,
) -> Option<UpgradeStatus> {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        if let Some(status) = read_persisted_status(path) {
            if !status.running {
                return Some(status);
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    None
}

pub fn current_platform() -> &'static str {
    if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        "linux-x86_64-musl"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-aarch64"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "windows-x86_64"
    } else {
        "unsupported"
    }
}

pub fn asset_name(platform: &str, package: PackageKind) -> String {
    let ext = if platform.starts_with("windows-") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("os-watcher-{platform}-{package}.{ext}")
}

pub fn select_asset<'a>(
    assets: &'a [GithubAsset],
    platform: &str,
    package: PackageKind,
) -> Option<&'a GithubAsset> {
    let expected = asset_name(platform, package);
    assets.iter().find(|asset| asset.name == expected)
}

pub fn is_newer_version(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => normalize_version(latest) != normalize_version(current),
    }
}

fn normalize_version(version: &str) -> &str {
    version
        .trim()
        .trim_start_matches('v')
        .trim_start_matches('V')
}

fn parse_version(version: &str) -> Option<[u64; 3]> {
    let normalized = normalize_version(version)
        .split(['-', '+'])
        .next()
        .unwrap_or_default();
    let mut parts = [0_u64; 3];
    for (idx, segment) in normalized.split('.').take(3).enumerate() {
        parts[idx] = segment.parse().ok()?;
    }
    Some(parts)
}

async fn download_with_retries(client: &reqwest::Client, url: &str, target: &Path) -> Result<()> {
    let mut last_error = None;
    for attempt in 1..=3 {
        match download_once(client, url, target).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                warn!("Download attempt {attempt}/3 failed: {err:#}");
                last_error = Some(err);
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_secs(attempt * 2)).await;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("download failed")))
}

async fn download_once(client: &reqwest::Client, url: &str, target: &Path) -> Result<()> {
    let bytes = client
        .get(url)
        .send()
        .await
        .context("download release asset")?
        .error_for_status()
        .context("release asset returned an error")?
        .bytes()
        .await
        .context("read release asset body")?;
    tokio::fs::write(target, &bytes)
        .await
        .with_context(|| format!("write release asset {}", target.display()))?;
    Ok(())
}

async fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    if archive.extension() == Some(OsStr::new("zip")) {
        extract_zip_archive(archive, dest).await
    } else {
        extract_tar_gz_archive(archive, dest).await
    }
}

#[cfg(target_os = "windows")]
async fn extract_zip_archive(archive: &Path, dest: &Path) -> Result<()> {
    let command = format!(
        "Expand-Archive -LiteralPath {} -DestinationPath {} -Force",
        quote_powershell(archive),
        quote_powershell(dest)
    );
    let status = tokio::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &command,
        ])
        .status()
        .await
        .context("run Expand-Archive")?;
    if !status.success() {
        return Err(anyhow!("Expand-Archive failed with status {status}"));
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
async fn extract_zip_archive(_archive: &Path, _dest: &Path) -> Result<()> {
    Err(anyhow!(
        "zip extraction is only supported on Windows packages"
    ))
}

#[cfg(not(target_os = "windows"))]
async fn extract_tar_gz_archive(archive: &Path, dest: &Path) -> Result<()> {
    let status = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .status()
        .await
        .context("run tar")?;
    if !status.success() {
        return Err(anyhow!("tar failed with status {status}"));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
async fn extract_tar_gz_archive(_archive: &Path, _dest: &Path) -> Result<()> {
    Err(anyhow!(
        "tar.gz extraction is only supported on Linux packages"
    ))
}

fn find_payload_root(extract_dir: &Path) -> Result<PathBuf> {
    let mut dirs = fs::read_dir(extract_dir)
        .with_context(|| format!("read extracted directory {}", extract_dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    dirs.sort();

    dirs.into_iter()
        .next()
        .ok_or_else(|| anyhow!("release archive did not contain a package directory"))
}

fn backup_current_install(current_exe: &Path, install_dir: &Path, backup_dir: &Path) -> Result<()> {
    fs::create_dir_all(backup_dir)
        .with_context(|| format!("create backup directory {}", backup_dir.display()))?;
    copy_file_if_exists(current_exe, &backup_dir.join(file_name(current_exe)?))?;

    for name in ["config.toml", "config.example.toml"] {
        copy_file_if_exists(&install_dir.join(name), &backup_dir.join(name))?;
    }

    let web_dist = install_dir.join("web-dist");
    if web_dist.is_dir() {
        copy_dir_recursive(&web_dist, &backup_dir.join("web-dist"))?;
    }

    Ok(())
}

fn install_payload(payload_root: &Path, install_dir: &Path, current_exe: &Path) -> Result<()> {
    let binary_name = canonical_binary_name();
    let mut staged_binary = false;
    for entry in fs::read_dir(payload_root)
        .with_context(|| format!("read payload root {}", payload_root.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let src = entry.path();
        let dst = install_dir.join(&name);

        // Match the archive's own binary name rather than the running file name:
        // the running executable may have been renamed, and comparing against it
        // would skip staging and then report a successful upgrade that never
        // replaced the binary.
        if name.as_os_str() == OsStr::new(binary_name) {
            stage_binary_replacement(&src, current_exe)?;
            staged_binary = true;
        } else if src.is_dir() {
            if dst.exists() {
                fs::remove_dir_all(&dst)
                    .with_context(|| format!("replace directory {}", dst.display()))?;
            }
            copy_dir_recursive(&src, &dst)?;
        } else if src.is_file() {
            fs::copy(&src, &dst)
                .with_context(|| format!("copy {} to {}", src.display(), dst.display()))?;
            if name.as_os_str() == OsStr::new(DEPLOY_SCRIPT_NAME) {
                set_executable(&dst)?;
            }
        }
    }

    if !staged_binary {
        return Err(anyhow!(
            "release archive did not contain the {binary_name} binary"
        ));
    }

    Ok(())
}

fn stage_binary_replacement(src: &Path, current_exe: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let staged = windows_staged_exe_path(current_exe);
        fs::copy(src, &staged)
            .with_context(|| format!("stage Windows binary {}", staged.display()))?;
        return Ok(());
    }

    #[cfg(not(target_os = "windows"))]
    {
        let staged = current_exe.with_extension("new");
        fs::copy(src, &staged)
            .with_context(|| format!("stage binary replacement {}", staged.display()))?;
        set_executable(&staged)?;
        fs::rename(&staged, current_exe)
            .with_context(|| format!("replace executable {}", current_exe.display()))?;
        Ok(())
    }
}

fn rollback_backup(backup_dir: &Path, install_dir: &Path, current_exe: &Path) -> Result<()> {
    let exe_backup = backup_dir.join(file_name(current_exe)?);
    restore_binary_backup(&exe_backup, current_exe)?;
    for name in ["config.toml", "config.example.toml"] {
        restore_file_or_remove(&backup_dir.join(name), &install_dir.join(name))?;
    }
    let web_dist_backup = backup_dir.join("web-dist");
    let web_dist = install_dir.join("web-dist");
    if web_dist.exists() {
        fs::remove_dir_all(&web_dist).with_context(|| format!("remove {}", web_dist.display()))?;
    }
    if web_dist_backup.is_dir() {
        copy_dir_recursive(&web_dist_backup, &web_dist)?;
    }
    Ok(())
}

fn restore_binary_backup(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_file() {
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        let staged = dst.with_extension("rollback");
        fs::copy(src, &staged)
            .with_context(|| format!("stage rollback binary {}", staged.display()))?;
        set_executable(&staged)?;
        fs::rename(&staged, dst).with_context(|| format!("restore binary {}", dst.display()))?;
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    {
        copy_file_if_exists(src, dst)
    }
}

fn copy_file_if_exists(src: &Path, dst: &Path) -> Result<()> {
    if src.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create directory {}", parent.display()))?;
        }
        fs::copy(src, dst)
            .with_context(|| format!("copy {} to {}", src.display(), dst.display()))?;
    }
    Ok(())
}

fn restore_file_or_remove(src: &Path, dst: &Path) -> Result<()> {
    if src.is_file() {
        copy_file_if_exists(src, dst)
    } else {
        if dst.exists() {
            fs::remove_file(dst).with_context(|| format!("remove {}", dst.display()))?;
        }
        Ok(())
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("create directory {}", dst.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("read directory {}", src.display()))? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if src_path.is_file() {
            fs::copy(&src_path, &dst_path).with_context(|| {
                format!("copy {} to {}", src_path.display(), dst_path.display())
            })?;
        }
    }
    Ok(())
}

fn file_name(path: &Path) -> Result<std::ffi::OsString> {
    path.file_name()
        .map(|name| name.to_os_string())
        .ok_or_else(|| anyhow!("path has no file name: {}", path.display()))
}

/// Name of the binary inside every release archive.
///
/// The running executable is normally this name, but users do rename it (for
/// example to keep a hand-downloaded build beside the installed one). Matching
/// the payload against this name keeps upgrades working in that case instead of
/// silently copying the new binary into place without staging it.
fn canonical_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "os-watcher.exe"
    } else {
        "os-watcher"
    }
}

/// Release archives may carry `deploy.sh` without its executable bit: `tar`
/// derives the mode from the packing machine's umask, and the git index mode
/// (`100644`) wins for `git archive`. Restore it on install so a node that
/// upgraded itself can still be re-deployed by running the script directly.
const DEPLOY_SCRIPT_NAME: &str = "deploy.sh";

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(all(not(unix), not(target_os = "windows")))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

pub async fn run_upgrade_helper(request: UpgradeHelperRequest) -> Result<()> {
    tokio::time::sleep(Duration::from_secs(1)).await;

    match restart_service_and_wait(&request.service_name).await {
        Ok(()) => {
            let status = helper_status(
                UpgradePhase::Succeeded,
                false,
                format!("service {} restarted successfully", request.service_name),
                &request,
            );
            write_persisted_status(&request.status_file, &status)?;
            Ok(())
        }
        Err(restart_err) => {
            let rollback_result = rollback_backup(
                &request.backup_dir,
                &request.install_dir,
                &request.current_exe,
            );
            match rollback_result {
                Ok(()) => {
                    let restart_old = restart_service_and_wait(&request.service_name).await;
                    let (phase, message) = match restart_old {
                        Ok(()) => (
                            UpgradePhase::RolledBack,
                            format!("restart failed and backup was restored: {restart_err:#}"),
                        ),
                        Err(old_err) => (
                            UpgradePhase::Failed,
                            format!(
                                "restart failed; backup restored but service restart failed: {restart_err:#}; {old_err:#}"
                            ),
                        ),
                    };
                    let status = helper_status(phase, false, message, &request);
                    write_persisted_status(&request.status_file, &status)?;
                    Ok(())
                }
                Err(rollback_err) => {
                    let message = format!(
                        "restart failed: {restart_err:#}; rollback failed: {rollback_err:#}"
                    );
                    let status =
                        helper_status(UpgradePhase::Failed, false, message.clone(), &request);
                    write_persisted_status(&request.status_file, &status)?;
                    Err(anyhow!(message))
                }
            }
        }
    }
}

fn helper_status(
    phase: UpgradePhase,
    running: bool,
    message: impl Into<String>,
    request: &UpgradeHelperRequest,
) -> UpgradeStatus {
    UpgradeStatus {
        running,
        phase,
        message: message.into(),
        package: Some(request.package),
        target_version: Some(request.target_version.clone()),
        started_at: None,
        finished_at: Some(Utc::now()),
    }
}

#[cfg(target_os = "linux")]
async fn schedule_service_restart(request: &UpgradeHelperRequest) -> Result<()> {
    let service = shell_safe_service_name(&request.service_name)?;
    let unit = format!("os-watcher-upgrade-{}", Utc::now().timestamp_millis());
    let status = tokio::process::Command::new("systemd-run")
        .args(["--unit", &unit, "--collect", "--quiet"])
        .arg(&request.current_exe)
        .args(helper_args(request, service))
        .status()
        .await
        .context("schedule systemd restart helper with systemd-run")?;
    if !status.success() {
        return Err(anyhow!(
            "failed to schedule systemd restart helper: {status}"
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
async fn schedule_service_restart(request: &UpgradeHelperRequest) -> Result<()> {
    let script = windows_restart_script(request)?;
    let launcher = windows_restart_launcher(&script);
    let status = tokio::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-WindowStyle",
            "Hidden",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &launcher,
        ])
        .status()
        .await
        .context("schedule Windows service restart")?;
    if !status.success() {
        return Err(anyhow!(
            "failed to schedule Windows service restart: {status}"
        ));
    }
    Ok(())
}

/// Build the PowerShell script that stops the service, swaps in the staged
/// binary, restarts it, and records the outcome in the status file.
#[cfg(target_os = "windows")]
fn windows_restart_script(request: &UpgradeHelperRequest) -> Result<String> {
    let staged = windows_staged_exe_path(&request.current_exe);
    let backup_exe = request.backup_dir.join(file_name(&request.current_exe)?);
    let config = request.install_dir.join("config.toml");
    let config_example = request.install_dir.join("config.example.toml");
    let backup_config = request.backup_dir.join("config.toml");
    let backup_config_example = request.backup_dir.join("config.example.toml");
    let web_dist = request.install_dir.join("web-dist");
    let backup_web_dist = request.backup_dir.join("web-dist");
    Ok(format!(
        r#"
$ErrorActionPreference = 'Stop'
function Write-UpgradeStatus([string]$phase, [bool]$running, [string]$message) {{
  $status = [ordered]@{{
    running = $running
    phase = $phase
    message = $message
    package = {package}
    target_version = {target_version}
    started_at = $null
    finished_at = (Get-Date).ToUniversalTime().ToString('o')
  }}
  $json = $status | ConvertTo-Json -Compress
  # PowerShell 5.1's UTF8 file encoding emits a BOM, which the parent process
  # cannot parse as JSON. Write UTF-8 without a BOM explicitly.
  [System.IO.File]::WriteAllText({status_file}, $json, (New-Object System.Text.UTF8Encoding $false))
}}

function Wait-ServiceState([string]$name, [string]$state, [int]$timeoutSeconds) {{
  $deadline = (Get-Date).AddSeconds($timeoutSeconds)
  do {{
    $query = sc.exe query $name | Out-String
    if ($LASTEXITCODE -eq 0 -and $query -match $state) {{
      return
    }}
    Start-Sleep -Seconds 1
  }} while ((Get-Date) -lt $deadline)
  throw "service did not become $state"
}}

try {{
  Start-Sleep -Seconds 1
  sc.exe query {svc} | Out-Null
  if ($LASTEXITCODE -ne 0) {{
    throw "service {svc} is not installed; run deploy.sh --package {package_kind} on this host to register it"
  }}
  sc.exe stop {svc} | Out-Null
  if ($LASTEXITCODE -ne 0) {{
    throw 'service stop command failed'
  }}
  Wait-ServiceState {svc} 'STOPPED' 30
  Move-Item -LiteralPath {staged} -Destination {exe} -Force
  sc.exe start {svc} | Out-Null
  if ($LASTEXITCODE -ne 0) {{
    throw 'service start command failed'
  }}
  Wait-ServiceState {svc} 'RUNNING' 30
  Write-UpgradeStatus 'succeeded' $false 'service restarted successfully'
}} catch {{
  $restartError = $_.Exception.Message
  try {{
    if (Test-Path -LiteralPath {backup_exe}) {{
      Copy-Item -LiteralPath {backup_exe} -Destination {exe} -Force
    }}
    if (Test-Path -LiteralPath {backup_config}) {{
      Copy-Item -LiteralPath {backup_config} -Destination {config} -Force
    }} elseif (Test-Path -LiteralPath {config}) {{
      Remove-Item -LiteralPath {config} -Force
    }}
    if (Test-Path -LiteralPath {backup_config_example}) {{
      Copy-Item -LiteralPath {backup_config_example} -Destination {config_example} -Force
    }} elseif (Test-Path -LiteralPath {config_example}) {{
      Remove-Item -LiteralPath {config_example} -Force
    }}
    if (Test-Path -LiteralPath {backup_web_dist}) {{
      Remove-Item -LiteralPath {web_dist} -Recurse -Force -ErrorAction SilentlyContinue
      Copy-Item -LiteralPath {backup_web_dist} -Destination {web_dist} -Recurse -Force
    }} elseif (Test-Path -LiteralPath {web_dist}) {{
      Remove-Item -LiteralPath {web_dist} -Recurse -Force
    }}
    sc.exe start {svc} | Out-Null
    if ($LASTEXITCODE -ne 0) {{
      throw 'rollback service start command failed'
    }}
    Wait-ServiceState {svc} 'RUNNING' 30
    Write-UpgradeStatus 'rolled_back' $false "restart failed and backup was restored: $restartError"
  }} catch {{
    Write-UpgradeStatus 'failed' $false "restart failed: $restartError; rollback failed: $($_.Exception.Message)"
  }}
}}
"#,
        svc = quote_powershell_arg(&request.service_name),
        package_kind = request.package.as_str(),
        staged = quote_powershell(&staged),
        exe = quote_powershell(&request.current_exe),
        package = quote_powershell_arg(request.package.as_str()),
        target_version = quote_powershell_arg(&request.target_version),
        status_file = quote_powershell(&request.status_file),
        backup_exe = quote_powershell(&backup_exe),
        backup_config = quote_powershell(&backup_config),
        backup_config_example = quote_powershell(&backup_config_example),
        backup_web_dist = quote_powershell(&backup_web_dist),
        config = quote_powershell(&config),
        config_example = quote_powershell(&config_example),
        web_dist = quote_powershell(&web_dist),
    ))
}

/// Wrap the restart script in the command line that launches it.
///
/// The script travels as a single base64 (UTF-16LE) token. Handing it to
/// `Start-Process -ArgumentList` instead makes PowerShell strip every double
/// quote while re-parsing the child command line, which truncated status
/// messages to their first word and broke the interpolated paths.
///
/// The launch itself goes through WMI rather than `Start-Process`: the service
/// manager puts the service process in a job object, so a child process is
/// killed the moment the service stops -- exactly when the helper still has to
/// replace the binary and start the service again. WMI spawns the helper from
/// WmiPrvSE, outside that job, so it survives the stop it performs.
#[cfg(target_os = "windows")]
fn windows_restart_launcher(script: &str) -> String {
    let command_line = format!(
        "powershell.exe -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -EncodedCommand {}",
        encode_powershell_command(script)
    );
    format!(
        "Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{CommandLine={}}} | Out-Null",
        quote_powershell_arg(&command_line)
    )
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
async fn schedule_service_restart(_request: &UpgradeHelperRequest) -> Result<()> {
    Err(anyhow!(
        "service restart is only supported on Linux and Windows"
    ))
}

#[cfg(target_os = "linux")]
fn helper_args(request: &UpgradeHelperRequest, service_name: &str) -> Vec<String> {
    vec![
        "upgrade-helper".to_string(),
        "--service-name".to_string(),
        service_name.to_string(),
        "--current-exe".to_string(),
        request.current_exe.to_string_lossy().into_owned(),
        "--install-dir".to_string(),
        request.install_dir.to_string_lossy().into_owned(),
        "--backup-dir".to_string(),
        request.backup_dir.to_string_lossy().into_owned(),
        "--status-file".to_string(),
        request.status_file.to_string_lossy().into_owned(),
        "--target-version".to_string(),
        request.target_version.clone(),
        "--package".to_string(),
        request.package.as_str().to_string(),
    ]
}

async fn restart_service_and_wait(service_name: &str) -> Result<()> {
    restart_service(service_name).await?;
    wait_service_active(service_name, Duration::from_secs(30)).await
}

/// Fail early when the configured service is not registered.
///
/// Windows nodes started by hand (no `deploy.sh` run, or `nssm` missing) have no
/// service to stop, so every `sc.exe` call fails and the upgrade rolls back with
/// a misleading "restart failed" message. Linux already fails on the first
/// `systemctl restart`; this gives Windows the same early, explicit signal.
#[cfg(target_os = "windows")]
async fn ensure_service_registered(service_name: &str) -> Result<()> {
    let status = tokio::process::Command::new("sc.exe")
        .arg("query")
        .arg(service_name)
        .status()
        .await
        .context("query Windows service registration")?;
    if !status.success() {
        return Err(anyhow!(
            "service {service_name} is not registered; register it with deploy.sh --package node|full before upgrading"
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
async fn ensure_service_registered(_service_name: &str) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
async fn restart_service(service_name: &str) -> Result<()> {
    let status = tokio::process::Command::new("systemctl")
        .arg("restart")
        .arg(shell_safe_service_name(service_name)?)
        .status()
        .await
        .context("restart systemd service")?;
    if !status.success() {
        return Err(anyhow!("systemctl restart failed with status {status}"));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
async fn restart_service(service_name: &str) -> Result<()> {
    let stop = tokio::process::Command::new("sc.exe")
        .arg("stop")
        .arg(service_name)
        .status()
        .await
        .context("stop Windows service")?;
    if !stop.success() {
        warn!("sc.exe stop returned {stop}; continuing with start");
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let start = tokio::process::Command::new("sc.exe")
        .arg("start")
        .arg(service_name)
        .status()
        .await
        .context("start Windows service")?;
    if !start.success() {
        return Err(anyhow!("sc.exe start failed with status {start}"));
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
async fn restart_service(_service_name: &str) -> Result<()> {
    Err(anyhow!(
        "service restart is only supported on Linux and Windows"
    ))
}

#[cfg(target_os = "linux")]
async fn wait_service_active(service_name: &str, timeout: Duration) -> Result<()> {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        let status = tokio::process::Command::new("systemctl")
            .arg("is-active")
            .arg("--quiet")
            .arg(shell_safe_service_name(service_name)?)
            .status()
            .await
            .context("check systemd service status")?;
        if status.success() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(anyhow!("service {service_name} did not become active"))
}

#[cfg(target_os = "windows")]
async fn wait_service_active(service_name: &str, timeout: Duration) -> Result<()> {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        let output = tokio::process::Command::new("sc.exe")
            .arg("query")
            .arg(service_name)
            .output()
            .await
            .context("query Windows service status")?;
        let text = String::from_utf8_lossy(&output.stdout);
        if text.contains("RUNNING") {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(anyhow!("service {service_name} did not become active"))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
async fn wait_service_active(_service_name: &str, _timeout: Duration) -> Result<()> {
    Err(anyhow!(
        "service status checks are only supported on Linux and Windows"
    ))
}

#[cfg(target_os = "windows")]
fn windows_staged_exe_path(current_exe: &Path) -> PathBuf {
    let stem = current_exe
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("os-watcher");
    current_exe.with_file_name(format!("{stem}.new.exe"))
}

#[cfg(target_os = "linux")]
fn shell_safe_service_name(service_name: &str) -> Result<&str> {
    if service_name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
    {
        Ok(service_name)
    } else {
        Err(anyhow!("invalid service name: {service_name}"))
    }
}

#[cfg(target_os = "windows")]
fn quote_powershell(path: &Path) -> String {
    quote_powershell_arg(&path.to_string_lossy())
}

#[cfg(target_os = "windows")]
fn quote_powershell_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Encode a script for `powershell -EncodedCommand`.
///
/// The switch expects base64 over UTF-16LE. Passing the script this way keeps it
/// a single token with no quotes or newlines, so the child process receives it
/// verbatim instead of being re-parsed as a command line.
#[cfg(target_os = "windows")]
fn encode_powershell_command(script: &str) -> String {
    use base64::Engine;
    let utf16: Vec<u8> = script
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(utf16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_compare_numeric_segments() {
        assert!(is_newer_version("v1.10.0", "0.9.9"));
        assert!(is_newer_version("v1.10.0", "1.9.9"));
        assert!(!is_newer_version("v1.2.0", "1.10.0"));
        assert!(!is_newer_version("v1.2.0", "1.2.0"));
    }

    #[test]
    fn release_asset_name_matches_platform_package_and_extension() {
        assert_eq!(
            asset_name("linux-x86_64", PackageKind::Node),
            "os-watcher-linux-x86_64-node.tar.gz"
        );
        assert_eq!(
            asset_name("windows-x86_64", PackageKind::Full),
            "os-watcher-windows-x86_64-full.zip"
        );
    }

    #[test]
    fn selects_exact_asset_for_platform_and_package() {
        let assets = vec![
            GithubAsset {
                name: "os-watcher-linux-x86_64-node.tar.gz".to_string(),
                browser_download_url: "https://example.invalid/node".to_string(),
            },
            GithubAsset {
                name: "os-watcher-linux-x86_64-full.tar.gz".to_string(),
                browser_download_url: "https://example.invalid/full".to_string(),
            },
        ];

        let selected = select_asset(&assets, "linux-x86_64", PackageKind::Full)
            .expect("full package asset should be selected");

        assert_eq!(
            selected.browser_download_url,
            "https://example.invalid/full"
        );
    }

    #[test]
    fn current_platform_is_one_of_release_platforms() {
        assert!(matches!(
            current_platform(),
            "linux-x86_64"
                | "linux-x86_64-musl"
                | "linux-aarch64"
                | "windows-x86_64"
                | "unsupported"
        ));
    }

    #[tokio::test]
    async fn background_failure_does_not_overwrite_completed_status() {
        let manager = UpgradeManager::new(UpgradeConfig::default(), "0.1.0")
            .expect("upgrade manager should be created");

        manager
            .set_status(
                UpgradePhase::RolledBack,
                false,
                "backup restored",
                Some(PackageKind::Node),
                Some("v0.2.0".to_string()),
            )
            .await;

        manager
            .finish_failed("late background error".to_string())
            .await;

        let status = manager.upgrade_status().await;
        assert_eq!(status.phase, UpgradePhase::RolledBack);
        assert_eq!(status.message, "backup restored");
    }

    #[test]
    fn pending_restart_for_current_version_is_confirmed_on_startup() {
        let recovered = recover_persisted_status(
            UpgradeStatus {
                running: true,
                phase: UpgradePhase::Restarting,
                message: "service restart scheduled".to_string(),
                package: Some(PackageKind::Node),
                target_version: Some("v0.1.0".to_string()),
                started_at: Some(Utc::now()),
                finished_at: None,
            },
            "0.1.0",
        );

        assert!(!recovered.running);
        assert_eq!(recovered.phase, UpgradePhase::Succeeded);
    }

    #[test]
    fn pending_restart_for_other_version_stays_unconfirmed_on_startup() {
        let recovered = recover_persisted_status(
            UpgradeStatus {
                running: true,
                phase: UpgradePhase::Restarting,
                message: "service restart scheduled".to_string(),
                package: Some(PackageKind::Node),
                target_version: Some("v0.2.0".to_string()),
                started_at: Some(Utc::now()),
                finished_at: None,
            },
            "0.1.0",
        );

        assert!(recovered.running);
        assert_eq!(recovered.phase, UpgradePhase::Restarting);
    }

    #[test]
    fn persisted_status_tolerates_utf8_bom() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let path = temp.path().join("status.json");
        let status = UpgradeStatus {
            running: false,
            phase: UpgradePhase::Failed,
            message: "restart failed: boom".to_string(),
            package: Some(PackageKind::Full),
            target_version: Some("v0.1.2".to_string()),
            started_at: None,
            finished_at: Some(Utc::now()),
        };
        let json = serde_json::to_string(&status).expect("status should serialize");
        fs::write(&path, format!("\u{feff}{json}")).expect("status file should be written");

        let read = read_persisted_status(&path)
            .expect("a status file written by PowerShell 5.1 must still parse");

        assert_eq!(read.phase, UpgradePhase::Failed);
        assert_eq!(read.message, "restart failed: boom");
    }

    #[test]
    fn install_payload_stages_binary_when_running_executable_was_renamed() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let install_dir = temp.path().join("install");
        let payload_root = temp.path().join("payload");
        fs::create_dir_all(&install_dir).expect("install dir should be created");
        fs::create_dir_all(&payload_root).expect("payload dir should be created");

        // A user-renamed binary must not stop the archive's binary from being staged.
        let running_exe = install_dir.join(if cfg!(target_os = "windows") {
            "os-watcher_new.exe"
        } else {
            "os-watcher_new"
        });
        fs::write(&running_exe, "old").expect("running executable should be written");
        fs::write(payload_root.join(canonical_binary_name()), "new")
            .expect("archive binary should be written");

        install_payload(&payload_root, &install_dir, &running_exe)
            .expect("install should stage the archive binary");

        #[cfg(target_os = "windows")]
        let replaced = windows_staged_exe_path(&running_exe);
        #[cfg(not(target_os = "windows"))]
        let replaced = running_exe.clone();

        assert_eq!(
            fs::read_to_string(&replaced).expect("staged binary should be readable"),
            "new"
        );
    }

    #[test]
    fn install_payload_rejects_archive_without_binary() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let install_dir = temp.path().join("install");
        let payload_root = temp.path().join("payload");
        fs::create_dir_all(&install_dir).expect("install dir should be created");
        fs::create_dir_all(&payload_root).expect("payload dir should be created");
        fs::write(payload_root.join("README.md"), "docs").expect("readme should be written");
        let running_exe = install_dir.join(canonical_binary_name());
        fs::write(&running_exe, "old").expect("running executable should be written");

        let err = install_payload(&payload_root, &install_dir, &running_exe)
            .expect_err("an archive without the binary must not report success");

        assert!(err.to_string().contains("did not contain"));
    }

    #[cfg(unix)]
    #[test]
    fn install_payload_restores_executable_bit_on_deploy_script() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir should be created");
        let install_dir = temp.path().join("install");
        let payload_root = temp.path().join("payload");
        fs::create_dir_all(&install_dir).expect("install dir should be created");
        fs::create_dir_all(&payload_root).expect("payload dir should be created");
        let running_exe = install_dir.join(canonical_binary_name());
        fs::write(&running_exe, "old").expect("running executable should be written");
        fs::write(payload_root.join(canonical_binary_name()), "new")
            .expect("archive binary should be written");

        // Release tarballs carry deploy.sh without its executable bit.
        let payload_script = payload_root.join(DEPLOY_SCRIPT_NAME);
        fs::write(&payload_script, "#!/usr/bin/env bash\ntrue\n")
            .expect("deploy script should be written");
        fs::set_permissions(&payload_script, fs::Permissions::from_mode(0o644))
            .expect("deploy script should be made non-executable");

        install_payload(&payload_root, &install_dir, &running_exe)
            .expect("install should restore the deploy script mode");

        let installed = install_dir.join(DEPLOY_SCRIPT_NAME);
        let mode = fs::metadata(&installed)
            .expect("installed deploy script should exist")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o111,
            0o111,
            "installed deploy.sh must stay runnable after an upgrade"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_restart_script_writes_status_without_bom() {
        let request = windows_test_request();
        let script = windows_restart_script(&request).expect("script should be built");

        assert!(
            script.contains("[System.IO.File]::WriteAllText"),
            "status must be written as BOM-less UTF-8 so the parent can parse it"
        );
        assert!(
            !script.contains("Set-Content"),
            "status must not go through Set-Content, whose UTF8 encoding adds a BOM on PowerShell 5.1"
        );
        assert!(
            script.contains("sc.exe query") && script.contains("is not installed"),
            "the helper must report an unregistered service instead of a bare restart failure"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_restart_launcher_round_trips_script_as_single_token() {
        let request = windows_test_request();
        let script = windows_restart_script(&request).expect("script should be built");
        let launcher = windows_restart_launcher(&script);

        assert!(
            !launcher.contains("Start-Process"),
            "a re-parsed argument list strips the quotes inside the script"
        );

        let encoded = encode_powershell_command(&script);
        assert!(
            launcher.contains(&format!("-EncodedCommand {encoded}")),
            "the launcher must pass the encoded script verbatim"
        );

        let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &encoded)
            .expect("encoded script should be valid base64");
        let units = raw
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        assert_eq!(
            String::from_utf16(&units).expect("encoded script should be UTF-16"),
            script,
            "the child process must receive the script byte for byte"
        );
    }

    #[cfg(target_os = "windows")]
    fn windows_test_request() -> UpgradeHelperRequest {
        UpgradeHelperRequest {
            service_name: "os-watcher".to_string(),
            current_exe: PathBuf::from(r"C:\Program Files\os-watcher\os-watcher.exe"),
            install_dir: PathBuf::from(r"C:\Program Files\os-watcher"),
            backup_dir: PathBuf::from(r"C:\Program Files\os-watcher\backups\upgrade-1"),
            status_file: PathBuf::from(
                r"C:\Program Files\os-watcher\.os-watcher-upgrade-status.json",
            ),
            target_version: "v0.1.2".to_string(),
            package: PackageKind::Full,
        }
    }

    #[test]
    fn rollback_removes_files_that_were_not_in_backup() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let install_dir = temp.path().join("install");
        let backup_dir = temp.path().join("backup");
        fs::create_dir_all(&install_dir).expect("install dir should be created");
        fs::create_dir_all(&backup_dir).expect("backup dir should be created");

        let exe_name = if cfg!(target_os = "windows") {
            "os-watcher.exe"
        } else {
            "os-watcher"
        };
        let current_exe = install_dir.join(exe_name);
        fs::write(&current_exe, "new").expect("current executable should be written");
        fs::write(backup_dir.join(exe_name), "old").expect("backup executable should be written");
        fs::write(install_dir.join("config.example.toml"), "new")
            .expect("new config example should be written");
        fs::create_dir_all(install_dir.join("web-dist")).expect("web-dist should be created");
        fs::write(install_dir.join("web-dist").join("index.html"), "new")
            .expect("web asset should be written");

        rollback_backup(&backup_dir, &install_dir, &current_exe).expect("rollback should succeed");

        assert_eq!(
            fs::read_to_string(&current_exe).expect("executable should be restored"),
            "old"
        );
        assert!(!install_dir.join("config.example.toml").exists());
        assert!(!install_dir.join("web-dist").exists());
    }
}
