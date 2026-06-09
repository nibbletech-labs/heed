//! `events.jsonl` tail-watcher.
//!
//! Watches `~/.heed/` via `notify` for Modify/Create on `events.jsonl`. On
//! each event we read new bytes from the saved offset, parse each JSON line,
//! and dispatch to the reducer. Survives log truncation/rotation: if the
//! file shrinks below our offset, we reset to 0.

use log::{debug, warn};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::state::HookEvent;

/// How often we drain the log even without a notify event. Belt-and-suspenders
/// for platforms (notably macOS fsevents) where notifications can lag or
/// coalesce. Cheap — a stat + (usually no-op) read.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Tail-watch `events.jsonl`, sending each parsed event to `tx`.
///
/// `initial_offset` is what byte we seek to on startup — typically the current
/// end-of-file so we ignore the backlog, but the daemon can pass 0 to replay
/// from the beginning (e.g., for the persisted-offset v0.2 feature).
///
/// The watcher runs until `tx` is dropped (i.e., the daemon shutdown drops
/// the receiver), at which point send errors break the loop.
pub struct EventWatcher {
    pub thread: JoinHandle<()>,
    /// Current byte offset into the event log. The daemon reads this when
    /// persisting `watcher.state` so a restart can resume cleanly.
    pub offset: Arc<AtomicU64>,
    /// Held internally so the OS-level watch is kept alive for as long as
    /// the daemon retains the `EventWatcher`.
    _watcher: RecommendedWatcher,
}

pub fn spawn(
    log_path: PathBuf,
    initial_offset: u64,
    tx: Sender<HookEvent>,
) -> Result<EventWatcher, String> {
    let watch_dir = log_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "log_path must have a parent".to_string())?;

    let (notify_tx, notify_rx) = std::sync::mpsc::channel::<notify::Event>();
    let mut watcher =
        notify::recommended_watcher(move |result: Result<notify::Event, notify::Error>| {
            if let Ok(ev) = result {
                let _ = notify_tx.send(ev);
            }
        })
        .map_err(|e| format!("notify init: {e}"))?;
    watcher
        .watch(&watch_dir, RecursiveMode::NonRecursive)
        .map_err(|e| format!("watch({watch_dir:?}): {e}"))?;

    let offset = Arc::new(AtomicU64::new(initial_offset));
    let offset_for_thread = offset.clone();
    let log_path_for_thread = log_path.clone();
    let thread = std::thread::Builder::new()
        .name("heed-watcher".into())
        .spawn(move || run_loop(&log_path_for_thread, offset_for_thread, notify_rx, tx))
        .map_err(|e| format!("spawn watcher thread: {e}"))?;

    Ok(EventWatcher {
        thread,
        offset,
        _watcher: watcher,
    })
}

fn run_loop(
    log_path: &Path,
    offset: Arc<AtomicU64>,
    notify_rx: std::sync::mpsc::Receiver<notify::Event>,
    tx: Sender<HookEvent>,
) {
    let mut local_offset = offset.load(Ordering::Relaxed);
    debug!("event watcher started (log={log_path:?}, offset={local_offset})");

    if drain(log_path, &mut local_offset, &tx).is_err() {
        return;
    }
    offset.store(local_offset, Ordering::Relaxed);

    while let Ok(_) | Err(RecvTimeoutError::Timeout) = notify_rx.recv_timeout(POLL_INTERVAL) {
        if drain(log_path, &mut local_offset, &tx).is_err() {
            break;
        }
        offset.store(local_offset, Ordering::Relaxed);
    }
}

fn drain(log_path: &Path, offset: &mut u64, tx: &Sender<HookEvent>) -> Result<(), ()> {
    let Ok(mut file) = fs::File::open(log_path) else {
        return Ok(());
    };
    let size = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return Ok(()),
    };
    if size < *offset {
        // File truncated/rotated — reset.
        *offset = 0;
    }
    if size == *offset {
        return Ok(());
    }
    if file.seek(SeekFrom::Start(*offset)).is_err() {
        return Ok(());
    }
    // Read the span as raw bytes and decode lossily. Reading as a UTF-8 string
    // (`read_to_string`) fails wholesale on a single invalid byte — and because
    // the early return left `*offset` un-advanced, every later append re-read
    // the same corrupt span and the tail wedged permanently (live-but-dead
    // daemon). Decoding lossily turns bad bytes into U+FFFD so the offending
    // line just fails JSON parse and is skipped, while the offset still moves on.
    let to_read = (size - *offset) as usize;
    let mut bytes = vec![0u8; to_read];
    if file.read_exact(&mut bytes).is_err() {
        // Short read (e.g. concurrent truncation): don't advance; the next
        // drain re-evaluates size (and the `size < *offset` reset recovers).
        return Ok(());
    }
    *offset = size;
    let buf = String::from_utf8_lossy(&bytes);

    for line in buf.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<HookEvent>(line) {
            Ok(ev) => {
                if tx.send(ev).is_err() {
                    return Err(());
                }
            }
            Err(e) => warn!("bad event line: {e} (line: {line})"),
        }
    }
    Ok(())
}

/// Return the current end-of-file byte offset for `log_path`, or 0 if missing.
/// Used at daemon startup so we ignore the backlog and only react to fresh events.
pub fn current_eof(log_path: &Path) -> u64 {
    fs::metadata(log_path).map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Cli, HookEventKind};
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::Duration;
    use tempfile::tempdir;

    fn write_event(file: &Path, line: &str) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
            .unwrap();
        writeln!(f, "{line}").unwrap();
        f.sync_all().unwrap();
    }

    fn append_raw(file: &Path, bytes: &[u8]) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
            .unwrap();
        f.write_all(bytes).unwrap();
        f.sync_all().unwrap();
    }

    #[test]
    fn current_eof_returns_zero_for_missing_file() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("events.jsonl");
        assert_eq!(current_eof(&p), 0);
    }

    #[test]
    fn current_eof_returns_file_size() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("events.jsonl");
        fs::write(&p, "abc\n").unwrap();
        assert_eq!(current_eof(&p), 4);
    }

    #[test]
    fn watcher_delivers_new_events_appended_after_start() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("events.jsonl");
        // Pre-existing line before start; should NOT be delivered (we start
        // at current_eof to ignore backlog).
        write_event(
            &log,
            r#"{"event":"turn_start","ts":1.0,"cli":"claude","thread_id":"backlog","pid":1,"pid_start":""}"#,
        );

        let (tx, rx) = mpsc::channel();
        let initial = current_eof(&log);
        let _w = spawn(log.clone(), initial, tx).unwrap();

        // Give notify time to set up.
        std::thread::sleep(Duration::from_millis(100));

        write_event(
            &log,
            r#"{"event":"turn_start","ts":2.0,"cli":"claude","thread_id":"new","pid":2,"pid_start":""}"#,
        );

        let ev = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("expected one event from the watcher");
        assert_eq!(ev.event, HookEventKind::TurnStart);
        assert_eq!(ev.cli, Cli::Claude);
        assert_eq!(ev.thread_id, "new");
        assert!(rx.try_recv().is_err(), "no other events should arrive");
    }

    #[test]
    fn watcher_skips_malformed_lines() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("events.jsonl");

        let (tx, rx) = mpsc::channel();
        let _w = spawn(log.clone(), 0, tx).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        write_event(&log, "this is not json");
        write_event(
            &log,
            r#"{"event":"turn_start","ts":3.0,"cli":"codex","thread_id":"good","pid":7,"pid_start":""}"#,
        );

        let ev = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("good event should arrive");
        assert_eq!(ev.thread_id, "good");
    }

    #[test]
    fn watcher_advances_past_invalid_utf8_and_keeps_delivering() {
        // Regression for the live-but-dead stall: a single invalid-UTF-8 byte in
        // events.jsonl used to wedge the tail forever (read_to_string errored and
        // the offset never advanced). The watcher must skip the bad bytes and
        // keep delivering subsequent valid events.
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("events.jsonl");

        let (tx, rx) = mpsc::channel();
        let _w = spawn(log.clone(), 0, tx).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        // A corrupt "line": invalid UTF-8 bytes terminated by a newline.
        append_raw(&log, &[0x7b, 0xff, 0xfe, 0x6f, 0x6f, b'\n']);
        write_event(
            &log,
            r#"{"event":"turn_start","ts":9.0,"cli":"codex","thread_id":"after-corruption","pid":9,"pid_start":""}"#,
        );

        let ev = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("event after the corrupt bytes must still be delivered");
        assert_eq!(ev.thread_id, "after-corruption");
    }

    #[test]
    fn watcher_replays_from_initial_offset_zero() {
        // initial_offset=0 → backlog IS delivered.
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("events.jsonl");
        write_event(
            &log,
            r#"{"event":"turn_start","ts":1.0,"cli":"claude","thread_id":"backlog","pid":1,"pid_start":""}"#,
        );

        let (tx, rx) = mpsc::channel();
        let _w = spawn(log.clone(), 0, tx).unwrap();

        let ev = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("backlog event should be delivered when starting from 0");
        assert_eq!(ev.thread_id, "backlog");
    }
}
