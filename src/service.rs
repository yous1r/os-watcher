//! Windows Service Control Manager host.
//!
//! `sc.exe` cannot host a console program: the SCM requires the process to call
//! `StartServiceCtrlDispatcherW` and to keep reporting its state, which a plain
//! `main()` never does. Registering the released binary without that call makes
//! every `sc.exe start` fail with "error 1053: the service did not respond to
//! the start or control request in a timely fashion".
//!
//! This module speaks that protocol, so `sc.exe create` alone is enough to run
//! the agent as a service — no nssm or other wrapper.
//!
//! Non-Windows targets get inert stubs so the rest of the crate can call in
//! without `#[cfg]` at every site.

/// Hand the process to the SCM if it was launched as a service.
///
/// Returns `Ok(true)` when the process *was* a service and has now stopped, so
/// `main` should exit. Returns `Ok(false)` for an ordinary console launch,
/// which the caller runs as usual.
#[cfg(target_os = "windows")]
pub fn dispatch(cli: &crate::Cli) -> anyhow::Result<bool> {
    imp::dispatch(cli)
}

#[cfg(not(target_os = "windows"))]
pub fn dispatch(_cli: &crate::Cli) -> anyhow::Result<bool> {
    Ok(false)
}

/// Whether this process is running as a Windows service.
pub fn is_service() -> bool {
    #[cfg(target_os = "windows")]
    {
        imp::is_service()
    }

    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// Directory the binary was installed in, when running as a service.
///
/// A service starts with its working directory set to `System32`, so every
/// relative path a release config ships with would resolve there. Callers use
/// this to anchor them to the install directory instead.
pub fn install_dir() -> Option<std::path::PathBuf> {
    if !is_service() {
        return None;
    }
    std::env::current_exe()
        .ok()?
        .parent()
        .map(std::path::Path::to_path_buf)
}

/// Resolve a possibly-relative path against the install directory.
pub fn anchor(path: impl AsRef<std::path::Path>) -> std::path::PathBuf {
    match install_dir() {
        Some(dir) => anchor_to(&dir, path.as_ref()),
        None => path.as_ref().to_path_buf(),
    }
}

/// Join `path` onto `base` unless it is already absolute.
///
/// Split from [`anchor`] so the rule can be tested without a service around it.
pub fn anchor_to(base: &std::path::Path, path: &std::path::Path) -> std::path::PathBuf {
    if path.is_relative() {
        base.join(path)
    } else {
        path.to_path_buf()
    }
}

/// Report the agent as fully started. No-op outside a service.
///
/// The SCM holds the service in "start pending" until this runs, so calling it
/// only once the listeners are up turns a failed startup into a failed
/// `sc.exe start` instead of a service that reports RUNNING and then dies.
#[cfg(target_os = "windows")]
pub fn report_ready() {
    imp::report_ready();
}

#[cfg(not(target_os = "windows"))]
pub fn report_ready() {}

/// Resolves when the SCM asks the service to stop.
///
/// Never resolves outside a service, so callers can `select!` it against
/// `tokio::signal::ctrl_c()`.
#[cfg(target_os = "windows")]
pub async fn stopped() {
    imp::stopped().await;
}

#[cfg(not(target_os = "windows"))]
pub async fn stopped() {
    std::future::pending::<()>().await;
}

#[cfg(target_os = "windows")]
mod imp {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    use anyhow::{anyhow, Context, Result};
    use tokio::sync::Notify;
    use tracing::{error, info, warn};
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{
        self, ServiceControlHandlerResult, ServiceStatusHandle,
    };
    use windows_service::service_dispatcher;
    use windows_service::Error as ServiceError;

    /// Entry point registered with the SCM.
    ///
    /// The name is ignored for `SERVICE_WIN32_OWN_PROCESS` services, so a
    /// service registered under a custom `sc.exe create` name still reaches
    /// this entry point.
    const SERVICE_NAME: &str = "os-watcher";

    /// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`: the process was not started
    /// by the SCM, i.e. somebody ran the binary from a console.
    const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: i32 = 1063;

    /// How long the SCM tolerates a pending start or stop before it declares
    /// the service hung. Has to fit in `u32` milliseconds.
    const START_HINT: Duration = Duration::from_secs(30);
    const STOP_HINT: Duration = Duration::from_secs(30);

    /// Launch parameters, published before the dispatcher blocks so the
    /// `extern "system"` entry point can reach them.
    static LAUNCH: OnceLock<crate::Cli> = OnceLock::new();
    /// Set once the control handler is registered.
    static STATUS: OnceLock<ServiceStatusHandle> = OnceLock::new();
    /// Set once the control handler is registered; `None` outside a service.
    static STOP: OnceLock<Arc<Notify>> = OnceLock::new();

    windows_service::define_windows_service!(ffi_service_main, service_main);

    pub(super) fn dispatch(cli: &crate::Cli) -> Result<bool> {
        let _ = LAUNCH.set(cli.clone());

        match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
            Ok(()) => Ok(true),
            Err(ServiceError::Winapi(e))
                if e.raw_os_error() == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) =>
            {
                Ok(false)
            }
            Err(e) => Err(anyhow!("StartServiceCtrlDispatcherW failed: {e}")),
        }
    }

    pub(super) fn is_service() -> bool {
        STOP.get().is_some()
    }

    pub(super) fn report_ready() {
        report(
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            Duration::ZERO,
            0,
            ServiceExitCode::NO_ERROR,
        );
    }

    pub(super) async fn stopped() {
        match STOP.get() {
            Some(stop) => stop.notified().await,
            // Console run: never resolve, so `select!` falls through to ctrl_c.
            None => std::future::pending().await,
        }
    }

    /// Runs on the thread the SCM creates for us, while `dispatch` keeps the
    /// main thread blocked inside `StartServiceCtrlDispatcherW`.
    fn service_main(_arguments: Vec<OsString>) {
        let Some(cli) = LAUNCH.get().cloned() else {
            // Only reachable if the SCM invoked us without `dispatch` running
            // first, which the process layout makes impossible.
            return;
        };

        // A service has no console to write to, so logs go to a file next to
        // the binary unless `--log-file` says otherwise. The guard has to stay
        // alive for the whole service or the writer thread stops flushing.
        let _log_guard = match crate::init_logging(
            &cli.log_level,
            cli.log_file.clone().or_else(default_log_file),
        ) {
            Ok(guard) => guard,
            Err(e) => {
                // Reported before the handler exists, so `sc.exe start` fails
                // with 1053 rather than a service that runs but logs nowhere.
                eprintln!("Could not open the service log file: {e:#}");
                return;
            }
        };

        let stop = Arc::new(Notify::new());
        let _ = STOP.set(Arc::clone(&stop));

        let handler_stop = Arc::clone(&stop);
        let event_handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
            match control {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    report(
                        ServiceState::StopPending,
                        ServiceControlAccept::empty(),
                        STOP_HINT,
                        0,
                        ServiceExitCode::NO_ERROR,
                    );
                    handler_stop.notify_one();
                    ServiceControlHandlerResult::NoError
                }
                // The SCM polls this every few seconds; without an answer it
                // assumes the service is unresponsive.
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        match service_control_handler::register(SERVICE_NAME, event_handler) {
            Ok(handle) => {
                let _ = STATUS.set(handle);
            }
            Err(e) => {
                eprintln!("Could not register the service control handler: {e}");
                return;
            }
        }

        report(
            ServiceState::StartPending,
            ServiceControlAccept::empty(),
            START_HINT,
            1,
            ServiceExitCode::NO_ERROR,
        );

        match run_agent(cli) {
            Ok(()) => {
                info!("Service stopped");
                report(
                    ServiceState::Stopped,
                    ServiceControlAccept::empty(),
                    Duration::ZERO,
                    0,
                    ServiceExitCode::NO_ERROR,
                );
            }
            Err(e) => {
                error!("Service failed: {e:#}");
                // Non-zero so `sc.exe start` and the upgrade restart script
                // see the failure instead of a clean stop.
                report(
                    ServiceState::Stopped,
                    ServiceControlAccept::empty(),
                    Duration::ZERO,
                    0,
                    ServiceExitCode::Win32(1),
                );
            }
        }
    }

    /// Drive the agent to completion on the service thread's own runtime; the
    /// main thread is parked inside the dispatcher and cannot host it.
    fn run_agent(cli: crate::Cli) -> Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("build the agent runtime")?
            .block_on(crate::run(cli))
    }

    /// Services have no stdout, so an unconfigured service logs next to the
    /// binary it was installed as.
    fn default_log_file() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        Some(exe.parent()?.join("os-watcher.log"))
    }

    fn report(
        state: ServiceState,
        accepted: ServiceControlAccept,
        hint: Duration,
        checkpoint: u32,
        exit_code: ServiceExitCode,
    ) {
        let Some(handle) = STATUS.get() else {
            return;
        };
        let status = ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accepted,
            exit_code,
            checkpoint,
            wait_hint: hint,
            process_id: None,
        };
        if let Err(e) = handle.set_service_status(status) {
            warn!("Could not report service state {state:?}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A release config ships `db_path = "os-watcher.db"` and `web.dir =
    /// "web-dist"`. A service starts in `System32`, so leaving those relative
    /// writes the database and reads the dashboard from the wrong directory.
    #[test]
    fn relative_paths_are_anchored_to_the_install_directory() {
        let base = std::path::Path::new(r"C:\Program Files\os-watcher");

        assert_eq!(
            anchor_to(base, std::path::Path::new("os-watcher.db")),
            base.join("os-watcher.db")
        );
        assert_eq!(
            anchor_to(base, std::path::Path::new("web-dist")),
            base.join("web-dist")
        );
    }

    /// An operator who wrote an absolute path meant it, and a console run
    /// already resolves relative paths against its own working directory.
    #[test]
    fn absolute_paths_and_console_runs_are_left_alone() {
        let base = std::path::Path::new(r"C:\Program Files\os-watcher");
        let absolute = std::path::Path::new(r"D:\data\os-watcher.db");

        assert_eq!(anchor_to(base, absolute), absolute);
        assert!(
            install_dir().is_none(),
            "a test process is not a service, so nothing may be re-anchored"
        );
        assert_eq!(
            anchor(std::path::Path::new("os-watcher.db")),
            std::path::PathBuf::from("os-watcher.db")
        );
    }

    /// The guard against a regression where the relative-path rule is applied
    /// in the console path too: a non-service process must be a no-op.
    #[test]
    fn is_service_is_false_for_a_console_process() {
        assert!(!is_service());
    }
}
