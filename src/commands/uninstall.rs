//! `heed install --uninstall` driver. Removes Heed entries from Claude/Codex
//! configs. Daemon shutdown is best-effort: on macOS the login item is
//! unregistered (from Heed.app) and any legacy launchd plist is booted out
//! and deleted first, then we send SIGTERM via the lockfile if present.

use std::path::{Path, PathBuf};

use crate::daemon_spawn;
use crate::install::{self, InstallOptions};
use crate::service::{self, AGENT_LABEL};

pub fn run(skip_claude: bool, skip_codex: bool) -> Result<(), String> {
    let opts = InstallOptions {
        home: home_dir()?,
        skip_claude,
        skip_codex,
    };
    teardown_service(&opts.home);
    if let Err(e) = daemon_spawn::stop_if_running(&opts.home) {
        eprintln!("Warning: could not stop daemon: {e}");
    }
    install::uninstall(&opts)?;
    println!("heed entries removed from Claude / Codex configs.");
    println!("Run `rm -rf ~/.heed` to fully clean up state files.");
    Ok(())
}

/// Bundle: unregister the SMAppService agent, then legacy cleanup. Bare
/// binary: legacy cleanup only. Runs before `stop_if_running` so launchd's
/// KeepAlive cannot respawn the daemon we are about to SIGTERM. Best-effort.
/// `~/.heed/bin/heed` is left alone (Codezilla's stable path).
fn teardown_service(home: &Path) {
    if service::bundle_context().is_some() {
        let agent = service::heed_agent();
        if agent.status().is_registered() {
            match agent.unregister() {
                Ok(()) => println!("{AGENT_LABEL} unregistered."),
                Err(e) => eprintln!("Warning: could not unregister {AGENT_LABEL}: {e}"),
            }
        }
    }
    match service::legacy::run_legacy_cleanup(home) {
        Ok(notes) => {
            for n in notes {
                println!("{n}");
            }
        }
        Err(e) => eprintln!("Warning: {e}"),
    }
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| "HOME env var not set".to_string())
}
