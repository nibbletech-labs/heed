//! `heed watch` — re-render status every second until Ctrl-C.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::{
    cursor,
    terminal::{self, Clear, ClearType},
    ExecutableCommand,
};

use crate::commands::status::{self, StatusArgs};
use crate::state::Activity;

#[derive(Clone, Debug)]
pub struct WatchArgs {
    pub all: bool,
    pub ascii: bool,
    pub notify: bool,
}

pub fn run(args: WatchArgs) -> Result<(), String> {
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    let _ = ctrlc::try_set_handler(move || r.store(false, Ordering::SeqCst));

    let mut stdout = std::io::stdout();
    let _ = stdout.execute(terminal::EnterAlternateScreen);
    let _ = stdout.execute(cursor::Hide);

    let mut previously_awaiting: HashSet<String> = HashSet::new();
    while running.load(Ordering::SeqCst) {
        let _ = stdout.execute(Clear(ClearType::All));
        let _ = stdout.execute(cursor::MoveTo(0, 0));
        let _ = status::run(StatusArgs {
            all: args.all,
            json: false,
            ascii: args.ascii,
        });
        let _ = stdout.flush();

        if args.notify {
            check_notifications(&mut previously_awaiting);
        }

        for _ in 0..10 {
            if !running.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    let _ = stdout.execute(cursor::Show);
    let _ = stdout.execute(terminal::LeaveAlternateScreen);
    Ok(())
}

/// Detect threads that transitioned to `AwaitingInput` since the last tick
/// and fire a macOS notification for each one.
fn check_notifications(previously_awaiting: &mut HashSet<String>) {
    let home = match std::env::var("HOME").map(std::path::PathBuf::from) {
        Ok(h) => h,
        Err(_) => return,
    };
    let state_path = crate::install::state_path(&home);
    let Ok(raw) = std::fs::read_to_string(&state_path) else {
        return;
    };
    let Ok(state) = serde_json::from_str::<crate::daemon::state_writer::StateFile>(&raw) else {
        return;
    };

    let currently_awaiting: HashMap<String, &crate::state::ThreadState> = state
        .threads
        .iter()
        .filter(|(_, t)| t.activity == Activity::AwaitingInput)
        .map(|(k, v)| (k.clone(), v))
        .collect();

    for (key, thread) in &currently_awaiting {
        if !previously_awaiting.contains(key) {
            notify_macos(thread);
        }
    }
    *previously_awaiting = currently_awaiting.keys().cloned().collect();
}

#[cfg(target_os = "macos")]
fn notify_macos(thread: &crate::state::ThreadState) {
    let title = format!("heed · {} awaiting input", thread.cli);
    let body = thread
        .subtitle
        .clone()
        .unwrap_or_else(|| thread.thread_id.clone());
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(format!(
            "display notification {:?} with title {:?}",
            body, title
        ))
        .output();
}

#[cfg(not(target_os = "macos"))]
fn notify_macos(_thread: &crate::state::ThreadState) {
    // No-op on non-macOS for v0.2; libnotify integration is future work.
}
