//! `heed install` driver.
//!
//! Calls the install library, prints a summary, then spawns the daemon as a
//! detached child unless `--no-spawn` or `--service-install` was given.
//!
//! Inside `Heed.app` on macOS 13+ neither flag matters: the daemon is
//! registered with ServiceManagement as a login item (`dev.heed.agent`);
//! any legacy `dev.heed.daemon` agent is booted out first and its plist
//! removed once registration has succeeded.

use std::path::{Path, PathBuf};

use crate::daemon_spawn;
use crate::install::service::{render_linux_systemd, render_macos_plist, ServiceUnit};
use crate::install::{self, InstallOptions, InstallReport};
use crate::service::{self, BundleLayout, RegisterAction, ServiceStatus, AGENT_LABEL};

#[derive(Clone, Debug)]
pub struct InstallArgs {
    pub skip_claude: bool,
    pub skip_codex: bool,
    pub no_spawn: bool,
    pub service_install: bool,
}

pub fn run(args: InstallArgs) -> Result<(), String> {
    let opts = InstallOptions {
        home: home_dir()?,
        skip_claude: args.skip_claude,
        skip_codex: args.skip_codex,
    };

    let report = install::install(&opts)?;
    print_report(&report);

    if let Some(bundle) = service::bundle_context() {
        return register_bundle_agent(&opts.home, &bundle);
    }

    if args.service_install {
        write_service_unit(&opts.home)?;
        return Ok(());
    }

    if !args.no_spawn {
        match daemon_spawn::spawn_if_not_running(&opts.home) {
            Ok(daemon_spawn::SpawnResult::Spawned(pid)) => {
                println!("\nDaemon spawned (pid {pid}). Run `heed status` to see threads.");
            }
            Ok(daemon_spawn::SpawnResult::AlreadyRunning(pid)) => {
                println!("\nDaemon already running (pid {pid}).");
            }
            Err(e) => eprintln!("\nWarning: could not spawn daemon: {e}"),
        }
    }
    Ok(())
}

/// Bundle path (criterion 1). Decided from `SMAppService.status` alone —
/// there is no "unsealed bundle" pre-check, because `notFound` is exactly
/// what a fresh, valid bundle reports before its first `register()`.
///
/// Register path: boot out the legacy agent (plist file kept) → stop any
/// pidfile daemon → `register()`. On success delete the legacy plist and
/// ensure the `~/.heed/bin/heed` symlink; on failure bootstrap the untouched
/// legacy plist again and return the NSError text, so a bundle that
/// ServiceManagement rejects never dismantles a working legacy agent.
///
/// Already-registered path (`enabled` / `requiresApproval`): skip
/// `register()`, run the full legacy cleanup and the symlink as before.
///
/// The legacy label is only ever booted out / bootstrapped through
/// `launchctl`; it never reaches `SMAppService` (HD-2).
fn register_bundle_agent(home: &Path, bundle: &BundleLayout) -> Result<(), String> {
    let agent = service::heed_agent();
    let before = agent.status();

    match service::register_action(before) {
        RegisterAction::AlreadyRegistered => {
            for note in service::legacy::run_legacy_cleanup(home)? {
                println!("  {note}");
            }
            ensure_symlink(home, bundle)?;
            println!("\n{AGENT_LABEL} already registered ({}).", before.as_str());
            if before == ServiceStatus::RequiresApproval {
                print_approval_hint();
            }
            Ok(())
        }
        RegisterAction::Register => {
            // The bundle plist is RunAtLoad: registering starts the new
            // daemon at once, so nothing else may own the state files then.
            let plan = service::legacy::plan_legacy_cleanup(home);
            let bootout_notes = service::legacy::bootout_legacy(&plan);
            for note in &bootout_notes {
                println!("  {note}");
            }
            if !bootout_notes.is_empty() {
                // `launchctl bootout` returns before the job's process is
                // fully gone; let the pidfile daemon disappear on its own so
                // the SIGTERM below is not aimed at a pid that just exited.
                daemon_spawn::wait_for_daemon_exit(home);
            }
            let stopped_daemon = match daemon_spawn::stop_if_running(home) {
                Ok(stopped) => stopped,
                Err(e) => {
                    eprintln!("Warning: could not stop daemon: {e}");
                    false
                }
            };

            let outcome = service::legacy::decide_after_register(&plan, agent.register());
            let respawn = service::legacy::respawn_after_rollback(&outcome, stopped_daemon);
            let finished = service::legacy::finish_migration(outcome);
            if respawn {
                // No legacy plist to bootstrap again, so the rollback would
                // leave the daemon we just stopped stopped. Put it back the
                // way a bare-binary install runs it.
                match daemon_spawn::spawn_if_not_running(home) {
                    Ok(daemon_spawn::SpawnResult::Spawned(pid)) => {
                        eprintln!(
                            "Registration failed; restarted the daemon it stopped (pid {pid})."
                        )
                    }
                    Ok(daemon_spawn::SpawnResult::AlreadyRunning(pid)) => {
                        eprintln!("Registration failed; daemon already running again (pid {pid}).")
                    }
                    Err(e) => eprintln!("Warning: could not restart daemon: {e}"),
                }
            }
            let notes = finished.map_err(|e| format!("could not register {AGENT_LABEL}: {e}"))?;
            for note in notes {
                println!("  {note}");
            }
            ensure_symlink(home, bundle)?;

            let after = agent.status();
            println!(
                "\nRegistered {AGENT_LABEL} via SMAppService: {}",
                after.as_str()
            );
            if after == ServiceStatus::RequiresApproval {
                print_approval_hint();
            }
            Ok(())
        }
    }
}

fn ensure_symlink(home: &Path, bundle: &BundleLayout) -> Result<(), String> {
    service::ensure_cli_symlink(home, &bundle.executable)?;
    println!(
        "  {} -> {}",
        service::cli_symlink_path(home).display(),
        bundle.executable.display()
    );
    Ok(())
}

fn print_approval_hint() {
    println!(
        "Allow \"Heed\" under System Settings › General › Login Items & Extensions to start it."
    );
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| "HOME env var not set".to_string())
}

fn print_report(report: &InstallReport) {
    println!("heed install");
    println!("  heed dir: {}", report.heed_dir.display());
    for (name, sub) in [("claude", &report.claude), ("codex", &report.codex)] {
        let Some(sub) = sub else {
            println!("  {name}: skipped (--skip-{name})");
            continue;
        };
        print!("  {name}: scripts {}", sub.scripts_dir.display());
        if sub.scripts_extracted {
            print!(" (extracted)");
        }
        match &sub.settings_path {
            Some(p) if sub.settings_changed => println!(" → {} (updated)", p.display()),
            Some(p) => println!(" → {} (no change)", p.display()),
            None => println!(),
        }
        for note in &sub.notes {
            println!("       ↳ {note}");
        }
    }
}

fn write_service_unit(home: &std::path::Path) -> Result<(), String> {
    let heed_binary = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let unit: ServiceUnit = if cfg!(target_os = "macos") {
        render_macos_plist(home, &heed_binary)
    } else {
        render_linux_systemd(home, &heed_binary)
    };
    install::atomic_write(&unit.path, unit.contents.as_bytes())?;
    println!("Service unit written to {}", unit.path.display());
    if cfg!(target_os = "macos") {
        println!(
            "To start now: launchctl bootstrap gui/$(id -u) {}",
            unit.path.display()
        );
    } else {
        println!("To start now: systemctl --user enable --now heed");
    }
    Ok(())
}
