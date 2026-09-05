//! `heed install` driver.
//!
//! Calls the install library, prints a summary, then spawns the daemon as a
//! detached child unless `--no-spawn` or `--service-install` was given.
//!
//! Inside `Heed.app` on macOS 13+ neither flag matters: the daemon is
//! registered with ServiceManagement as a login item (`dev.heed.agent`),
//! after any legacy `dev.heed.daemon` plist has been booted out and removed.

use std::path::{Path, PathBuf};

use crate::daemon_spawn;
use crate::install::service::{render_linux_systemd, render_macos_plist, ServiceUnit};
use crate::install::{self, InstallOptions, InstallReport};
use crate::service::{self, BundleLayout, ServiceStatus, AGENT_LABEL};

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

/// Bundle path. HD-1 order: bootout legacy → delete legacy plist → symlink →
/// register — but only after ServiceManagement has confirmed it can see this
/// bundle, so an unsealed bundle never dismantles a working legacy agent.
fn register_bundle_agent(home: &Path, bundle: &BundleLayout) -> Result<(), String> {
    let agent = service::heed_agent();
    let before = agent.status();

    if before == ServiceStatus::NotFound {
        return Err(format!(
            "Heed.app at {} is not sealed/visible to ServiceManagement (status notFound); \
             legacy agent left untouched. Is the bundle Developer-ID signed?",
            bundle.root.display()
        ));
    }

    for note in service::legacy::run_legacy_cleanup(home)? {
        println!("  {note}");
    }
    service::ensure_cli_symlink(home, &bundle.executable)?;
    println!(
        "  {} -> {}",
        service::cli_symlink_path(home).display(),
        bundle.executable.display()
    );

    if before.is_registered() {
        println!("\n{AGENT_LABEL} already registered ({}).", before.as_str());
        if before == ServiceStatus::RequiresApproval {
            print_approval_hint();
        }
        return Ok(());
    }

    // NotRegistered: stop a foreground-spawned daemon (pidfile) so the
    // launchd-managed one owns the state files, then register.
    if let Err(e) = daemon_spawn::stop_if_running(home) {
        eprintln!("Warning: could not stop daemon: {e}");
    }
    agent.register()?;
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
