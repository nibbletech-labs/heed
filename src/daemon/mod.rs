//! Daemon orchestrator: wires watcher + owners overlay + liveness poller +
//! reducer + state writer into a single foreground process.
//!
//! Lifecycle (SPEC §5 / §7 / §8.3):
//! 1. Trim event log if > 1 MiB.
//! 2. Spawn the events.jsonl watcher (starts at EOF — backlog ignored).
//! 3. Load owners.json overlay.
//! 4. Run the central reducer loop until SIGINT/SIGTERM.
//! 5. On each Hook event: run reducer, schedule a debounced state.json write.
//! 6. Every 30s: poll liveness for threads idle > 120s.
//! 7. After a TurnEnd: 200ms later, run transcript scan and resolve activity.

pub mod log;
pub mod owners;
pub mod persisted_offset;
pub mod state_writer;
pub mod watcher;

use ::log::{error, info, warn};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::liveness::{self, LivenessCheck};
use crate::state::{
    apply_event, initial_state, Activity, HookEvent, HookEventKind, Liveness, ThreadKey,
    ThreadState,
};
use crate::succession;
use crate::tool_display;
use crate::transcript;

/// How long between liveness polls.
const LIVENESS_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// A thread that fired an event within this window is treated as live without
/// polling — its event stream proves liveness.
const LIVENESS_EVENT_GRACE: Duration = Duration::from_secs(120);
/// State.json write debounce — coalesces a burst of events into one write.
const STATE_WRITE_DEBOUNCE: Duration = Duration::from_millis(100);
/// Delay between TurnEnd and the post-Stop transcript scan, giving Claude
/// time to flush the final assistant entry to disk.
const POST_STOP_SCAN_DELAY: Duration = Duration::from_millis(200);
/// How long a thread holding the owner overlay must stay hook-silent before a
/// successor session takes it over.
const OWNER_TAKEOVER_GRACE: Duration =
    Duration::from_secs(succession::OWNER_TAKEOVER_GRACE_SECS as u64);

#[derive(Clone, Debug)]
pub struct DaemonPaths {
    pub event_log: PathBuf,
    pub state_file: PathBuf,
    pub owners_file: PathBuf,
}

/// Tunable knobs (mostly so tests can override timings).
#[derive(Clone, Debug)]
pub struct DaemonConfig {
    /// Skip watcher / owners / liveness threads and just process one batch of
    /// pre-injected events. Used by integration tests.
    pub run_once: bool,
    pub liveness_poll_interval: Duration,
    pub liveness_event_grace: Duration,
    pub state_write_debounce: Duration,
    pub post_stop_scan_delay: Duration,
    pub owner_takeover_grace: Duration,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            run_once: false,
            liveness_poll_interval: LIVENESS_POLL_INTERVAL,
            liveness_event_grace: LIVENESS_EVENT_GRACE,
            state_write_debounce: STATE_WRITE_DEBOUNCE,
            post_stop_scan_delay: POST_STOP_SCAN_DELAY,
            owner_takeover_grace: OWNER_TAKEOVER_GRACE,
        }
    }
}

/// Events flowing into the central reducer loop. The reducer is single-threaded
/// (`run_loop`), so all state mutations serialize through this channel.
#[derive(Debug)]
pub enum DaemonEvent {
    Hook(HookEvent),
    /// owners.json changed; reducer re-loads and re-applies overlay.
    OwnersChanged,
    /// 30s tick: poll liveness for any thread idle > LIVENESS_EVENT_GRACE.
    LivenessTick,
    /// Triggered POST_STOP_SCAN_DELAY after a TurnEnd. Reducer reads the
    /// transcript and resolves activity.
    PostStopScan {
        key: ThreadKey,
    },
    /// SIGINT / SIGTERM received.
    Shutdown,
}

/// Run the daemon in the foreground. Returns when shutdown is requested.
pub fn run(paths: DaemonPaths) -> Result<(), String> {
    run_with_config(paths, DaemonConfig::default())
}

pub fn run_with_config(paths: DaemonPaths, cfg: DaemonConfig) -> Result<(), String> {
    // Step 1: trim if needed.
    if let Err(e) = log::maybe_truncate_event_log(&paths.event_log) {
        warn!("event log truncation failed: {e}");
    }

    // Step 1b: resolve persisted watcher offset.
    // Prefer the saved offset (so we don't miss events while the daemon was
    // down); fall back to current EOF if the log shrank below it (rotation).
    let home = paths
        .event_log
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf());
    let saved_offset = home
        .as_ref()
        .map(|h| persisted_offset::load(h).events_offset)
        .unwrap_or(0);
    let current_size = watcher::current_eof(&paths.event_log);
    let initial_offset = persisted_offset::effective_offset(saved_offset, current_size);

    let (tx, rx) = mpsc::channel::<DaemonEvent>();

    // Step 2: spawn event watcher (unless run_once).
    let watcher_handle = if cfg.run_once {
        None
    } else {
        let watcher_tx = tx.clone();
        let hook_tx = make_hook_sender(watcher_tx);
        Some(watcher::spawn(
            paths.event_log.clone(),
            initial_offset,
            hook_tx,
        )?)
    };
    let watcher_offset = watcher_handle.as_ref().map(|w| w.offset.clone());

    // Step 3: owners.json watcher (small inline watcher on the parent dir).
    let _owners_watcher = if cfg.run_once {
        None
    } else {
        Some(spawn_owners_watcher(&paths.owners_file, tx.clone())?)
    };

    // Liveness ticker.
    let liveness_running = Arc::new(AtomicBool::new(true));
    let _liveness_thread = if cfg.run_once {
        None
    } else {
        Some(spawn_liveness_ticker(
            tx.clone(),
            cfg.liveness_poll_interval,
            liveness_running.clone(),
        ))
    };

    // Shutdown signal.
    if !cfg.run_once {
        let shutdown_tx = tx.clone();
        if let Err(e) = ctrlc::try_set_handler(move || {
            let _ = shutdown_tx.send(DaemonEvent::Shutdown);
        }) {
            warn!("could not install Ctrl-C handler: {e}");
        }
    }

    let result = run_loop(
        tx.clone(),
        rx,
        &paths,
        &cfg,
        watcher_offset.clone(),
        home.as_deref(),
        watcher_handle.as_ref().map(|w| &w.thread),
    );
    liveness_running.store(false, Ordering::SeqCst);
    // Persist the final offset so the next daemon start can resume cleanly.
    if let (Some(home), Some(off)) = (home, watcher_offset) {
        let _ = persisted_offset::save(
            &home,
            &persisted_offset::WatcherState {
                events_offset: off.load(Ordering::Relaxed),
            },
        );
    }
    result
}

fn make_hook_sender(tx: Sender<DaemonEvent>) -> Sender<HookEvent> {
    // Bridge: HookEvent → DaemonEvent::Hook.
    let (hook_tx, hook_rx) = mpsc::channel::<HookEvent>();
    std::thread::Builder::new()
        .name("heed-hook-bridge".into())
        .spawn(move || {
            while let Ok(ev) = hook_rx.recv() {
                if tx.send(DaemonEvent::Hook(ev)).is_err() {
                    break;
                }
            }
        })
        .expect("spawn hook bridge");
    hook_tx
}

fn spawn_owners_watcher(
    owners_path: &Path,
    tx: Sender<DaemonEvent>,
) -> Result<notify::RecommendedWatcher, String> {
    use notify::{RecursiveMode, Watcher};
    let owners_path = owners_path.to_path_buf();
    let dir = owners_path
        .parent()
        .ok_or("owners.json has no parent")?
        .to_path_buf();

    let mut watcher =
        notify::recommended_watcher(move |result: Result<notify::Event, notify::Error>| {
            let Ok(ev) = result else { return };
            if !ev.paths.iter().any(|p| p == &owners_path) {
                return;
            }
            let _ = tx.send(DaemonEvent::OwnersChanged);
        })
        .map_err(|e| format!("owners watcher: {e}"))?;
    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .map_err(|e| format!("watch {dir:?}: {e}"))?;
    Ok(watcher)
}

fn spawn_liveness_ticker(
    tx: Sender<DaemonEvent>,
    interval: Duration,
    running: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("heed-liveness".into())
        .spawn(move || {
            while running.load(Ordering::SeqCst) {
                std::thread::sleep(interval);
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                if tx.send(DaemonEvent::LivenessTick).is_err() {
                    break;
                }
            }
        })
        .expect("spawn liveness ticker")
}

/// Core reducer loop. Exits on `DaemonEvent::Shutdown` or channel closure.
fn run_loop(
    self_tx: Sender<DaemonEvent>,
    rx: Receiver<DaemonEvent>,
    paths: &DaemonPaths,
    cfg: &DaemonConfig,
    watcher_offset: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    home: Option<&Path>,
    watcher_thread: Option<&JoinHandle<()>>,
) -> Result<(), String> {
    let mut threads: HashMap<ThreadKey, ThreadState> = HashMap::new();
    let mut owners = owners::load_or_quarantine(&paths.owners_file);
    // Used to age out succession decisions that rest on a thread's *absence*
    // from `threads`, which is meaningless in the first moments after startup.
    let started_at = state_writer::now_unix();

    // Apply initial owners overlay over an empty map (no-op, but consistent).
    apply_owners_overlay(&mut threads, &owners);

    // Initial write so consumers can poll state.json immediately.
    flush_state(&threads, &paths.state_file)?;

    let mut pending_write_at: Option<Instant> = None;

    loop {
        let now = Instant::now();
        let timeout = match pending_write_at {
            Some(t) if t > now => t.duration_since(now),
            Some(_) => Duration::from_millis(0),
            None => Duration::from_secs(60),
        };

        let recv_result = if cfg.run_once {
            rx.try_recv().map_err(|e| match e {
                mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
            })
        } else {
            rx.recv_timeout(timeout)
        };

        match recv_result {
            Ok(DaemonEvent::Hook(ev)) => {
                handle_hook(
                    ev,
                    &mut threads,
                    &mut owners,
                    &paths.owners_file,
                    started_at,
                    &self_tx,
                    cfg,
                );
                pending_write_at = Some(now + cfg.state_write_debounce);
            }
            Ok(DaemonEvent::OwnersChanged) => {
                // Quarantine a corrupt overlay rather than aborting the reload:
                // the overlay only *sets* tags (never clears), so an empty
                // result leaves already-applied owners on live threads intact,
                // and the next `heed owner register` writes a fresh file.
                owners = owners::load_or_quarantine(&paths.owners_file);
                apply_owners_overlay(&mut threads, &owners);
                pending_write_at = Some(now + cfg.state_write_debounce);
            }
            Ok(DaemonEvent::LivenessTick) => {
                if poll_liveness(&mut threads, cfg) {
                    pending_write_at = Some(now + cfg.state_write_debounce);
                }
            }
            Ok(DaemonEvent::PostStopScan { key }) => {
                if let Some(state) = threads.get(&key).cloned() {
                    if let Some(path) = state.transcript_path.clone() {
                        let result = transcript::scan_post_stop(Path::new(&path));
                        if let Some(s) = threads.get_mut(&key) {
                            *s = crate::state::resolve_post_stop(s.clone(), result);
                            s.subtitle = Some(tool_display::format_for_thread(s));
                            pending_write_at = Some(now + cfg.state_write_debounce);
                        }
                    } else if let Some(s) = threads.get_mut(&key) {
                        // No transcript bound (e.g., Codex unbound) → fall back to Idle,
                        // unless a pre_tool_use(AskUserQuestion) already flipped us to
                        // AwaitingInput between TurnEnd and the scan.
                        if s.activity != Activity::AwaitingInput {
                            s.activity = Activity::Idle;
                            s.subtitle = Some(tool_display::format_for_thread(s));
                            pending_write_at = Some(now + cfg.state_write_debounce);
                        }
                    }
                }
            }
            Ok(DaemonEvent::Shutdown) => {
                info!("daemon: shutdown requested");
                flush_state(&threads, &paths.state_file)?;
                persist_watcher_offset(home, watcher_offset.as_ref());
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                // Either a debounced write is due, or run_once is draining.
                if let Some(when) = pending_write_at {
                    if now >= when {
                        flush_state(&threads, &paths.state_file)?;
                        persist_watcher_offset(home, watcher_offset.as_ref());
                        pending_write_at = None;
                    }
                }
                // Watchdog: if the event-watcher thread has died (panic, or a
                // notify backend failure), no hook event can ever reach us
                // again — we'd be a live-but-dead daemon. Flush, persist the
                // offset, and exit non-zero so the launchd/systemd supervisor
                // (KeepAlive) restarts us; the fresh daemon resumes from the
                // persisted offset. Idle timeouts (≤60s) bound the detection
                // latency. run_once has no watcher thread, so this never fires.
                if watcher_died(watcher_thread) {
                    error!("daemon: event watcher thread exited unexpectedly; restarting");
                    flush_state(&threads, &paths.state_file)?;
                    persist_watcher_offset(home, watcher_offset.as_ref());
                    return Err("event watcher thread exited; restart for recovery".into());
                }
                if cfg.run_once {
                    // Pending state at the end of run_once still flushes.
                    if pending_write_at.is_none() {
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    if pending_write_at.is_some() {
        flush_state(&threads, &paths.state_file)?;
        persist_watcher_offset(home, watcher_offset.as_ref());
    }
    Ok(())
}

/// Watchdog predicate: true when a watcher thread was spawned but has since
/// finished. In normal operation the watcher only ends on shutdown (which
/// breaks the loop before this is checked), so a finished thread here means it
/// died unexpectedly. `None` (run_once / no watcher) is never "died".
fn watcher_died(watcher_thread: Option<&JoinHandle<()>>) -> bool {
    watcher_thread.map(JoinHandle::is_finished).unwrap_or(false)
}

fn persist_watcher_offset(
    home: Option<&Path>,
    watcher_offset: Option<&std::sync::Arc<std::sync::atomic::AtomicU64>>,
) {
    let (Some(home), Some(offset)) = (home, watcher_offset) else {
        return;
    };
    let state = persisted_offset::WatcherState {
        events_offset: offset.load(std::sync::atomic::Ordering::Relaxed),
    };
    let _ = persisted_offset::save(home, &state);
}

fn handle_hook(
    ev: HookEvent,
    threads: &mut HashMap<ThreadKey, ThreadState>,
    owners: &mut HashMap<ThreadKey, owners::OwnerRecord>,
    owners_file: &Path,
    started_at: f64,
    self_tx: &Sender<DaemonEvent>,
    cfg: &DaemonConfig,
) {
    let key = ev.key();

    let known = threads.contains_key(&key);
    let current = threads.remove(&key).unwrap_or_else(|| initial_state(&ev));
    let mut next = apply_event(current, &ev);

    // First sighting of a session id: decide whether it continues one we already
    // track (a forked or in-place-rotated session). This reads the live process,
    // so it has to happen now — once the predecessor exits, the link is gone.
    if !known {
        if let Some(parent_key) = succession::resolve_predecessor(&ev, threads, owners) {
            next.supersedes = Some(parent_key.1.clone());
            if let Some(parent) = threads.get_mut(&parent_key) {
                parent.superseded_by = Some(next.thread_id.clone());
                parent.subtitle = Some(tool_display::format_for_thread(parent));
            }
            info!("succession: {} continues {}", next.thread_id, parent_key.1);
        }
    }

    // Owner overlay.
    if let Some(rec) = owners.get(&key) {
        next.owner_product = rec.owner_product.clone();
        next.owner_thread_id = rec.owner_thread_id.clone();
        if let Some(cwd) = &rec.cwd {
            if !cwd.is_empty() {
                next.cwd = Some(cwd.clone());
            }
        }
    }

    next.subtitle = Some(tool_display::format_for_thread(&next));

    let needs_post_stop = matches!(ev.event, HookEventKind::TurnEnd);
    threads.insert(key.clone(), next);

    // Re-checked on every event, not just the first: at the moment a session
    // forks, its predecessor has only just stopped emitting, so the handoff is
    // rarely provable yet.
    transfer_owner_to_successor(&key, threads, owners, owners_file, started_at, cfg);

    if needs_post_stop {
        let tx = self_tx.clone();
        let delay = cfg.post_stop_scan_delay;
        std::thread::Builder::new()
            .name("heed-post-stop".into())
            .spawn(move || {
                std::thread::sleep(delay);
                let _ = tx.send(DaemonEvent::PostStopScan { key });
            })
            .expect("spawn post-stop thread");
    }
}

/// Hand the owner overlay to `key` when it has superseded the thread currently
/// holding it. Ownership *moves*: a product's thread id must name exactly one
/// native session, or a consumer diffing state.json would flap between the
/// superseded record's frozen activity and the live one's.
fn transfer_owner_to_successor(
    key: &ThreadKey,
    threads: &mut HashMap<ThreadKey, ThreadState>,
    owners: &mut HashMap<ThreadKey, owners::OwnerRecord>,
    owners_file: &Path,
    started_at: f64,
    cfg: &DaemonConfig,
) {
    let Some(child) = threads.get(key) else {
        return;
    };
    let Some(parent_id) = child.supersedes.clone() else {
        return;
    };
    let parent_key: ThreadKey = (child.cli, parent_id);
    let now = state_writer::now_unix();
    let grace = cfg.owner_takeover_grace.as_secs_f64();

    let record = match threads.get(&parent_key) {
        Some(parent) => {
            if !succession::should_take_ownership(parent, child, now, grace) {
                return;
            }
            owners::OwnerRecord {
                owner_product: parent.owner_product.clone(),
                owner_thread_id: parent.owner_thread_id.clone(),
                cwd: child.cwd.clone().or_else(|| parent.cwd.clone()),
            }
        }
        // The predecessor isn't tracked at all, so it hasn't emitted since this
        // daemon started and cannot be the live end of the conversation. Its
        // overlay is still the persistent record of who owns the thread. Wait
        // out the grace from startup first, so a daemon that restarted while a
        // session was mid-turn doesn't mistake it for an abandoned one.
        None => {
            let Some(rec) = owners.get(&parent_key) else {
                return;
            };
            if now - started_at < grace {
                return;
            }
            owners::OwnerRecord {
                owner_product: rec.owner_product.clone(),
                owner_thread_id: rec.owner_thread_id.clone(),
                cwd: child.cwd.clone().or_else(|| rec.cwd.clone()),
            }
        }
    };
    if record.owner_product.is_none() && record.owner_thread_id.is_none() {
        return;
    }
    if let Err(e) = owners::transfer(owners_file, key.0, &parent_key.1, &key.1, record.clone()) {
        warn!("succession: could not move owner overlay to {}: {e}", key.1);
        return;
    }
    info!(
        "succession: owner {} moved from {} to {}",
        record.owner_thread_id.as_deref().unwrap_or("?"),
        parent_key.1,
        key.1
    );

    // Mirror the write into the in-memory overlay so an event arriving before
    // the owners.json watcher fires can't re-tag the predecessor.
    owners.remove(&parent_key);
    owners.insert(key.clone(), record.clone());

    if let Some(parent) = threads.get_mut(&parent_key) {
        parent.owner_product = None;
        parent.owner_thread_id = None;
        parent.subtitle = Some(tool_display::format_for_thread(parent));
    }
    if let Some(child) = threads.get_mut(key) {
        child.owner_product = record.owner_product;
        child.owner_thread_id = record.owner_thread_id;
        child.subtitle = Some(tool_display::format_for_thread(child));
    }
}

fn apply_owners_overlay(
    threads: &mut HashMap<ThreadKey, ThreadState>,
    owners: &HashMap<ThreadKey, owners::OwnerRecord>,
) {
    for (key, rec) in owners.iter() {
        if let Some(s) = threads.get_mut(key) {
            s.owner_product = rec.owner_product.clone();
            s.owner_thread_id = rec.owner_thread_id.clone();
            if let Some(cwd) = &rec.cwd {
                if !cwd.is_empty() {
                    s.cwd = Some(cwd.clone());
                }
            }
            s.subtitle = Some(tool_display::format_for_thread(s));
        }
    }
}

/// Poll liveness for threads whose last_event is stale. Returns true if any
/// thread changed state.
fn poll_liveness(threads: &mut HashMap<ThreadKey, ThreadState>, cfg: &DaemonConfig) -> bool {
    let now_unix = state_writer::now_unix();
    let mut changed = false;
    for state in threads.values_mut() {
        if state.liveness == Liveness::Gone {
            continue;
        }
        let event_age = Duration::from_secs_f64((now_unix - state.last_event).max(0.0));
        if event_age < cfg.liveness_event_grace {
            continue;
        }
        let result = liveness::check(state.pid, &state.pid_start);
        state.last_check = now_unix;
        if matches!(result, LivenessCheck::Gone) && state.liveness != Liveness::Gone {
            state.liveness = Liveness::Gone;
            // A dead thread isn't working — don't leave activity frozen mid-turn.
            state.activity = Activity::Idle;
            state.subtitle = Some(tool_display::format_for_thread(state));
            changed = true;
        }
    }
    changed
}

fn flush_state(threads: &HashMap<ThreadKey, ThreadState>, state_path: &Path) -> Result<(), String> {
    let mut by_key = HashMap::new();
    for ((cli, tid), state) in threads.iter() {
        by_key.insert(format!("{cli}:{tid}"), state.clone());
    }
    state_writer::write(state_path, &state_writer::build_state_file(by_key))
}

/// Process events from a slice without spawning any threads. Used by tests
/// and as a building block for non-daemon CLI paths. Deliberately skips
/// succession: linking a session to the one it continues reads the live
/// process, which a replay of historical events can't do.
pub fn run_events_in_memory(
    events: Vec<HookEvent>,
    owners: HashMap<ThreadKey, owners::OwnerRecord>,
) -> HashMap<ThreadKey, ThreadState> {
    let mut threads: HashMap<ThreadKey, ThreadState> = HashMap::new();
    for ev in events {
        let key = ev.key();
        let current = threads.remove(&key).unwrap_or_else(|| initial_state(&ev));
        let mut next = apply_event(current, &ev);
        if let Some(rec) = owners.get(&key) {
            next.owner_product = rec.owner_product.clone();
            next.owner_thread_id = rec.owner_thread_id.clone();
            if let Some(c) = &rec.cwd {
                if !c.is_empty() {
                    next.cwd = Some(c.clone());
                }
            }
        }
        next.subtitle = Some(tool_display::format_for_thread(&next));
        threads.insert(key, next);
    }
    threads
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Cli, HookEvent, HookEventExtra, HookEventKind};
    use tempfile::tempdir;

    fn ev(kind: HookEventKind, ts: f64, cli: Cli, thread: &str, tool: Option<&str>) -> HookEvent {
        HookEvent {
            event: kind,
            ts,
            cli,
            thread_id: thread.into(),
            pid: 1234,
            pid_start: "stable".into(),
            cwd: None,
            transcript_path: None,
            extra: HookEventExtra {
                tool_name: tool.map(str::to_string),
                ..Default::default()
            },
        }
    }

    /// A session rotating its id in place must carry its product ownership
    /// across, or the consumer keeps watching the abandoned id and shows the
    /// thread idle while the work continues.
    #[test]
    fn successor_takes_over_the_owner_overlay() {
        const PARENT: &str = "c7df16c4-a60a-4f11-a3ec-3afd5e15aaed";
        const CHILD: &str = "e033bfc1-a5dc-4fa4-806b-4040d1e9716f";
        let dir = tempdir().unwrap();
        let owners_file = dir.path().join("owners.json");
        let record = owners::OwnerRecord {
            owner_product: Some("codezilla".into()),
            owner_thread_id: Some("cz-48".into()),
            cwd: Some("/repo".into()),
        };
        owners::register(&owners_file, Cli::Claude, PARENT, record.clone()).unwrap();

        let mut threads = HashMap::new();
        let mut owner_map = owners::load(&owners_file).unwrap();
        let (tx, _rx) = mpsc::channel();
        let cfg = DaemonConfig::default();

        // Same pid and start time, new session id — an in-place rotation.
        for (ts, id) in [(1.0, PARENT), (2.0, CHILD)] {
            handle_hook(
                ev(HookEventKind::ToolUse, ts, Cli::Claude, id, Some("Bash")),
                &mut threads,
                &mut owner_map,
                &owners_file,
                0.0,
                &tx,
                &cfg,
            );
        }

        let parent = &threads[&(Cli::Claude, PARENT.to_string())];
        let child = &threads[&(Cli::Claude, CHILD.to_string())];
        assert_eq!(parent.superseded_by.as_deref(), Some(CHILD));
        assert_eq!(child.supersedes.as_deref(), Some(PARENT));
        assert_eq!(child.owner_thread_id.as_deref(), Some("cz-48"));
        assert_eq!(child.owner_product.as_deref(), Some("codezilla"));
        assert!(
            parent.owner_thread_id.is_none() && parent.owner_product.is_none(),
            "ownership must move, not duplicate"
        );

        let on_disk = owners::load(&owners_file).unwrap();
        assert!(!on_disk.contains_key(&(Cli::Claude, PARENT.to_string())));
        assert_eq!(
            on_disk[&(Cli::Claude, CHILD.to_string())]
                .owner_thread_id
                .as_deref(),
            Some("cz-48")
        );
    }

    /// After a restart the predecessor is no longer in memory, but its overlay
    /// persists in owners.json. A handoff that happened while the daemon was
    /// down has to stay recoverable, or the thread is stranded for good.
    #[test]
    fn successor_claims_ownership_from_an_untracked_predecessor() {
        const PARENT: &str = "c7df16c4-a60a-4f11-a3ec-3afd5e15aaed";
        const CHILD: &str = "e033bfc1-a5dc-4fa4-806b-4040d1e9716f";
        let dir = tempdir().unwrap();
        let owners_file = dir.path().join("owners.json");
        owners::register(
            &owners_file,
            Cli::Claude,
            PARENT,
            owners::OwnerRecord {
                owner_product: Some("codezilla".into()),
                owner_thread_id: Some("cz-48".into()),
                cwd: Some("/repo".into()),
            },
        )
        .unwrap();
        let mut owner_map = owners::load(&owners_file).unwrap();

        let mut threads = HashMap::new();
        let key = (Cli::Claude, CHILD.to_string());
        let mut child = initial_state(&ev(
            HookEventKind::ToolUse,
            10.0,
            Cli::Claude,
            CHILD,
            Some("Bash"),
        ));
        child.supersedes = Some(PARENT.to_string());
        threads.insert(key.clone(), child);

        let cfg = DaemonConfig::default();
        // Startup grace not yet elapsed: the predecessor's silence proves
        // nothing this soon after boot.
        transfer_owner_to_successor(
            &key,
            &mut threads,
            &mut owner_map,
            &owners_file,
            state_writer::now_unix(),
            &cfg,
        );
        assert!(threads[&key].owner_thread_id.is_none());

        // Long enough after startup, the silence is meaningful.
        transfer_owner_to_successor(&key, &mut threads, &mut owner_map, &owners_file, 0.0, &cfg);
        assert_eq!(threads[&key].owner_thread_id.as_deref(), Some("cz-48"));
        assert_eq!(threads[&key].owner_product.as_deref(), Some("codezilla"));

        let on_disk = owners::load(&owners_file).unwrap();
        assert!(!on_disk.contains_key(&(Cli::Claude, PARENT.to_string())));
        assert!(on_disk.contains_key(&key));
    }

    /// An unowned session that rotates gets linked, but there is no overlay to
    /// move and nothing is invented for it.
    #[test]
    fn succession_without_ownership_links_but_transfers_nothing() {
        let dir = tempdir().unwrap();
        let owners_file = dir.path().join("owners.json");
        let mut threads = HashMap::new();
        let mut owner_map = HashMap::new();
        let (tx, _rx) = mpsc::channel();
        let cfg = DaemonConfig::default();

        for (ts, id) in [(1.0, "aaa"), (2.0, "bbb")] {
            handle_hook(
                ev(HookEventKind::ToolUse, ts, Cli::Claude, id, Some("Bash")),
                &mut threads,
                &mut owner_map,
                &owners_file,
                0.0,
                &tx,
                &cfg,
            );
        }

        assert_eq!(
            threads[&(Cli::Claude, "bbb".to_string())]
                .supersedes
                .as_deref(),
            Some("aaa")
        );
        assert!(threads[&(Cli::Claude, "bbb".to_string())]
            .owner_thread_id
            .is_none());
        assert!(
            !owners_file.exists(),
            "no overlay write for an unowned lineage"
        );
    }

    #[test]
    fn run_events_in_memory_applies_reducer_and_subtitle() {
        let events = vec![
            ev(HookEventKind::TurnStart, 1.0, Cli::Claude, "a", None),
            ev(
                HookEventKind::PreToolUse,
                2.0,
                Cli::Claude,
                "a",
                Some("AskUserQuestion"),
            ),
        ];
        let threads = run_events_in_memory(events, HashMap::new());
        let s = threads.get(&(Cli::Claude, "a".into())).unwrap();
        assert_eq!(s.activity, Activity::AwaitingInput);
        assert_eq!(s.subtitle.as_deref(), Some("Awaiting input"));
    }

    #[test]
    fn owner_overlay_propagates_to_threads() {
        let events = vec![ev(HookEventKind::TurnStart, 1.0, Cli::Claude, "a", None)];
        let mut owners = HashMap::new();
        owners.insert(
            (Cli::Claude, "a".to_string()),
            owners::OwnerRecord {
                owner_product: Some("codezilla".into()),
                owner_thread_id: Some("t-1".into()),
                cwd: Some("/owned".into()),
            },
        );
        let threads = run_events_in_memory(events, owners);
        let s = threads.get(&(Cli::Claude, "a".into())).unwrap();
        assert_eq!(s.owner_product.as_deref(), Some("codezilla"));
        assert_eq!(s.cwd.as_deref(), Some("/owned"));
    }

    #[test]
    fn run_once_processes_pending_then_exits() {
        // Run the daemon in run_once mode, with no event log changes. It
        // should write an initial empty state.json and exit promptly.
        let tmp = tempdir().unwrap();
        let event_log = tmp.path().join("events.jsonl");
        std::fs::write(&event_log, "").unwrap();
        let state_file = tmp.path().join("state.json");
        let owners_file = tmp.path().join("owners.json");

        let paths = DaemonPaths {
            event_log,
            state_file: state_file.clone(),
            owners_file,
        };
        let cfg = DaemonConfig {
            run_once: true,
            ..Default::default()
        };
        run_with_config(paths, cfg).unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&state_file).unwrap()).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert!(parsed["threads"].as_object().unwrap().is_empty());
    }

    #[test]
    fn watcher_died_true_only_for_a_finished_thread() {
        // A thread that has returned → watchdog should report it dead.
        let finished = std::thread::spawn(|| {});
        while !finished.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(watcher_died(Some(&finished)));

        // A thread still blocked on a channel → alive.
        let (keepalive_tx, keepalive_rx) = mpsc::channel::<()>();
        let running = std::thread::spawn(move || {
            let _ = keepalive_rx.recv();
        });
        assert!(!watcher_died(Some(&running)));

        // No watcher (run_once) → never "died".
        assert!(!watcher_died(None));

        drop(keepalive_tx);
        let _ = running.join();
    }

    #[test]
    fn poll_liveness_marks_dead_pid_gone() {
        let mut threads = HashMap::new();
        let ev = ev(HookEventKind::TurnStart, 0.0, Cli::Claude, "a", None);
        let mut state = initial_state(&ev);
        state.pid = 4_000_000; // far above any plausible PID
        state.pid_start = "Mon Jan 1 00:00:00 1970".into();
        // Make last_event ancient so the grace check passes.
        state.last_event = 0.0;
        threads.insert((Cli::Claude, "a".into()), state);

        let cfg = DaemonConfig {
            liveness_event_grace: Duration::from_secs(0),
            ..Default::default()
        };
        let changed = poll_liveness(&mut threads, &cfg);
        assert!(changed);
        assert_eq!(threads[&(Cli::Claude, "a".into())].liveness, Liveness::Gone);
    }
}
