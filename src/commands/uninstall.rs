//! `heed install --uninstall` driver. Removes Heed entries from Claude/Codex
//! configs. Daemon shutdown is best-effort: we send SIGTERM via the lockfile
//! if present.

use std::path::PathBuf;

use crate::daemon_spawn;
use crate::install::{self, InstallOptions};

pub fn run(skip_claude: bool, skip_codex: bool) -> Result<(), String> {
    let opts = InstallOptions {
        home: home_dir()?,
        skip_claude,
        skip_codex,
    };
    if let Err(e) = daemon_spawn::stop_if_running(&opts.home) {
        eprintln!("Warning: could not stop daemon: {e}");
    }
    install::uninstall(&opts)?;
    println!("heed entries removed from Claude / Codex configs.");
    println!("Run `rm -rf ~/.heed` to fully clean up state files.");
    Ok(())
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| "HOME env var not set".to_string())
}
