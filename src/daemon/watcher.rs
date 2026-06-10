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

/// How often the watcher checks whether the log is over the size cap. The
/// daemon also trims on startup, but a long-running daemon never restarts, so
/// without this the log grows without bound between restarts.
const TRUNCATE_CHECK_INTERVAL: Duration = Duration::from_secs(60);

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

    let mut last_truncate_check = std::time::Instant::now();
    while let Ok(_) | Err(RecvTimeoutError::Timeout) = notify_rx.recv_timeout(POLL_INTERVAL) {
        if drain(log_path, &mut local_offset, &tx).is_err() {
            break;
        }
        if last_truncate_check.elapsed() >= TRUNCATE_CHECK_INTERVAL {
            last_truncate_check = std::time::Instant::now();
            maybe_truncate(log_path, &mut local_offset);
        }
        offset.store(local_offset, Ordering::Relaxed);
    }
}

/// Trim the log if it is over the cap, then jump the offset to the new EOF.
/// Runs in the watcher thread — the sole owner of the offset — so the rewrite
/// can't race a concurrent reader into replaying the kept suffix (`drain` has
/// already consumed every kept line; they are a suffix of what we just read).
/// Hook appends between the trim's read and its rename are dropped, the same
/// ~ms window the startup trim accepts.
fn maybe_truncate(log_path: &Path, offset: &mut u64) {
    // Only trim when fully drained: after a short read `drain` leaves the
    // offset behind EOF, and jumping it forward would skip those events.
    if current_eof(log_path) != *offset {
        return;
    }
    match super::log::maybe_truncate_event_log(log_path) {
        Ok(true) => {
            *offset = current_eof(log_path);
            debug!("event log trimmed; offset reset to {offset}");
        }
        Ok(false) => {}
        Err(e) => warn!("periodic event log truncation failed: {e}"),
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
    fn maybe_truncate_trims_oversized_log_and_jumps_offset() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("events.jsonl");
        let line = format!("{}\n", "x".repeat(440));
        let mut buf = String::new();
        for _ in 0..7_000 {
            buf.push_str(&line);
        }
        fs::write(&log, &buf).unwrap();

        // Fully drained watcher → trims, offset lands on the new EOF.
        let mut offset = current_eof(&log);
        maybe_truncate(&log, &mut offset);
        let new_size = current_eof(&log);
        assert!(new_size < buf.len() as u64);
        assert_eq!(offset, new_size);
    }

    #[test]
    fn maybe_truncate_is_noop_when_not_fully_drained() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("events.jsonl");
        let line = format!("{}\n", "x".repeat(440));
        let mut buf = String::new();
        for _ in 0..7_000 {
            buf.push_str(&line);
        }
        fs::write(&log, &buf).unwrap();

        // Offset behind EOF (short read) → must not trim or move the offset.
        let mut offset = 10;
        maybe_truncate(&log, &mut offset);
        assert_eq!(offset, 10);
        assert_eq!(current_eof(&log), buf.len() as u64);
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
