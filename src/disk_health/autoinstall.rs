// src/disk_health/autoinstall.rs
use std::sync::atomic::{AtomicBool, Ordering};

/// Global flag: we only attempt to install once per process lifetime.
static INSTALL_ATTEMPTED: AtomicBool = AtomicBool::new(false);

/// Try to install smartmontools using the platform package manager.
/// Returns `true` if smartctl is now available (was already present, or
/// installation succeeded), `false` otherwise.
/// Attempts at most once; subsequent calls return `false` immediately.
pub fn try_install_smartctl() -> bool {
    // Already attempted → do not retry.
    if INSTALL_ATTEMPTED.swap(true, Ordering::SeqCst) {
        return false;
    }
    install_platform()
}

#[cfg(unix)]
fn needs_sudo() -> bool {
    // nix::unistd::geteuid() == 0 means already root → no sudo needed.
    nix::unistd::geteuid().as_raw() != 0
}

#[cfg(unix)]
fn install_platform() -> bool {
    // Ordered by prevalence.
    const MANAGERS: &[(&str, &[&str])] = &[
        ("apt",    &["apt", "install", "-y", "smartmontools"]),
        ("dnf",    &["dnf", "install", "-y", "smartmontools"]),
        ("yum",    &["yum", "install", "-y", "smartmontools"]),
        ("pacman", &["pacman", "-S", "--noconfirm", "smartmontools"]),
        ("zypper", &["zypper", "install", "-y", "smartmontools"]),
    ];

    for (manager, args) in MANAGERS {
        // Skip if the package manager is not installed.
        if std::process::Command::new("which")
            .arg(manager)
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            continue;
        }

        let success = if needs_sudo() {
            // Confirm sudo itself is available before prepending it.
            if std::process::Command::new("which")
                .arg("sudo")
                .output()
                .map(|o| !o.status.success())
                .unwrap_or(true)
            {
                false
            } else {
                let mut cmd_args = vec!["sudo"];
                cmd_args.extend_from_slice(args);
                std::process::Command::new(cmd_args[0])
                    .args(&cmd_args[1..])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            }
        } else {
            std::process::Command::new(args[0])
                .args(&args[1..])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };

        if success {
            tracing::info!("smartmontools installed via {}", manager);
            return true;
        }
    }

    tracing::warn!("smartctl auto-install failed: no usable package manager found");
    false
}

#[cfg(target_os = "windows")]
fn install_platform() -> bool {
    // Try winget first, then choco.
    let winget = std::process::Command::new("winget")
        .args(["install", "--id", "Smartmontools.Smartmontools", "--silent", "--accept-source-agreements", "--accept-package-agreements"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if winget {
        tracing::info!("smartmontools installed via winget");
        return true;
    }

    let choco = std::process::Command::new("choco")
        .args(["install", "smartmontools", "-y"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if choco {
        tracing::info!("smartmontools installed via choco");
        return true;
    }

    tracing::warn!("smartctl auto-install failed on Windows: winget and choco both unavailable or failed");
    false
}

#[cfg(not(any(unix, target_os = "windows")))]
fn install_platform() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Second call after the flag is set must return false immediately.
    #[test]
    fn does_not_retry_after_first_attempt() {
        // Reset flag manually for this isolated test.
        INSTALL_ATTEMPTED.store(false, Ordering::SeqCst);
        // First call will try (and fail in test env — no package manager in CI).
        let _first = try_install_smartctl();
        // Flag is now true. Second call must be false.
        let second = try_install_smartctl();
        assert!(!second, "must not retry installation");
        // Restore for other tests.
        INSTALL_ATTEMPTED.store(false, Ordering::SeqCst);
    }
}
