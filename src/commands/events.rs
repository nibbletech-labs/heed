//! `heed events` — tail `~/.heed/events.jsonl`.

use std::io::Write;

#[derive(Clone, Debug)]
pub struct EventsArgs {
    pub json: bool,
    pub follow: bool,
}

pub fn run(args: EventsArgs) -> Result<(), String> {
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .map_err(|_| "HOME env var not set".to_string())?;
    let log_path = crate::install::event_log_path(&home);

    if !log_path.exists() {
        println!("heed: no event log yet at {}.", log_path.display());
        return Ok(());
    }

    // Print existing tail (last 100 lines).
    let raw = std::fs::read_to_string(&log_path).map_err(|e| format!("read: {e}"))?;
    let mut lines: Vec<&str> = raw.lines().collect();
    if lines.len() > 100 {
        lines = lines[lines.len() - 100..].to_vec();
    }
    for line in &lines {
        print_line(line, args.json);
    }
    if !args.follow {
        return Ok(());
    }

    // Follow.
    let (tx, rx) = std::sync::mpsc::channel();
    let initial_offset = crate::daemon::watcher::current_eof(&log_path);
    let _w = crate::daemon::watcher::spawn(log_path, initial_offset, tx)?;
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = running.clone();
    let _ = ctrlc::try_set_handler(move || r.store(false, std::sync::atomic::Ordering::SeqCst));

    while running.load(std::sync::atomic::Ordering::SeqCst) {
        match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(ev) => {
                // We have a parsed HookEvent — re-serialize as JSON or pretty.
                if args.json {
                    println!("{}", serde_json::to_string(&ev).unwrap_or_default());
                } else {
                    println!(
                        "{:>14.3}  {:>13}  {:<12}  {}",
                        ev.ts,
                        format!("{:?}", ev.event).to_lowercase(),
                        format!("{}:{}", ev.cli, short_id(&ev.thread_id)),
                        ev.tool_name().unwrap_or("")
                    );
                }
                let _ = std::io::stdout().flush();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    Ok(())
}

fn print_line(line: &str, json: bool) {
    if json {
        println!("{line}");
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        println!("(malformed) {line}");
        return;
    };
    println!(
        "{:>14.3}  {:>13}  {:<12}  {}",
        v["ts"].as_f64().unwrap_or(0.0),
        v["event"].as_str().unwrap_or(""),
        format!(
            "{}:{}",
            v["cli"].as_str().unwrap_or(""),
            short_id(v["thread_id"].as_str().unwrap_or(""))
        ),
        v.get("extra")
            .and_then(|e| e.get("tool_name"))
            .and_then(|t| t.as_str())
            .unwrap_or(""),
    );
}

fn short_id(id: &str) -> String {
    let head = id.split('-').next().unwrap_or(id);
    if head.len() >= 8 {
        head[..8].to_string()
    } else {
        head.to_string()
    }
}
