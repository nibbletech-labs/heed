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
    if teardown_service(&opts.home) {
        // launchd tears the job down asynchronously; let its process go on
        // its own so the SIGTERM below is not aimed at a pid that just
        // exited (and its pidfile has a chance to be cleared first).
        daemon_spawn::wait_for_daemon_exit(&opts.home);
    }
    if let Err(e) = daemon_spawn::stop_if_running(&opts.home) {
        eprintln!("Warning: could not stop daemon: {e}");
    }
    install::uninstall(&opts)?;
    for line in uninstall_summary_lines(skip_claude, skip_codex) {
        println!("{line}");
    }
    println!("Run `rm -rf ~/.heed` to fully clean up state files.");
    Ok(())
}

/// One line per hook target actually processed; skipped targets say so.
fn uninstall_summary_lines(skip_claude: bool, skip_codex: bool) -> Vec<String> {
    [
        ("Claude", "claude", skip_claude),
        ("Codex", "codex", skip_codex),
    ]
    .into_iter()
    .map(|(name, flag, skipped)| {
        if skipped {
            format!("{name} config left alone (--skip-{flag}).")
        } else {
            format!("heed entries removed from {name} config.")
        }
    })
    .collect()
}

/// Bundle: unregister the SMAppService agent, then legacy cleanup. Bare
/// binary: legacy cleanup only. Runs before `stop_if_running` so launchd's
/// KeepAlive cannot respawn the daemon we are about to SIGTERM. Best-effort.
/// `~/.heed/bin/heed` is left alone (Codezilla's stable path). Returns
/// `true` when launchd was asked to take a job down (agent unregistered or
/// legacy plist booted out), i.e. a daemon may still be on its way out.
fn teardown_service(home: &Path) -> bool {
    let mut launchd_touched = false;
    if service::bundle_context().is_some() {
        let agent = service::heed_agent();
        if agent.status().is_registered() {
            match agent.unregister() {
                Ok(()) => {
                    println!("{AGENT_LABEL} unregistered.");
                    launchd_touched = true;
                }
                Err(e) => eprintln!("Warning: could not unregister {AGENT_LABEL}: {e}"),
            }
        }
    }
    match service::legacy::run_legacy_cleanup(home) {
        Ok(notes) => {
            launchd_touched |= !notes.is_empty();
            for n in notes {
                println!("{n}");
            }
        }
        Err(e) => eprintln!("Warning: {e}"),
    }
    launchd_touched
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| "HOME env var not set".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HD-11 (6): only targets that were actually processed are reported
    /// as cleaned; skipped ones are named as skipped.
    #[test]
    fn uninstall_summary_names_only_processed_targets() {
        assert_eq!(
            uninstall_summary_lines(false, false),
            vec![
                "heed entries removed from Claude config.",
                "heed entries removed from Codex config.",
            ]
        );
        assert_eq!(
            uninstall_summary_lines(true, false),
            vec![
                "Claude config left alone (--skip-claude).",
                "heed entries removed from Codex config.",
            ]
        );
        assert_eq!(
            uninstall_summary_lines(true, true),
            vec![
                "Claude config left alone (--skip-claude).",
                "Codex config left alone (--skip-codex).",
            ]
        );
    }
}
