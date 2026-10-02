//! Session succession — linking a native CLI session to the one it continues.
//!
//! Heed keys a thread by the CLI's native session UUID (SPEC §4.1). That holds
//! until a CLI rotates its session id part-way through a conversation, which
//! Claude Code does when a session is forked into its background daemon
//! (`--session-id <new> --fork-session --resume <old>.jsonl`) or restarted
//! in-process. The conversation carries on, but the hook stream moves to a new
//! id — so a consumer watching the old id sees it fall silent forever while the
//! work continues somewhere it cannot see.
//!
//! Two deterministic signals resolve the link, both available at the successor's
//! first hook event:
//!
//! 1. **Same process, new session id.** Every hook event carries the CLI's pid
//!    and that process's start time. A new session reporting a `(pid,
//!    pid_start)` pair that already belongs to a tracked thread *is* that
//!    thread, continued in-process — unless that process is a shared session
//!    host (Codex's `app-server` daemon), where every session shares one pid.
//! 2. **`--resume <parent>` in the successor's argv.** A forked session names
//!    the transcript it continues; the file stem is the predecessor's session
//!    UUID. `claude --resume <uuid>` (no path) is accepted in the same way.
//!
//! Walking the process ancestry is deliberately *not* a signal. A background
//! session spawned from a tracked session shares its ancestry without
//! continuing it — a sibling, not a successor — and ancestry alone cannot tell
//! the two apart. `--resume` names the predecessor outright, so it can.
//!
//! Linking is separate from *ownership*. The owner overlay only moves once the
//! predecessor is demonstrably no longer the live end of the conversation (see
//! [`should_take_ownership`]), because a session may legitimately fork a
//! sibling and keep working itself.

use std::collections::HashMap;
use std::process::Command;

use crate::state::{HookEvent, Liveness, ThreadKey, ThreadState};

/// How long the overlay holder must stay hook-silent before a successor takes
/// ownership. Matches the liveness event grace: a thread that hasn't emitted
/// for this long is no longer demonstrably active.
pub const OWNER_TAKEOVER_GRACE_SECS: f64 = 120.0;

/// Find the thread that `ev`'s session continues, if any.
///
/// A predecessor counts if it is currently tracked *or* if it still holds an
/// owner overlay: the daemon's thread map is rebuilt from scratch on restart,
/// but `owners.json` persists, so a session that was already superseded before
/// the daemon came up can still be recognised and released.
///
/// `threads` must not contain `ev`'s own key (the daemon removes it before
/// calling); self-links are rejected regardless.
pub fn resolve_predecessor<T>(
    ev: &HookEvent,
    threads: &HashMap<ThreadKey, ThreadState>,
    owned: &HashMap<ThreadKey, T>,
) -> Option<ThreadKey> {
    predecessor_given_argv(ev, process_argv(ev.pid).as_deref(), threads, owned)
}

/// [`resolve_predecessor`] with the hook process's argv already read.
fn predecessor_given_argv<T>(
    ev: &HookEvent,
    argv: Option<&str>,
    threads: &HashMap<ThreadKey, ThreadState>,
    owned: &HashMap<ThreadKey, T>,
) -> Option<ThreadKey> {
    let same_process = if pid_identifies_session(ev.cli, argv) {
        same_process_predecessor(ev, threads)
    } else {
        None
    };
    same_process.or_else(|| resumed_predecessor(ev, argv?, threads, owned))
}

/// A failed process lookup is not evidence that a Codex host is dedicated to
/// one session. Claude's in-process rotation remains usable without argv.
fn pid_identifies_session(cli: crate::state::Cli, argv: Option<&str>) -> bool {
    !argv.is_some_and(is_shared_session_host) && (cli != crate::state::Cli::Codex || argv.is_some())
}

/// An older daemon may have persisted false succession links between Codex
/// sessions on one shared host. Keeping those links would still transfer
/// ownership on the next hook even after new linking has been fixed.
pub(crate) fn discard_untrusted_restored_links(state: &mut ThreadState) {
    discard_links_given_argv(state, process_argv(state.pid).as_deref());
}

fn discard_links_given_argv(state: &mut ThreadState, argv: Option<&str>) {
    if state.cli == crate::state::Cli::Codex && !pid_identifies_session(state.cli, argv) {
        state.supersedes = None;
        state.superseded_by = None;
    }
}

/// Whether `argv` is a process that hosts many independent sessions at once,
/// so sharing it says nothing about one session continuing another. Codex
/// runs every session — each terminal's, and every agent's — inside one
/// long-lived `codex app-server` daemon, and its hooks report that daemon as
/// their process.
fn is_shared_session_host(argv: &str) -> bool {
    let mut tokens = argv.split_whitespace();
    let is_codex = tokens
        .next()
        .and_then(|exe| exe.rsplit('/').next())
        .is_some_and(|name| name == "codex");
    is_codex && tokens.any(|t| t == "app-server")
}

/// A tracked thread on the same process instance — the session was rotated
/// without respawning the CLI. Requires a recorded `pid_start` on both sides:
/// pid alone would mislink after PID reuse.
fn same_process_predecessor(
    ev: &HookEvent,
    threads: &HashMap<ThreadKey, ThreadState>,
) -> Option<ThreadKey> {
    if ev.pid == 0 || ev.pid_start.is_empty() {
        return None;
    }
    threads
        .iter()
        .find(|((cli, tid), s)| {
            *cli == ev.cli
                && tid.as_str() != ev.thread_id
                // Agent records share their session's process; only a session
                // can be the one a new session continues.
                && s.kind == crate::state::NodeKind::Session
                && s.pid == ev.pid
                && s.pid_start == ev.pid_start
                // Already handed on to a later session — link to the tip, not
                // to a spent link in the chain.
                && s.superseded_by.is_none()
        })
        .map(|(k, _)| k.clone())
}

/// A thread named by `--resume` in the successor's argv.
fn resumed_predecessor<T>(
    ev: &HookEvent,
    argv: &str,
    threads: &HashMap<ThreadKey, ThreadState>,
    owned: &HashMap<ThreadKey, T>,
) -> Option<ThreadKey> {
    let parent = parse_resumed_session_id(argv)?;
    if parent == ev.thread_id {
        return None;
    }
    let key = (ev.cli, parent);
    (threads.contains_key(&key) || owned.contains_key(&key)).then_some(key)
}

/// Extract the session UUID a command line resumes: the value after `--resume`
/// (or `--resume=<value>`), taken as a path stem when it names a transcript.
/// Returns `None` when the flag is absent or its value isn't a UUID.
pub fn parse_resumed_session_id(argv: &str) -> Option<String> {
    let mut tokens = argv.split_whitespace();
    while let Some(tok) = tokens.next() {
        let value = if let Some(v) = tok.strip_prefix("--resume=") {
            v
        } else if tok == "--resume" {
            tokens.next()?
        } else {
            continue;
        };
        let stem = value.rsplit('/').next().unwrap_or(value);
        let stem = stem.strip_suffix(".jsonl").unwrap_or(stem);
        return looks_like_uuid(stem).then(|| stem.to_string());
    }
    None
}

fn looks_like_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    s.bytes().enumerate().all(|(i, b)| match i {
        8 | 13 | 18 | 23 => b == b'-',
        _ => b.is_ascii_hexdigit(),
    })
}

/// Read a process's full argv. `-ww` disables the width truncation `ps` would
/// otherwise apply, so a long argv still yields its `--resume` value.
fn process_argv(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let out = Command::new("ps")
        .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let argv = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!argv.is_empty()).then_some(argv)
}

/// Whether `child` should take over the owner overlay currently held by its
/// predecessor.
///
/// Immediate when the predecessor cannot still be running the conversation —
/// its process is gone, or it *is* this process (a session rotated in place).
/// Otherwise the predecessor must have fallen silent for `grace_secs` while the
/// successor went on emitting, which is what separates a genuine handoff from a
/// session that forked a sibling and kept working.
pub fn should_take_ownership(
    parent: &ThreadState,
    child: &ThreadState,
    now: f64,
    grace_secs: f64,
) -> bool {
    if parent
        .superseded_by
        .as_deref()
        .is_some_and(|s| s != child.thread_id)
    {
        return false;
    }
    if parent.owner_product.is_none() && parent.owner_thread_id.is_none() {
        return false;
    }
    if parent.liveness == Liveness::Gone {
        return true;
    }
    if parent.pid == child.pid && !child.pid_start.is_empty() && parent.pid_start == child.pid_start
    {
        return true;
    }
    now - parent.last_event >= grace_secs && child.last_event > parent.last_event
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Activity, Cli, HookEventKind};
    use std::collections::VecDeque;

    /// No persistent overlay to fall back on — link only what's tracked.
    fn no_owners() -> HashMap<ThreadKey, ()> {
        HashMap::new()
    }

    fn thread(id: &str, pid: u32, pid_start: &str) -> ThreadState {
        ThreadState {
            thread_id: id.to_string(),
            cli: Cli::Claude,
            activity: Activity::Idle,
            liveness: Liveness::Live,
            first_seen: 100.0,
            last_event: 100.0,
            last_check: 100.0,
            pid,
            pid_start: pid_start.to_string(),
            in_plan_mode: false,
            plan_progress: None,
            last_tool_name: None,
            last_tool_target: None,
            subtitle: None,
            cwd: None,
            transcript_path: None,
            owner_product: Some("codezilla".into()),
            owner_thread_id: Some("cz-1".into()),
            supersedes: None,
            superseded_by: None,
            recent_events: VecDeque::new(),
            kind: crate::state::NodeKind::Session,
            agent_id: None,
            agent_type: None,
            parent_thread_id: None,
            agent_description: None,
            agent_name: None,
            agent_color: None,
            own_activity: None,
            agents_active: 0,
            spawned_by: None,
        }
    }

    fn event(id: &str, pid: u32, pid_start: &str) -> HookEvent {
        HookEvent {
            event: HookEventKind::ToolUse,
            ts: 200.0,
            cli: Cli::Claude,
            thread_id: id.to_string(),
            pid,
            pid_start: pid_start.to_string(),
            cwd: None,
            transcript_path: None,
            agent_id: None,
            agent_type: None,
            spawned_by: None,
            extra: Default::default(),
        }
    }

    const UUID_A: &str = "c7df16c4-a60a-4f11-a3ec-3afd5e15aaed";
    const UUID_B: &str = "e033bfc1-a5dc-4fa4-806b-4040d1e9716f";
    const UUID_C: &str = "b43f485a-468e-4d85-b6a6-9e1fe415f003";

    #[test]
    fn parses_resume_transcript_path() {
        let argv = format!(
            "/path/claude --session-id {UUID_B} --fork-session --resume \
             /Users/t/.claude/projects/-p/{UUID_A}.jsonl --permission-mode auto"
        );
        assert_eq!(parse_resumed_session_id(&argv), Some(UUID_A.to_string()));
    }

    #[test]
    fn parses_bare_resume_uuid_and_equals_form() {
        assert_eq!(
            parse_resumed_session_id(&format!("claude --resume {UUID_A}")),
            Some(UUID_A.to_string())
        );
        assert_eq!(
            parse_resumed_session_id(&format!("claude --resume={UUID_A}")),
            Some(UUID_A.to_string())
        );
    }

    #[test]
    fn ignores_missing_or_non_uuid_resume() {
        assert_eq!(parse_resumed_session_id("claude --session-id abc"), None);
        assert_eq!(parse_resumed_session_id("claude --resume latest"), None);
        assert_eq!(parse_resumed_session_id("claude --resume"), None);
    }

    #[test]
    fn same_process_new_session_id_is_a_successor() {
        let mut threads = HashMap::new();
        threads.insert(
            (Cli::Claude, UUID_A.to_string()),
            thread(UUID_A, 13483, "Wed Aug 19 17:03:32 2026"),
        );
        let ev = event(UUID_B, 13483, "Wed Aug 19 17:03:32 2026");
        assert_eq!(
            resolve_predecessor(&ev, &threads, &no_owners()),
            Some((Cli::Claude, UUID_A.to_string()))
        );
    }

    #[test]
    fn pid_reuse_does_not_link() {
        let mut threads = HashMap::new();
        threads.insert(
            (Cli::Claude, UUID_A.to_string()),
            thread(UUID_A, 13483, "Wed Aug 19 17:03:32 2026"),
        );
        // Same pid, different start time — a recycled pid, not the same process.
        let ev = event(UUID_B, 13483, "Wed Aug 19 19:41:07 2026");
        assert_eq!(resolve_predecessor(&ev, &threads, &no_owners()), None);
    }

    #[test]
    fn spent_links_are_skipped_for_the_tip() {
        let mut spent = thread(UUID_A, 13483, "start");
        spent.superseded_by = Some("mid".into());
        let mut threads = HashMap::new();
        threads.insert((Cli::Claude, UUID_A.to_string()), spent);
        let ev = event(UUID_B, 13483, "start");
        assert_eq!(resolve_predecessor(&ev, &threads, &no_owners()), None);
    }

    const APP_SERVER: &str = "/Users/t/.codex/packages/app-server-daemon/releases/0.158.0-aarch64-apple-darwin/bin/codex app-server --listen unix:// --managed-daemon";

    fn codex_threads_on(pid: u32, pid_start: &str) -> HashMap<ThreadKey, ThreadState> {
        let mut a = thread(UUID_A, pid, pid_start);
        a.cli = Cli::Codex;
        HashMap::from([((Cli::Codex, UUID_A.to_string()), a)])
    }

    fn codex_event(id: &str, pid: u32, pid_start: &str) -> HookEvent {
        HookEvent {
            cli: Cli::Codex,
            ..event(id, pid, pid_start)
        }
    }

    #[test]
    fn sessions_sharing_the_codex_app_server_are_not_successors() {
        let threads = codex_threads_on(50509, "Mon Sep 28 07:14:55 2026");
        let ev = codex_event(UUID_B, 50509, "Mon Sep 28 07:14:55 2026");
        assert_eq!(
            predecessor_given_argv(&ev, Some(APP_SERVER), &threads, &no_owners()),
            None
        );
    }

    #[test]
    fn a_shared_codex_host_does_not_link_to_an_owned_session() {
        let threads = codex_threads_on(50509, "start");
        let owned = HashMap::from([((Cli::Codex, UUID_A.to_string()), ())]);
        let ev = codex_event(UUID_B, 50509, "start");
        assert_eq!(
            predecessor_given_argv(&ev, Some(APP_SERVER), &threads, &owned),
            None
        );
    }

    #[test]
    fn an_unreadable_codex_host_does_not_link_sessions_by_pid() {
        let threads = codex_threads_on(50509, "start");
        let ev = codex_event(UUID_B, 50509, "start");
        assert_eq!(
            predecessor_given_argv(&ev, None, &threads, &no_owners()),
            None
        );
    }

    #[test]
    fn an_unreadable_claude_process_still_links_in_process_rotation() {
        let threads = HashMap::from([(
            (Cli::Claude, UUID_A.to_string()),
            thread(UUID_A, 4242, "start"),
        )]);
        assert_eq!(
            predecessor_given_argv(&event(UUID_B, 4242, "start"), None, &threads, &no_owners()),
            Some((Cli::Claude, UUID_A.to_string()))
        );
    }

    #[test]
    fn discards_persisted_shared_host_links_in_both_directions() {
        let mut restored = thread(UUID_A, 50509, "start");
        restored.cli = Cli::Codex;
        restored.supersedes = Some(UUID_B.into());
        restored.superseded_by = Some(UUID_C.into());
        discard_links_given_argv(&mut restored, Some(APP_SERVER));
        assert_eq!(restored.supersedes, None);
        assert_eq!(restored.superseded_by, None);
    }

    #[test]
    fn discards_persisted_codex_links_when_the_host_cannot_be_inspected() {
        let mut restored = thread(UUID_A, 50509, "start");
        restored.cli = Cli::Codex;
        restored.supersedes = Some(UUID_B.into());
        restored.superseded_by = Some(UUID_C.into());
        discard_links_given_argv(&mut restored, None);
        assert_eq!(restored.supersedes, None);
        assert_eq!(restored.superseded_by, None);
    }

    #[test]
    fn retains_persisted_links_for_a_dedicated_codex_process() {
        let mut restored = thread(UUID_A, 4242, "start");
        restored.cli = Cli::Codex;
        restored.supersedes = Some(UUID_B.into());
        restored.superseded_by = Some(UUID_C.into());
        discard_links_given_argv(&mut restored, Some("/opt/homebrew/bin/codex --yolo"));
        assert_eq!(restored.supersedes.as_deref(), Some(UUID_B));
        assert_eq!(restored.superseded_by.as_deref(), Some(UUID_C));
    }

    #[test]
    fn a_codex_tui_process_still_links_by_pid() {
        let threads = codex_threads_on(4242, "start");
        let ev = codex_event(UUID_B, 4242, "start");
        assert_eq!(
            predecessor_given_argv(
                &ev,
                Some("/opt/homebrew/bin/codex --yolo"),
                &threads,
                &no_owners()
            ),
            Some((Cli::Codex, UUID_A.to_string()))
        );
    }

    #[test]
    fn recognises_only_the_codex_app_server_as_a_shared_host() {
        assert!(is_shared_session_host(APP_SERVER));
        assert!(is_shared_session_host("codex app-server"));
        assert!(!is_shared_session_host("/opt/homebrew/bin/codex --yolo"));
        assert!(!is_shared_session_host("claude --resume app-server"));
        assert!(!is_shared_session_host(""));
    }

    #[test]
    fn unowned_predecessor_never_hands_over() {
        let mut parent = thread(UUID_A, 1, "s");
        parent.owner_product = None;
        parent.owner_thread_id = None;
        let child = thread(UUID_B, 2, "t");
        assert!(!should_take_ownership(&parent, &child, 1_000.0, 120.0));
    }

    #[test]
    fn gone_predecessor_hands_over_immediately() {
        let mut parent = thread(UUID_A, 1, "s");
        parent.liveness = Liveness::Gone;
        let child = thread(UUID_B, 2, "t");
        assert!(should_take_ownership(&parent, &child, 100.0, 120.0));
    }

    #[test]
    fn in_place_rotation_hands_over_immediately() {
        let parent = thread(UUID_A, 13483, "Wed Aug 19 17:03:32 2026");
        let child = thread(UUID_B, 13483, "Wed Aug 19 17:03:32 2026");
        assert!(should_take_ownership(&parent, &child, 100.0, 120.0));
    }

    #[test]
    fn a_still_working_predecessor_keeps_ownership() {
        // The sibling case: a session forks a background helper and carries on.
        let mut parent = thread(UUID_A, 1, "s");
        parent.last_event = 990.0;
        let mut child = thread(UUID_B, 2, "t");
        child.last_event = 1_000.0;
        assert!(!should_take_ownership(&parent, &child, 1_000.0, 120.0));
    }

    #[test]
    fn a_silent_predecessor_hands_over_once_the_grace_elapses() {
        let mut parent = thread(UUID_A, 1, "s");
        parent.last_event = 800.0;
        let mut child = thread(UUID_B, 2, "t");
        child.last_event = 1_000.0;
        assert!(should_take_ownership(&parent, &child, 1_000.0, 120.0));
    }
}
