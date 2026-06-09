//! `heed install` driver.
//!
//! Calls the install library, prints a summary, then spawns the daemon as a
//! detached child unless `--no-spawn` or `--service-install` was given.

use std::path::PathBuf;

use crate::daemon_spawn;
use crate::install::service::{render_linux_systemd, render_macos_plist, ServiceUnit};
use crate::install::{self, InstallOptions, InstallReport};

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
