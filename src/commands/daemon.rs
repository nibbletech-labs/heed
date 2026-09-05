//! `heed daemon` — run the daemon in the foreground.

use std::path::PathBuf;

use crate::daemon::{run, DaemonPaths};
use crate::service;

pub fn run_foreground() -> Result<(), String> {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| "HOME env var not set".to_string())?;

    // Spawned by launchd? The bundle's agent plist has no StandardOutPath,
    // so point our own stdio at ~/.heed/heedd.{out,err}.log.
    if let Some(label) = std::env::var_os("XPC_SERVICE_NAME")
        .filter(|v| service::launchd_log::under_launchd(Some(v.as_os_str())))
    {
        let label = label.to_string_lossy().into_owned();
        if let Err(e) = service::launchd_log::redirect_stdio_to_logs(&home, &label) {
            eprintln!("heed daemon: could not redirect logs: {e}");
        }
    }

    let paths = DaemonPaths {
        event_log: crate::install::event_log_path(&home),
        state_file: crate::install::state_path(&home),
        owners_file: crate::install::owners_path(&home),
    };

    // Write our PID lockfile so `heed install` won't double-spawn us.
    let pid_path = crate::daemon_spawn::pid_lockfile(&home);
    let pid = std::process::id();
    let lstart = crate::liveness::current_lstart(pid).unwrap_or_default();
    let _ = crate::install::atomic_write(&pid_path, format!("{pid}\n{lstart}\n").as_bytes());

    let result = run(paths);
    let _ = std::fs::remove_file(&pid_path);
    result
}
