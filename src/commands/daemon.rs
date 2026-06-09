//! `heed daemon` — run the daemon in the foreground.

use std::path::PathBuf;

use crate::daemon::{run, DaemonPaths};

pub fn run_foreground() -> Result<(), String> {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| "HOME env var not set".to_string())?;
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
