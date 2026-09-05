//! Per-thread state and the activity reducer.
//!
//! Ported from Codezilla's `src/components/CenterPanel/Terminal.tsx`:
//! `applyHookEvent` lines 514–636 and `isMetaTool`. Adapted for Rust + daemon
//! context — the post-Stop transcript scan is a daemon-side operation rather
//! than an inline branch of the reducer (see [`resolve_post_stop`]).

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fmt;

/// Which CLI emitted this thread's events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cli {
    Claude,
    Codex,
}

impl fmt::Display for Cli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Cli::Claude => "claude",
            Cli::Codex => "codex",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Working,
    AwaitingInput,
    Idle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Liveness {
    Live,
    Gone,
}

/// Composite key for the `(cli, thread_id)` pair the daemon uses to dedupe.
/// Native session UUIDs are globally unique in practice, but pairing with
/// `cli` is cheap insurance.
pub type ThreadKey = (Cli, String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanProgress {
    pub total: u32,
    pub done: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerInfo {
    pub owner_product: Option<String>,
    pub owner_thread_id: Option<String>,
    pub cwd: Option<String>,
}

/// One record per native CLI session — the unit the daemon writes to state.json.
/// Matches SPEC §4.1 verbatim.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadState {
    pub thread_id: String,
    pub cli: Cli,
    pub activity: Activity,
    pub liveness: Liveness,
    /// Unix seconds (fractional).
    pub first_seen: f64,
    pub last_event: f64,
    pub last_check: f64,
    pub pid: u32,
    /// `ps -o lstart=` output for `pid` at first sighting. Empty if not
    /// captured (older hook or unusual `ps`).
    pub pid_start: String,
    pub in_plan_mode: bool,
    pub plan_progress: Option<PlanProgress>,
    pub last_tool_name: Option<String>,
    pub last_tool_target: Option<String>,
    /// Pre-rendered display subtitle (filled in by the daemon after each event
    /// using `tool_display::format_for_thread`).
    pub subtitle: Option<String>,
    pub cwd: Option<String>,
    pub transcript_path: Option<String>,
    pub owner_product: Option<String>,
    pub owner_thread_id: Option<String>,
    /// Native session id of the thread this one continues, when a CLI rotated
    /// its session id mid-conversation. See [`crate::succession`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// Native session id of the thread that continued this one. Set on the
    /// predecessor once a successor is linked; a consumer should follow this to
    /// the tip rather than reading a superseded record's frozen activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Last N hook events for `heed tui` detail pane. Bounded by
    /// [`RECENT_EVENTS_CAP`]; oldest entries drop off as new ones land.
    #[serde(default)]
    pub recent_events: VecDeque<RecentEvent>,
}

pub const RECENT_EVENTS_CAP: usize = 10;

/// Compact event record kept inside [`ThreadState::recent_events`] for the TUI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecentEvent {
    pub event: HookEventKind,
    pub ts: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_target: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEventKind {
    TurnStart,
    PreToolUse,
    ToolUse,
    TurnEnd,
    /// Reserved for v0.2 `SessionEnd` hook (SPEC §5.4 / §13). Reducer marks
    /// the thread `gone` immediately.
    SessionEnd,
}

/// One parsed line from `events.jsonl`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HookEvent {
    pub event: HookEventKind,
    pub ts: f64,
    pub cli: Cli,
    pub thread_id: String,
    pub pid: u32,
    #[serde(default)]
    pub pid_start: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub extra: HookEventExtra,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct HookEventExtra {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub todos_total: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub todos_done: Option<u32>,
}

impl HookEvent {
    pub fn key(&self) -> ThreadKey {
        (self.cli, self.thread_id.clone())
    }

    fn truthy_string(s: &Option<String>) -> Option<String> {
        s.as_ref().filter(|v| !v.is_empty()).cloned()
    }

    pub fn tool_name(&self) -> Option<&str> {
        self.extra.tool_name.as_deref().filter(|s| !s.is_empty())
    }

    pub fn tool_target(&self) -> Option<&str> {
        self.extra.tool_target.as_deref().filter(|s| !s.is_empty())
    }
}

/// Codezilla meta-tool list (Terminal.tsx::isMetaTool, line 507–512).
fn is_meta_tool(name: Option<&str>) -> bool {
    matches!(
        name,
        Some("AskUserQuestion" | "EnterPlanMode" | "ExitPlanMode" | "PermissionRequest")
    )
}

/// Initial state for a freshly-observed thread, before any reducer call.
pub fn initial_state(ev: &HookEvent) -> ThreadState {
    let cwd = HookEvent::truthy_string(&ev.cwd);
    let transcript_path = HookEvent::truthy_string(&ev.transcript_path);
    ThreadState {
        thread_id: ev.thread_id.clone(),
        cli: ev.cli,
        activity: Activity::Working,
        liveness: Liveness::Live,
        first_seen: ev.ts,
        last_event: ev.ts,
        last_check: ev.ts,
        pid: ev.pid,
        pid_start: ev.pid_start.clone(),
        in_plan_mode: false,
        plan_progress: None,
        last_tool_name: None,
        last_tool_target: None,
        subtitle: None,
        cwd,
        transcript_path,
        owner_product: None,
        owner_thread_id: None,
        supersedes: None,
        superseded_by: None,
        recent_events: VecDeque::with_capacity(RECENT_EVENTS_CAP),
    }
}

/// Apply a hook event to a thread state, returning the updated state.
///
/// SPEC §4.2: `turn_end` records the event but **does not** flip activity —
/// the daemon must run a transcript scan and then call
/// [`resolve_post_stop`] to land on `AwaitingInput` or `Idle`.
pub fn apply_event(mut state: ThreadState, ev: &HookEvent) -> ThreadState {
    // Always-on bookkeeping.
    state.last_event = ev.ts;
    state.pid = ev.pid;
    if !ev.pid_start.is_empty() {
        state.pid_start = ev.pid_start.clone();
    }
    if let Some(p) = HookEvent::truthy_string(&ev.transcript_path) {
        state.transcript_path = Some(p);
    }
    if let Some(c) = HookEvent::truthy_string(&ev.cwd) {
        state.cwd = Some(c);
    }
    state.liveness = Liveness::Live;

    push_recent(&mut state, ev);

    match ev.event {
        HookEventKind::TurnStart => {
            // Drop completed plan progress so the counter doesn't linger.
            if let Some(p) = state.plan_progress {
                if p.done >= p.total {
                    state.plan_progress = None;
                }
            }
            state.activity = Activity::Working;
            state.last_tool_name = None;
            state.last_tool_target = None;
        }
        HookEventKind::PreToolUse => {
            let tool = ev.tool_name();
            let is_plan_mode_tool = matches!(tool, Some("EnterPlanMode" | "ExitPlanMode"));
            if is_plan_mode_tool {
                state.in_plan_mode = true;
            }
            if !is_meta_tool(tool) {
                if let Some(n) = tool {
                    state.last_tool_name = Some(n.to_string());
                    state.last_tool_target = ev.tool_target().map(str::to_string);
                }
            }
            if matches!(
                tool,
                Some("AskUserQuestion" | "ExitPlanMode" | "PermissionRequest")
            ) {
                state.activity = Activity::AwaitingInput;
            } else if tool.is_some() && !is_meta_tool(tool) {
                // Execution has resumed; do not wait for a long-running tool
                // to finish before clearing a stale awaiting-input state.
                state.activity = Activity::Working;
            }
        }
        HookEventKind::ToolUse => {
            let tool = ev.tool_name();

            if !is_meta_tool(tool) {
                if let Some(n) = tool {
                    state.last_tool_name = Some(n.to_string());
                    if let Some(t) = ev.tool_target() {
                        state.last_tool_target = Some(t.to_string());
                    }
                }
            }

            match tool {
                Some("ExitPlanMode") => state.in_plan_mode = false,
                Some("TaskCreate") => {
                    let prev = state
                        .plan_progress
                        .unwrap_or(PlanProgress { total: 0, done: 0 });
                    state.plan_progress = Some(PlanProgress {
                        total: prev.total + 1,
                        done: prev.done,
                    });
                }
                Some("TaskUpdate") => match ev.extra.task_status.as_deref() {
                    Some("completed") => {
                        let prev = state
                            .plan_progress
                            .unwrap_or(PlanProgress { total: 0, done: 0 });
                        state.plan_progress = Some(PlanProgress {
                            total: prev.total,
                            done: prev.done + 1,
                        });
                    }
                    Some("deleted") => {
                        let prev = state
                            .plan_progress
                            .unwrap_or(PlanProgress { total: 0, done: 0 });
                        state.plan_progress = Some(PlanProgress {
                            total: prev.total.saturating_sub(1),
                            done: prev.done,
                        });
                    }
                    _ => {}
                },
                Some("TodoWrite") => {
                    if let Some(total) = ev.extra.todos_total {
                        state.plan_progress = if total > 0 {
                            Some(PlanProgress {
                                total,
                                done: ev.extra.todos_done.unwrap_or(0),
                            })
                        } else {
                            None
                        };
                    }
                }
                _ => {}
            }

            state.activity = Activity::Working;
        }
        HookEventKind::TurnEnd => {
            // Activity is NOT flipped here. The daemon runs a transcript
            // scan and then calls resolve_post_stop(...).
            // We keep activity == Working (or whatever it was) until then.
        }
        HookEventKind::SessionEnd => {
            // v0.2 fast-path: clean exit → immediately gone, no polling.
            state.liveness = Liveness::Gone;
            // A dead thread isn't working — don't leave activity frozen mid-turn.
            state.activity = Activity::Idle;
        }
    }

    state
}

fn push_recent(state: &mut ThreadState, ev: &HookEvent) {
    state.recent_events.push_back(RecentEvent {
        event: ev.event,
        ts: ev.ts,
        tool_name: ev.tool_name().map(str::to_string),
        tool_target: ev.tool_target().map(str::to_string),
    });
    while state.recent_events.len() > RECENT_EVENTS_CAP {
        state.recent_events.pop_front();
    }
}

/// After a `TurnEnd`, the daemon runs a transcript scan and feeds the result
/// back here. SPEC §4.2 post-Stop evaluation rule:
/// - last meaningful char `?` → AwaitingInput
/// - last meaningful char `.` / `!` → Idle
/// - neither (transcript unreachable, etc.) → Idle (fallback)
///
/// No-op if the thread is already `AwaitingInput` — a
/// `pre_tool_use(AskUserQuestion)` arriving between the Stop and the scan
/// must not be clobbered back to Idle.
pub fn resolve_post_stop(mut state: ThreadState, result: PostStopResult) -> ThreadState {
    if state.activity == Activity::AwaitingInput {
        return state;
    }
    state.activity = match result {
        PostStopResult::Question => Activity::AwaitingInput,
        PostStopResult::Statement | PostStopResult::Neither => Activity::Idle,
    };
    state
}

/// Result of `transcript::ends_like_question` against the post-Stop transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostStopResult {
    Question,
    Statement,
    Neither,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: HookEventKind, ts: f64, tool: Option<&str>, target: Option<&str>) -> HookEvent {
        HookEvent {
            event: kind,
            ts,
            cli: Cli::Claude,
            thread_id: "t1".into(),
            pid: 100,
            pid_start: "Mon Jan 1 00:00:00 2026".into(),
            cwd: Some("/cwd".into()),
            transcript_path: Some("/transcript.jsonl".into()),
            extra: HookEventExtra {
                tool_name: tool.map(str::to_string),
                tool_target: target.map(str::to_string),
                task_status: None,
                todos_total: None,
                todos_done: None,
            },
        }
    }

    fn ev_taskupdate(status: &str) -> HookEvent {
        let mut e = ev(HookEventKind::ToolUse, 1.0, Some("TaskUpdate"), None);
        e.extra.task_status = Some(status.into());
        e
    }

    fn ev_todowrite(total: u32, done: u32) -> HookEvent {
        let mut e = ev(HookEventKind::ToolUse, 1.0, Some("TodoWrite"), None);
        e.extra.todos_total = Some(total);
        e.extra.todos_done = Some(done);
        e
    }

    fn apply_all(events: &[HookEvent]) -> ThreadState {
        let mut state = initial_state(&events[0]);
        for e in events {
            state = apply_event(state, e);
        }
        state
    }

    #[test]
    fn turn_start_sets_working_and_clears_tool() {
        let mut state = initial_state(&ev(HookEventKind::TurnStart, 1.0, None, None));
        state.last_tool_name = Some("Bash".into());
        state.last_tool_target = Some("ls".into());
        let next = apply_event(state, &ev(HookEventKind::TurnStart, 2.0, None, None));
        assert_eq!(next.activity, Activity::Working);
        assert_eq!(next.last_tool_name, None);
        assert_eq!(next.last_tool_target, None);
    }

    #[test]
    fn ask_user_question_flips_to_awaiting() {
        let events = vec![
            ev(HookEventKind::TurnStart, 1.0, None, None),
            ev(
                HookEventKind::PreToolUse,
                2.0,
                Some("AskUserQuestion"),
                None,
            ),
        ];
        let state = apply_all(&events);
        assert_eq!(state.activity, Activity::AwaitingInput);
        // Meta tools do not populate last_tool_name.
        assert_eq!(state.last_tool_name, None);
    }

    #[test]
    fn permission_request_flips_to_awaiting() {
        let events = vec![
            ev(HookEventKind::TurnStart, 1.0, None, None),
            ev(
                HookEventKind::PreToolUse,
                2.0,
                Some("PermissionRequest"),
                None,
            ),
        ];
        assert_eq!(apply_all(&events).activity, Activity::AwaitingInput);
    }

    #[test]
    fn ordinary_tool_start_clears_stale_waiting_before_completion() {
        for activity in [Activity::AwaitingInput, Activity::Idle] {
            let event = ev(
                HookEventKind::PreToolUse,
                2.0,
                Some("Bash"),
                Some("long build"),
            );
            let mut state = initial_state(&event);
            state.activity = activity;
            assert_eq!(apply_event(state, &event).activity, Activity::Working);
        }
    }

    #[test]
    fn tool_use_after_awaiting_returns_to_working() {
        let events = vec![
            ev(HookEventKind::TurnStart, 1.0, None, None),
            ev(
                HookEventKind::PreToolUse,
                2.0,
                Some("AskUserQuestion"),
                None,
            ),
            ev(HookEventKind::ToolUse, 3.0, Some("Bash"), Some("ls -la")),
        ];
        let state = apply_all(&events);
        assert_eq!(state.activity, Activity::Working);
        assert_eq!(state.last_tool_name.as_deref(), Some("Bash"));
        assert_eq!(state.last_tool_target.as_deref(), Some("ls -la"));
    }

    #[test]
    fn pre_tool_use_real_tool_updates_last_tool() {
        let events = vec![
            ev(HookEventKind::TurnStart, 1.0, None, None),
            ev(
                HookEventKind::PreToolUse,
                2.0,
                Some("Read"),
                Some("/foo.rs"),
            ),
        ];
        let state = apply_all(&events);
        assert_eq!(state.last_tool_name.as_deref(), Some("Read"));
        assert_eq!(state.last_tool_target.as_deref(), Some("/foo.rs"));
        // A real tool start establishes working activity.
        assert_eq!(state.activity, Activity::Working);
    }

    #[test]
    fn enter_plan_mode_sets_flag() {
        let events = vec![
            ev(HookEventKind::TurnStart, 1.0, None, None),
            ev(HookEventKind::PreToolUse, 2.0, Some("EnterPlanMode"), None),
        ];
        assert!(apply_all(&events).in_plan_mode);
    }

    #[test]
    fn exit_plan_mode_pre_arms_then_post_clears() {
        let events = vec![
            ev(HookEventKind::TurnStart, 1.0, None, None),
            ev(HookEventKind::PreToolUse, 2.0, Some("EnterPlanMode"), None),
            ev(HookEventKind::PreToolUse, 3.0, Some("ExitPlanMode"), None),
        ];
        let after_pre = apply_all(&events);
        assert!(after_pre.in_plan_mode);
        assert_eq!(after_pre.activity, Activity::AwaitingInput);
        let next = apply_event(
            after_pre,
            &ev(HookEventKind::ToolUse, 4.0, Some("ExitPlanMode"), None),
        );
        assert!(!next.in_plan_mode);
        assert_eq!(next.activity, Activity::Working);
    }

    #[test]
    fn task_create_increments_total() {
        let events = vec![
            ev(HookEventKind::TurnStart, 1.0, None, None),
            ev(HookEventKind::ToolUse, 2.0, Some("TaskCreate"), None),
            ev(HookEventKind::ToolUse, 3.0, Some("TaskCreate"), None),
            ev(HookEventKind::ToolUse, 4.0, Some("TaskCreate"), None),
        ];
        let state = apply_all(&events);
        assert_eq!(
            state.plan_progress,
            Some(PlanProgress { total: 3, done: 0 })
        );
    }

    #[test]
    fn task_update_completed_increments_done() {
        let events = vec![
            ev(HookEventKind::TurnStart, 1.0, None, None),
            ev(HookEventKind::ToolUse, 2.0, Some("TaskCreate"), None),
            ev(HookEventKind::ToolUse, 3.0, Some("TaskCreate"), None),
            ev(HookEventKind::ToolUse, 4.0, Some("TaskCreate"), None),
            ev_taskupdate("completed"),
            ev_taskupdate("completed"),
        ];
        let state = apply_all(&events);
        assert_eq!(
            state.plan_progress,
            Some(PlanProgress { total: 3, done: 2 })
        );
    }

    #[test]
    fn task_update_deleted_decrements_total_with_saturation() {
        let events = vec![
            ev(HookEventKind::TurnStart, 1.0, None, None),
            ev(HookEventKind::ToolUse, 2.0, Some("TaskCreate"), None),
            ev_taskupdate("deleted"),
            ev_taskupdate("deleted"), // would go to -1 without saturation
        ];
        let state = apply_all(&events);
        assert_eq!(
            state.plan_progress,
            Some(PlanProgress { total: 0, done: 0 })
        );
    }

    #[test]
    fn todowrite_replaces_progress() {
        let events = vec![
            ev(HookEventKind::TurnStart, 1.0, None, None),
            ev(HookEventKind::ToolUse, 2.0, Some("TaskCreate"), None),
            ev_todowrite(5, 2),
        ];
        let state = apply_all(&events);
        assert_eq!(
            state.plan_progress,
            Some(PlanProgress { total: 5, done: 2 })
        );
    }

    #[test]
    fn todowrite_zero_total_clears_progress() {
        let events = vec![
            ev(HookEventKind::TurnStart, 1.0, None, None),
            ev_todowrite(3, 1),
            ev_todowrite(0, 0),
        ];
        let state = apply_all(&events);
        assert_eq!(state.plan_progress, None);
    }

    #[test]
    fn turn_end_does_not_flip_activity_until_resolved() {
        let events = vec![
            ev(HookEventKind::TurnStart, 1.0, None, None),
            ev(HookEventKind::ToolUse, 2.0, Some("Bash"), Some("ls")),
            ev(HookEventKind::TurnEnd, 3.0, None, None),
        ];
        let state = apply_all(&events);
        // Activity stays Working until daemon runs transcript scan.
        assert_eq!(state.activity, Activity::Working);
    }

    #[test]
    fn resolve_post_stop_question_yields_awaiting() {
        let mut state = initial_state(&ev(HookEventKind::TurnStart, 1.0, None, None));
        state.activity = Activity::Working;
        let resolved = resolve_post_stop(state, PostStopResult::Question);
        assert_eq!(resolved.activity, Activity::AwaitingInput);
    }

    #[test]
    fn resolve_post_stop_statement_yields_idle() {
        let mut state = initial_state(&ev(HookEventKind::TurnStart, 1.0, None, None));
        state.activity = Activity::Working;
        let resolved = resolve_post_stop(state, PostStopResult::Statement);
        assert_eq!(resolved.activity, Activity::Idle);
    }

    #[test]
    fn resolve_post_stop_preserves_awaiting_input() {
        // SPEC §4.2: if pre_tool_use(AskUserQuestion) arrives between TurnEnd
        // and the delayed PostStopScan, the scan's Statement/Neither result
        // must not clobber AwaitingInput back to Idle.
        let state = initial_state(&ev(HookEventKind::TurnStart, 1.0, None, None));
        let after_turn_end = apply_event(state, &ev(HookEventKind::TurnEnd, 2.0, None, None));
        let after_ask = apply_event(
            after_turn_end,
            &ev(
                HookEventKind::PreToolUse,
                3.0,
                Some("AskUserQuestion"),
                None,
            ),
        );
        assert_eq!(after_ask.activity, Activity::AwaitingInput);

        let scanned = resolve_post_stop(after_ask.clone(), PostStopResult::Statement);
        assert_eq!(scanned.activity, Activity::AwaitingInput);
        let scanned_neither = resolve_post_stop(after_ask, PostStopResult::Neither);
        assert_eq!(scanned_neither.activity, Activity::AwaitingInput);
    }

    #[test]
    fn recent_events_bounded_at_cap() {
        let mut state = initial_state(&ev(HookEventKind::TurnStart, 0.0, None, None));
        for i in 0..30 {
            state = apply_event(
                state,
                &ev(HookEventKind::ToolUse, i as f64, Some("Bash"), Some("ls")),
            );
        }
        assert_eq!(state.recent_events.len(), RECENT_EVENTS_CAP);
        // Oldest dropped — first kept event is at ts = 30 - 10 = 20.
        let earliest = state.recent_events.front().unwrap().ts;
        assert_eq!(earliest, 20.0);
    }

    #[test]
    fn session_end_marks_gone() {
        let events = vec![
            ev(HookEventKind::TurnStart, 1.0, None, None),
            ev(HookEventKind::SessionEnd, 2.0, None, None),
        ];
        let state = apply_all(&events);
        assert_eq!(state.liveness, Liveness::Gone);
    }

    #[test]
    fn cwd_and_transcript_path_updated_on_each_event() {
        let mut state = initial_state(&ev(HookEventKind::TurnStart, 1.0, None, None));
        state.cwd = None;
        state.transcript_path = None;
        let mut e = ev(HookEventKind::PreToolUse, 2.0, Some("Read"), Some("/x"));
        e.cwd = Some("/new/cwd".into());
        e.transcript_path = Some("/new/transcript.jsonl".into());
        let next = apply_event(state, &e);
        assert_eq!(next.cwd.as_deref(), Some("/new/cwd"));
        assert_eq!(
            next.transcript_path.as_deref(),
            Some("/new/transcript.jsonl")
        );
    }

    #[test]
    fn empty_cwd_does_not_overwrite_existing() {
        let mut state = initial_state(&ev(HookEventKind::TurnStart, 1.0, None, None));
        state.cwd = Some("/original".into());
        let mut e = ev(HookEventKind::PreToolUse, 2.0, Some("Read"), Some("/x"));
        e.cwd = Some("".into()); // empty value from hook
        let next = apply_event(state, &e);
        assert_eq!(next.cwd.as_deref(), Some("/original"));
    }
}
