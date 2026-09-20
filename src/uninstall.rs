//! Uninstall: stop the service, optionally back up the config, remove the tree.
//!
//! A running agent cannot delete its own installation, and on Windows it cannot
//! even delete its own executable: the file is locked while the process runs.
//! The removal therefore runs in a **detached helper** that outlives the agent,
//! exactly like the self-upgrade restart helper.
//!
//! The two platforms need different helpers, because what holds the lock
//! differs:
//!
//! * **Linux** — unlinking a running executable is allowed, so the agent's own
//!   binary can do the work. `systemd-run` starts it as its own unit so it
//!   survives the `systemctl stop` it performs on the agent.
//! * **Windows** — nothing may delete a running `.exe`, so a PowerShell script
//!   does it. The script travels as a base64 `-EncodedCommand` through WMI: WMI
//!   spawns it outside the service's job object (so the stop does not kill it),
//!   and keeping the script off disk means there is no script file left holding
//!   the directory open.
//!
//! `uninstall.sh` / `uninstall.ps1` do the same work in-process, since there an
//! operator is driving and the script is not the agent.
//!
//! `POST /api/v1/uninstall` uses the detached path: it schedules the helper and
//! answers immediately, because the agent is about to be stopped and the
//! dashboard will not be able to reach it again.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Files kept when the operator asked to preserve the configuration.
const KEEP_WHEN_PRESERVING: &[&str] = &["config.toml"];

/// What an uninstall should do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UninstallOptions {
    /// Directory the agent is installed in.
    pub install_dir: PathBuf,
    /// Service to stop and unregister.
    pub service_name: String,
    /// Back up `config.toml` before removing the tree.
    pub backup: bool,
    /// Keep `config.toml` in place instead of deleting it with the tree.
    pub keep_config: bool,
    /// Where the backup goes. `None` picks a timestamped directory under
    /// `install_dir/backups`.
    pub backup_dir: Option<PathBuf>,
}

/// What an in-process removal achieved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UninstallOutcome {
    /// Directory the config was copied to, when a backup was taken.
    pub backup_dir: Option<PathBuf>,
    /// Files deliberately kept (only ever `config.toml`).
    pub kept: Vec<PathBuf>,
    /// Paths the OS will delete on the next reboot because they were locked.
    pub pending_reboot: Vec<PathBuf>,
}

impl UninstallOptions {
    /// Build options for an install directory, deriving the backup location.
    pub fn new(
        install_dir: impl Into<PathBuf>,
        service_name: impl Into<String>,
        backup: bool,
        keep_config: bool,
        backup_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            install_dir: install_dir.into(),
            service_name: service_name.into(),
            backup,
            keep_config,
            backup_dir,
        }
    }

    /// Directory the config is copied to when `backup` is set.
    ///
    /// It sits **beside** the install directory, never inside it: an uninstall
    /// deletes the install directory, so a backup kept there would be destroyed
    /// by the very run that took it.
    pub fn resolved_backup_dir(&self) -> PathBuf {
        match &self.backup_dir {
            Some(dir) => dir.clone(),
            None => {
                let stamp = Utc::now().format("%Y%m%d%H%M%S");
                let parent = self
                    .install_dir
                    .parent()
                    .unwrap_or_else(|| Path::new("."));
                parent.join(format!("os-watcher-backup-{stamp}"))
            }
        }
    }
}

/// Entries under `dir` that an uninstall removes, skipping what must survive.
///
/// Returns `(removable, kept)`. Kept entries are reported so the caller can
/// explain why the directory did not disappear.
pub fn plan_entries(dir: &Path, keep_config: bool) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut removable = Vec::new();
    let mut kept = Vec::new();

    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("read install directory {}", dir.display()))?;

    for entry in entries {
        let entry = entry.with_context(|| format!("read an entry of {}", dir.display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if keep_config && KEEP_WHEN_PRESERVING.iter().any(|keep| *keep == name) {
            kept.push(entry.path());
            continue;
        }
        removable.push(entry.path());
    }

    removable.sort();
    kept.sort();
    Ok((removable, kept))
}

/// Copy `config.toml` into `backup_dir`, if there is one.
///
/// Returns the directory that now holds the copy, or `None` when the install
/// had no config to preserve.
pub fn backup_config(install_dir: &Path, backup_dir: &Path) -> Result<Option<PathBuf>> {
    let config = install_dir.join("config.toml");
    if !config.is_file() {
        return Ok(None);
    }

    std::fs::create_dir_all(backup_dir)
        .with_context(|| format!("create backup directory {}", backup_dir.display()))?;
    let target = backup_dir.join("config.toml");
    std::fs::copy(&config, &target)
        .with_context(|| format!("back up {} to {}", config.display(), target.display()))?;
    Ok(Some(backup_dir.to_path_buf()))
}

/// Remove everything under `dir` except what must survive, then `dir` itself.
///
/// A path that cannot be removed is queued for deletion on reboot instead of
/// failing the whole uninstall: on Windows the running executable is always
/// locked. On Linux nothing is locked, so `pending` comes back empty.
pub fn remove_install_tree(dir: &Path, keep_config: bool) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let (removable, kept) = plan_entries(dir, keep_config)?;

    let mut pending = Vec::new();
    for path in removable {
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(error) = result {
            warn!("could not remove {}: {error}", path.display());
            pending.push(path);
        }
    }

    // The install directory itself goes last, and only when nothing is left in
    // it. A kept config keeps the directory alive on purpose.
    if kept.is_empty() && pending.is_empty() {
        if let Err(error) = std::fs::remove_dir(dir) {
            warn!("could not remove {}: {error}", dir.display());
            pending.push(dir.to_path_buf());
        }
    }

    Ok((kept, pending))
}

/// Queue paths for deletion on the next reboot.
///
/// Windows: `MoveFileExW(.., NULL, MOVEFILE_DELAY_UNTIL_REBOOT)`, the documented
/// way to remove a file that is still open. Linux needs no equivalent: an
/// unlinked executable disappears while it keeps running, so the caller is only
/// warned.
#[cfg(target_os = "windows")]
pub fn schedule_reboot_removal(paths: &[PathBuf]) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_DELAY_UNTIL_REBOOT: u32 = 0x4;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new_name: *const u16, flags: u32) -> i32;
    }

    for path in paths {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        // SAFETY: `wide` is NUL-terminated and outlives the call; a null
        // destination with MOVEFILE_DELAY_UNTIL_REBOOT asks for deletion.
        let ok = unsafe { MoveFileExW(wide.as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
        if ok == 0 {
            let error = std::io::Error::last_os_error();
            warn!(
                "could not queue {} for reboot removal: {error}",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn schedule_reboot_removal(paths: &[PathBuf]) -> Result<()> {
    if !paths.is_empty() {
        warn!("files could not be removed and need manual cleanup: {paths:?}");
    }
    Ok(())
}

/// Back up (when asked), stop the service and remove the tree, in this process.
///
/// A failed backup aborts before anything is deleted: losing the operator's
/// config is worse than a half-finished uninstall. The service is only stopped
/// once the backup is safe, so a failed backup never costs an outage either.
pub async fn run_uninstall(options: &UninstallOptions) -> Result<UninstallOutcome> {
    let backup_dir = if options.backup {
        backup_config(&options.install_dir, &options.resolved_backup_dir())?
    } else {
        None
    };

    // The helper runs in its own unit (`systemd-run --collect`), so stopping
    // the agent's service does not take the helper down with it.
    if let Err(error) = stop_and_remove_service(&options.service_name).await {
        warn!("could not fully unregister the service: {error:#}");
    }

    let (kept, pending) = remove_install_tree(&options.install_dir, options.keep_config)?;
    if !pending.is_empty() {
        schedule_reboot_removal(&pending)?;
    }

    Ok(UninstallOutcome {
        backup_dir,
        kept,
        pending_reboot: pending,
    })
}

/// Stop and unregister the service.
///
/// Best effort: a missing service is not an error, and a failure to stop it
/// must not block the removal.
pub async fn stop_and_remove_service(service_name: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let unit = format!("{service_name}.service");
        let _ = tokio::process::Command::new("systemctl")
            .args(["stop", service_name])
            .status()
            .await;
        let _ = tokio::process::Command::new("systemctl")
            .args(["disable", service_name])
            .status()
            .await;
        let path = Path::new("/etc/systemd/system").join(&unit);
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("remove unit file {}", path.display()))?;
        }
        let _ = tokio::process::Command::new("systemctl")
            .arg("daemon-reload")
            .status()
            .await;
        // `uninstall.sh` does the same: a unit that failed while running would
        // otherwise linger in `failed` state and confuse a later reinstall.
        let _ = tokio::process::Command::new("systemctl")
            .args(["reset-failed", service_name])
            .status()
            .await;
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        let _ = tokio::process::Command::new("sc.exe")
            .args(["stop", service_name])
            .status()
            .await;
        let status = tokio::process::Command::new("sc.exe")
            .args(["delete", service_name])
            .status()
            .await
            .context("delete Windows service")?;
        if !status.success() {
            warn!("sc.exe delete {service_name} returned {status}; it may already be gone");
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = service_name;
        Ok(())
    }
}

/// Arguments for the Linux `uninstall-helper` subcommand.
#[cfg(target_os = "linux")]
fn helper_args(options: &UninstallOptions) -> Vec<String> {
    let mut args = vec![
        "uninstall-helper".to_string(),
        "--install-dir".to_string(),
        options.install_dir.to_string_lossy().into_owned(),
        "--service-name".to_string(),
        options.service_name.clone(),
    ];
    if options.backup {
        args.push("--backup".to_string());
    }
    if options.keep_config {
        args.push("--keep-config".to_string());
    }
    if let Some(dir) = &options.backup_dir {
        args.push("--backup-dir".to_string());
        args.push(dir.to_string_lossy().into_owned());
    }
    args
}

/// Launch a detached process that performs the removal after this one exits.
#[cfg(target_os = "linux")]
pub fn spawn_detached_helper(options: &UninstallOptions) -> Result<()> {
    let exe = std::env::current_exe().context("locate the running executable")?;
    let unit = format!("os-watcher-uninstall-{}", Utc::now().timestamp_millis());

    // `systemd-run --collect` gives the helper its own unit, so stopping the
    // agent's service does not take the helper down with it.
    let status = std::process::Command::new("systemd-run")
        .args(["--unit", &unit, "--collect", "--quiet"])
        .arg(&exe)
        .args(helper_args(options))
        .status()
        .context("schedule detached uninstall helper with systemd-run")?;
    if !status.success() {
        return Err(anyhow!("failed to schedule the uninstall helper: {status}"));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn spawn_detached_helper(options: &UninstallOptions) -> Result<()> {
    let script = windows_uninstall_script(options)?;
    let launcher = windows_uninstall_launcher(&script);

    let status = std::process::Command::new("powershell")
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
        .context("schedule detached uninstall helper")?;
    if !status.success() {
        return Err(anyhow!("failed to schedule the uninstall helper: {status}"));
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn spawn_detached_helper(_options: &UninstallOptions) -> Result<()> {
    Err(anyhow!(
        "uninstall is only supported on Linux and Windows; remove the install directory by hand"
    ))
}

/// Build the PowerShell script that performs the whole uninstall.
///
/// It runs detached, so it stops and unregisters the service itself rather than
/// expecting the agent to have done it. Anything still locked (a console-run
/// agent's executable, for instance) is queued for deletion on the next reboot.
#[cfg(target_os = "windows")]
pub fn windows_uninstall_script(options: &UninstallOptions) -> Result<String> {
    let install_dir = quote_powershell(&options.install_dir);
    let service = quote_powershell_arg(&options.service_name);
    let config = quote_powershell(&options.install_dir.join("config.toml"));

    let backup_block = if options.backup {
        let dir = quote_powershell(&options.resolved_backup_dir());
        format!(
            r#"
if (Test-Path -LiteralPath {config}) {{
  New-Item -ItemType Directory -Path {dir} -Force | Out-Null
  Copy-Item -LiteralPath {config} -Destination (Join-Path {dir} 'config.toml') -Force
}}
"#
        )
    } else {
        String::new()
    };

    // Removing the whole tree in one call is the common case; the per-entry
    // pass exists only so a single locked file cannot block the rest.
    let remove_block = if options.keep_config {
        format!(
            r#"
Get-ChildItem -LiteralPath $installDir -Force |
  Where-Object {{ $_.Name -ne 'config.toml' }} |
  ForEach-Object {{ Remove-Entry $_.FullName }}
"#
        )
    } else {
        r#"
try {
  Remove-Item -LiteralPath $installDir -Recurse -Force -ErrorAction Stop
} catch {
  # 整树删失败通常是个别文件被占用。逐项删，删不掉的登记重启清理；
  # 最后再试一次目录本身，仍删不掉（被占用文件挡着）就一并登记：
  # 重启时按登记顺序先删文件、再删目录，不会剩一个空壳安装目录。
  Get-ChildItem -LiteralPath $installDir -Force | ForEach-Object { Remove-Entry $_.FullName }
  Remove-Entry $installDir
}
"#
        .to_string()
    };

    Ok(format!(
        r#"
$ErrorActionPreference = 'Stop'
$keepConfig = {keep_config}
$installDir = {install_dir}

# MoveFileEx 的 PENDING_DELETE：占用中的文件无法立即删除，登记为重启时删除。
# 必须用 IntPtr 重载：PowerShell 把 $null 传给 [string] 参数时会 marshal 成空
# 字符串而不是 NULL 指针，删除语义要求真正的 NULL，否则会静默失败。
if (-not ('Omp.Native' -as [type])) {{
  Add-Type -Namespace Omp -Name Native -MemberDefinition @'
[DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode, EntryPoint = "MoveFileExW")]
public static extern bool MoveFileEx(string lpExistingFileName, IntPtr lpNewFileName, int dwFlags);
'@
}}

function Remove-Entry([string]$path) {{
  try {{
    Remove-Item -LiteralPath $path -Recurse -Force -ErrorAction Stop
  }} catch {{
    [Omp.Native]::MoveFileEx($path, [IntPtr]::Zero, 4) | Out-Null
  }}
}}

Start-Sleep -Seconds 1
{backup_block}
# 停止并注销服务。缺失或已停止都不算失败：卸载要继续。
$svc = Get-Service -Name {service} -ErrorAction SilentlyContinue
if ($svc) {{
  if ($svc.Status -ne 'Stopped') {{
    Stop-Service -Name {service} -Force -ErrorAction SilentlyContinue
    try {{ $svc.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(30)) }} catch {{}}
  }}
  sc.exe delete {service} | Out-Null
}}
{remove_block}
"#,
        keep_config = if options.keep_config { "$true" } else { "$false" },
    ))
}

/// Wrap the uninstall script in the command line that launches it.
///
/// Same reasoning as the upgrade restart launcher: the script travels as a
/// single base64 (UTF-16LE) token, and WMI spawns it outside the service's job
/// object so the `Stop-Service` it performs does not kill it.
#[cfg(target_os = "windows")]
pub fn windows_uninstall_launcher(script: &str) -> String {
    let command_line = format!(
        "powershell.exe -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -EncodedCommand {}",
        encode_powershell_command(script)
    );
    format!(
        "Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{CommandLine={}}} | Out-Null",
        quote_powershell_arg(&command_line)
    )
}

#[cfg(target_os = "windows")]
fn encode_powershell_command(script: &str) -> String {
    use base64::Engine;
    let utf16: Vec<u8> = script
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(utf16)
}

/// Quote a value for a PowerShell single-quoted string.
fn quote_powershell_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Quote a path for PowerShell, tolerating the odd characters a path may hold.
fn quote_powershell(path: &Path) -> String {
    quote_powershell_arg(&path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_install() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let dir = temp.path().join("install");
        std::fs::create_dir_all(&dir).expect("install dir should be created");
        (temp, dir)
    }

    #[test]
    fn backup_copies_the_config_out_of_the_install_directory() {
        let (temp, dir) = temp_install();
        std::fs::write(dir.join("config.toml"), "[node]\n").expect("config should be written");
        let backup = temp.path().join("backup");

        let saved = backup_config(&dir, &backup).expect("backup should succeed");

        assert_eq!(saved.as_deref(), Some(backup.as_path()));
        assert_eq!(
            std::fs::read_to_string(backup.join("config.toml")).expect("backup should be readable"),
            "[node]\n"
        );
    }

    #[test]
    fn backup_of_a_missing_config_reports_nothing_saved() {
        let (temp, dir) = temp_install();
        let backup = temp.path().join("backup");

        let saved = backup_config(&dir, &backup).expect("backup should succeed");

        assert_eq!(saved, None);
        assert!(
            !backup.exists(),
            "an empty backup directory must not be left behind"
        );
    }

    #[test]
    fn default_backup_directory_is_timestamped_beside_the_install_directory() {
        let options = UninstallOptions::new("/opt/os-watcher", "os-watcher", true, false, None);

        let dir = options.resolved_backup_dir();

        assert_eq!(dir.parent(), Some(Path::new("/opt")));
        assert!(
            !dir.starts_with(&options.install_dir),
            "a backup inside the install directory would be deleted by the same run"
        );
        let name = dir
            .file_name()
            .expect("the backup directory has a name")
            .to_string_lossy();
        assert!(
            name.starts_with("os-watcher-backup-") && name.len() > "os-watcher-backup-".len(),
            "each run must get its own timestamped directory, got {name}"
        );
    }

    #[test]
    fn default_backup_directory_handles_an_install_directory_without_a_parent() {
        let options = UninstallOptions::new("/", "os-watcher", true, false, None);

        let dir = options.resolved_backup_dir();

        assert!(
            dir.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("os-watcher-backup-")),
            "a parentless install directory still gets a usable backup path, got {}",
            dir.display()
        );
    }

    #[test]
    fn explicit_backup_directory_wins() {
        let options = UninstallOptions::new(
            "/opt/os-watcher",
            "os-watcher",
            true,
            false,
            Some(PathBuf::from("/tmp/keep")),
        );

        assert_eq!(options.resolved_backup_dir(), PathBuf::from("/tmp/keep"));
    }

    #[test]
    fn removal_deletes_everything_and_the_directory_itself() {
        let (_temp, dir) = temp_install();
        std::fs::write(dir.join("os-watcher"), "binary").expect("binary should be written");
        std::fs::write(dir.join("config.toml"), "[node]\n").expect("config should be written");
        std::fs::create_dir_all(dir.join("web-dist")).expect("web-dist should be created");
        std::fs::write(dir.join("web-dist").join("index.html"), "<html>").expect("asset");

        let (kept, pending) = remove_install_tree(&dir, false).expect("removal should succeed");

        assert!(kept.is_empty());
        assert!(pending.is_empty(), "nothing is locked on this platform");
        assert!(!dir.exists(), "an unkept install directory must be gone");
    }

    #[test]
    fn keeping_the_config_preserves_it_and_its_directory() {
        let (_temp, dir) = temp_install();
        std::fs::write(dir.join("os-watcher"), "binary").expect("binary should be written");
        std::fs::write(dir.join("config.toml"), "[node]\n").expect("config should be written");

        let (kept, pending) = remove_install_tree(&dir, true).expect("removal should succeed");

        assert_eq!(kept, vec![dir.join("config.toml")]);
        assert!(pending.is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml")).expect("config should survive"),
            "[node]\n"
        );
        assert!(
            !dir.join("os-watcher").exists(),
            "the binary is removed even when the config is kept"
        );
    }

    #[test]
    fn hidden_files_are_removed_too() {
        let (_temp, dir) = temp_install();
        std::fs::write(dir.join(".os-watcher-upgrade-status.json"), "{}").expect("status file");

        let (kept, pending) = remove_install_tree(&dir, false).expect("removal should succeed");

        assert!(kept.is_empty());
        assert!(pending.is_empty());
        assert!(!dir.exists(), "a leftover dotfile must not keep the tree alive");
    }

    /// The service name is a sentinel that no host has registered: `run_uninstall`
    /// stops the service it is given, and a test must never touch a real one.
    const TEST_SERVICE: &str = "os-watcher-uninstall-test";

    #[tokio::test]
    async fn a_failed_backup_leaves_the_installation_untouched() {
        let (temp, dir) = temp_install();
        std::fs::write(dir.join("config.toml"), "[node]\n").expect("config should be written");
        // A file where the backup directory should go makes `create_dir_all` fail.
        let blocked = temp.path().join("blocked");
        std::fs::write(&blocked, "not a directory").expect("blocker should be written");

        let options =
            UninstallOptions::new(dir.clone(), TEST_SERVICE, true, false, Some(blocked));
        let error = run_uninstall(&options)
            .await
            .expect_err("a failed backup must abort");

        assert!(format!("{error:#}").contains("create backup directory"));
        assert!(
            dir.join("config.toml").exists(),
            "a failed backup must not cost the operator the installation"
        );
    }

    #[tokio::test]
    async fn a_successful_run_reports_the_backup_location() {
        let (temp, dir) = temp_install();
        std::fs::write(dir.join("config.toml"), "[node]\n").expect("config should be written");
        std::fs::write(dir.join("os-watcher"), "binary").expect("binary should be written");
        let backup = temp.path().join("backup");

        let options = UninstallOptions::new(
            dir.clone(),
            TEST_SERVICE,
            true,
            false,
            Some(backup.clone()),
        );
        let outcome = run_uninstall(&options).await.expect("uninstall should succeed");

        assert_eq!(outcome.backup_dir.as_deref(), Some(backup.as_path()));
        assert!(backup.join("config.toml").is_file());
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn skipping_the_backup_removes_the_config_with_the_tree() {
        let (_temp, dir) = temp_install();
        std::fs::write(dir.join("config.toml"), "[node]\n").expect("config should be written");

        let options = UninstallOptions::new(dir.clone(), TEST_SERVICE, false, false, None);
        let outcome = run_uninstall(&options).await.expect("uninstall should succeed");

        assert_eq!(outcome.backup_dir, None);
        assert!(!dir.exists());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_script_stops_the_service_and_backs_up_the_config() {
        let options = UninstallOptions::new(
            r"C:\Program Files\os-watcher",
            "os-watcher",
            true,
            false,
            None,
        );

        let script = windows_uninstall_script(&options).expect("script should be built");

        assert!(
            script.contains("Stop-Service") && script.contains("sc.exe delete"),
            "the helper has to stop and unregister the service itself"
        );
        assert!(
            script.contains("Copy-Item") && script.contains("os-watcher-backup-"),
            "a requested backup must be taken before the tree is removed"
        );
        assert!(
            script.contains("MoveFileEx"),
            "locked files must be queued for deletion on reboot"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_script_keeps_the_config_only_when_asked() {
        let base = UninstallOptions::new(r"C:\os-watcher", "os-watcher", false, false, None);
        let keeping = UninstallOptions::new(r"C:\os-watcher", "os-watcher", false, true, None);

        let removing = windows_uninstall_script(&base).expect("script should be built");
        let kept = windows_uninstall_script(&keeping).expect("script should be built");

        assert!(removing.contains("$keepConfig = $false"));
        assert!(kept.contains("$keepConfig = $true"));
    }

    /// `keep_config` 时绝不能删除或登记安装目录本身：目录里留着用户要求保留的
    /// `config.toml`，删目录等于把它一起删掉。
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_script_never_removes_the_install_dir_when_keeping_the_config() {
        let keeping = UninstallOptions::new(r"C:\os-watcher", "os-watcher", false, true, None);
        let script = windows_uninstall_script(&keeping).expect("script should be built");

        let body = script
            .split("$keepConfig = $true")
            .nth(1)
            .expect("the keepConfig branch should follow the flag");
        assert!(
            !body.contains("Remove-Entry $installDir") && !body.contains("$installDir -Recurse"),
            "keeping the config must leave the install directory alone, got:\n{body}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_launcher_round_trips_the_script_as_one_encoded_token() {
        let options = UninstallOptions::new(r"C:\os-watcher", "os-watcher", true, false, None);
        let script = windows_uninstall_script(&options).expect("script should be built");
        let launcher = windows_uninstall_launcher(&script);

        assert!(
            launcher.contains("Win32_Process") && launcher.contains("EncodedCommand"),
            "the helper must be spawned detached, with the script encoded inline"
        );
        assert!(
            !launcher.contains("Stop-Service"),
            "the script itself must not appear literally in the launcher"
        );
    }
}
