//! `heed` CLI entry point.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use heed::commands;
use heed::state::Cli as ThreadCli;

#[derive(Parser, Debug)]
#[command(
    name = "heed",
    version,
    about = "Watch Claude Code and Codex threads on your machine"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Install Heed hooks for detected CLIs (idempotent). Also spawns the
    /// daemon if it isn't already running.
    Install {
        /// Skip installing Claude hooks.
        #[arg(long = "skip-claude")]
        skip_claude: bool,
        /// Skip installing Codex hooks.
        #[arg(long = "skip-codex")]
        skip_codex: bool,
        /// Skip auto-spawning the daemon after install.
        #[arg(long = "no-spawn")]
        no_spawn: bool,
        /// Write launchd plist (macOS) / systemd user unit (Linux) for true
        /// autostart instead of foreground-spawning the daemon.
        #[arg(long = "service-install")]
        service_install: bool,
        /// Remove Heed entries from Claude/Codex configs instead of installing.
        #[arg(long = "uninstall")]
        uninstall: bool,
    },

    /// Run the daemon in the foreground. Used by launchd/systemd, or for debug.
    Daemon,

    /// One-shot snapshot of all active threads.
    Status {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        ascii: bool,
    },

    /// Alias for `heed status`.
    Threads {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        ascii: bool,
    },

    /// Live-refreshing list (Ctrl-C to exit).
    Watch {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        ascii: bool,
        /// macOS only — fire a notification when any thread transitions to
        /// awaiting_input.
        #[arg(long)]
        notify: bool,
    },

    /// Tail the event log.
    Events {
        #[arg(long)]
        json: bool,
        /// Follow new events instead of just printing the tail.
        #[arg(long, short = 'f')]
        follow: bool,
    },

    /// Interactive two-pane TUI (j/k navigate, e events, s toggle gone, q quit).
    Tui,

    /// Register or remove owner metadata for a native CLI session.
    Owner {
        #[command(subcommand)]
        action: OwnerCmd,
    },

    /// Print heed + bundled hook script versions.
    Version,
}

#[derive(Subcommand, Debug)]
enum OwnerCmd {
    Register {
        #[arg(long)]
        cli: CliArg,
        #[arg(long = "native-thread-id")]
        native_thread_id: String,
        #[arg(long = "owner-product")]
        owner_product: String,
        #[arg(long = "owner-thread-id")]
        owner_thread_id: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
    },
    Unregister {
        #[arg(long)]
        cli: CliArg,
        #[arg(long = "native-thread-id")]
        native_thread_id: String,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum CliArg {
    Claude,
    Codex,
}

impl From<CliArg> for ThreadCli {
    fn from(c: CliArg) -> Self {
        match c {
            CliArg::Claude => ThreadCli::Claude,
            CliArg::Codex => ThreadCli::Codex,
        }
    }
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    let result = match cli.command {
        Cmd::Install {
            skip_claude,
            skip_codex,
            no_spawn,
            service_install,
            uninstall,
        } => {
            if uninstall {
                commands::uninstall::run(skip_claude, skip_codex)
            } else {
                commands::install::run(commands::install::InstallArgs {
                    skip_claude,
                    skip_codex,
                    no_spawn,
                    service_install,
                })
            }
        }
        Cmd::Daemon => commands::daemon::run_foreground(),
        Cmd::Status { all, json, ascii } | Cmd::Threads { all, json, ascii } => {
            commands::status::run(commands::status::StatusArgs { all, json, ascii })
        }
        Cmd::Watch { all, ascii, notify } => {
            commands::watch::run(commands::watch::WatchArgs { all, ascii, notify })
        }
        Cmd::Events { json, follow } => {
            commands::events::run(commands::events::EventsArgs { json, follow })
        }
        Cmd::Tui => heed::tui::run(),
        Cmd::Owner { action } => match action {
            OwnerCmd::Register {
                cli,
                native_thread_id,
                owner_product,
                owner_thread_id,
                cwd,
            } => commands::owner::register(commands::owner::RegisterArgs {
                cli: cli.into(),
                native_thread_id,
                owner_product,
                owner_thread_id,
                cwd,
            }),
            OwnerCmd::Unregister {
                cli,
                native_thread_id,
            } => commands::owner::unregister(commands::owner::UnregisterArgs {
                cli: cli.into(),
                native_thread_id,
            }),
        },
        Cmd::Version => commands::version::run(),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("heed: {e}");
            ExitCode::FAILURE
        }
    }
}
