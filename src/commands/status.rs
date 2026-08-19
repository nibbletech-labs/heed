//! `heed status` — one-shot snapshot of all tracked threads.

use std::io::Write;
use std::path::Path;

use crossterm::{
    style::{Color, Print, ResetColor, SetForegroundColor},
    QueueableCommand,
};

use crate::daemon::state_writer::StateFile;
use crate::state::{Activity, Liveness, ThreadState};

#[derive(Clone, Debug)]
pub struct StatusArgs {
    pub all: bool,
    pub json: bool,
    pub ascii: bool,
}

pub fn run(args: StatusArgs) -> Result<(), String> {
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .map_err(|_| "HOME env var not set".to_string())?;
    let state_path = crate::install::state_path(&home);
    if !state_path.exists() {
        if args.json {
            println!("{{\"threads\":[]}}");
            return Ok(());
        }
        println!("heed: no state file yet. Run `heed install` to set up hooks.");
        return Ok(());
    }
    let raw =
        std::fs::read_to_string(&state_path).map_err(|e| format!("read {state_path:?}: {e}"))?;
    let state: StateFile =
        serde_json::from_str(&raw).map_err(|e| format!("parse state.json: {e}"))?;

    if args.json {
        let mut out = serde_json::Map::new();
        out.insert("updated_at".into(), state.updated_at.into());
        out.insert("heed_version".into(), state.heed_version.clone().into());
        let filtered = filter_for_output(&state, args.all);
        out.insert("threads".into(), serde_json::to_value(filtered).unwrap());
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return Ok(());
    }

    render_terminal(&state, &args)
}

fn filter_for_output(state: &StateFile, all: bool) -> Vec<&ThreadState> {
    let mut threads: Vec<&ThreadState> = state.threads.values().collect();
    if !all {
        threads.retain(|t| t.liveness == Liveness::Live);
    }
    sort_threads(&mut threads);
    threads
}

fn sort_threads(threads: &mut [&ThreadState]) {
    // SPEC §6.1: awaiting first, then working, then idle. Within a group,
    // most recent event first.
    fn activity_rank(a: Activity) -> u8 {
        match a {
            Activity::AwaitingInput => 0,
            Activity::Working => 1,
            Activity::Idle => 2,
        }
    }
    threads.sort_by(|a, b| {
        activity_rank(a.activity)
            .cmp(&activity_rank(b.activity))
            .then(
                b.last_event
                    .partial_cmp(&a.last_event)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
}

fn render_terminal(state: &StateFile, args: &StatusArgs) -> Result<(), String> {
    let mut threads: Vec<&ThreadState> = state.threads.values().collect();
    let gone_count = threads
        .iter()
        .filter(|t| t.liveness == Liveness::Gone)
        .count();
    if !args.all {
        threads.retain(|t| t.liveness == Liveness::Live);
    }
    sort_threads(&mut threads);

    let needs_attention = threads
        .iter()
        .filter(|t| t.activity == Activity::AwaitingInput)
        .count();

    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(
        stdout,
        "heed · {} thread{}{}",
        threads.len(),
        if threads.len() == 1 { "" } else { "s" },
        if needs_attention > 0 {
            format!(" · {needs_attention} needs attention")
        } else {
            String::new()
        }
    );
    let _ = writeln!(
        stdout,
        "─────────────────────────────────────────────────────────"
    );

    if threads.is_empty() {
        let _ = writeln!(
            stdout,
            "  (no active threads — run a Claude or Codex session and re-check)"
        );
    } else {
        for t in &threads {
            render_row(&mut stdout, t, args.ascii)?;
        }
    }

    if !args.all && gone_count > 0 {
        let _ = writeln!(
            stdout,
            "\n  +{} ended session{} (heed status --all)",
            gone_count,
            if gone_count == 1 { "" } else { "s" }
        );
    }
    let _ = stdout.flush();
    Ok(())
}

fn render_row(out: &mut impl Write, t: &ThreadState, ascii: bool) -> Result<(), String> {
    let (glyph, color) = match t.activity {
        Activity::AwaitingInput => (if ascii { "[!]" } else { "⚡" }, Color::Yellow),
        Activity::Working => (if ascii { "[>]" } else { "▶" }, Color::Green),
        Activity::Idle => (if ascii { "[ ]" } else { "○" }, Color::DarkGrey),
    };
    let label = match t.activity {
        Activity::AwaitingInput => "AWAITING",
        Activity::Working => "WORKING ",
        Activity::Idle => "IDLE    ",
    };
    let short_thread = display_thread_id(t);
    let subtitle = t.subtitle.clone().unwrap_or_default();
    let age = humanize_age(t.last_event);

    let _ = out.queue(SetForegroundColor(color));
    let _ = out.queue(Print(format!("{glyph} {label} ")));
    let _ = out.queue(ResetColor);
    let _ = out.queue(Print(format!(
        "{:<10} {:<7} {:<32} {:>5}",
        short_thread,
        t.cli,
        truncate_for_col(&subtitle, 32),
        age
    )));
    if t.liveness == Liveness::Gone {
        let _ = out.queue(SetForegroundColor(Color::DarkGrey));
        let _ = out.queue(Print(" (gone)"));
        let _ = out.queue(ResetColor);
    }
    let _ = out.queue(Print("\n"));
    Ok(())
}

fn display_thread_id(t: &ThreadState) -> String {
    let source = t.owner_thread_id.as_deref().unwrap_or(&t.thread_id);
    short_thread_id(source)
}

fn short_thread_id(id: &str) -> String {
    // First 8 chars of whichever ID we're displaying. Native UUIDs end up as
    // hex like "c0bd0747"; owner-style IDs like "mux-4c43c2-researcher" keep
    // enough prefix ("mux-4c43") to stay distinguishable between siblings.
    id.chars().take(8).collect()
}

fn truncate_for_col(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return s.to_string();
    }
    let mut out: String = chars.into_iter().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn humanize_age(last_event_unix: f64) -> String {
    let now = crate::daemon::state_writer::now_unix();
    let secs = (now - last_event_unix).max(0.0) as u64;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[allow(dead_code)]
pub fn load_state(state_path: &Path) -> Result<StateFile, String> {
    let raw =
        std::fs::read_to_string(state_path).map_err(|e| format!("read {state_path:?}: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("parse: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_age_units() {
        // future timestamps clamp to 0
        let now = crate::daemon::state_writer::now_unix();
        assert_eq!(humanize_age(now + 100.0), "0s");
        assert!(humanize_age(now - 30.0).ends_with("s"));
        assert!(humanize_age(now - 300.0).ends_with("m"));
        assert!(humanize_age(now - 9000.0).ends_with("h"));
        assert!(humanize_age(now - 200_000.0).ends_with("d"));
    }

    #[test]
    fn short_thread_id_truncates_uuids() {
        assert_eq!(short_thread_id("ce4f1234-abcd-5678-..."), "ce4f1234");
        assert_eq!(short_thread_id("abc"), "abc");
        // Owner-style IDs keep their dash-bearing prefix so siblings sharing
        // a product slug stay distinguishable.
        assert_eq!(short_thread_id("mux-4c43c2-researcher"), "mux-4c43");
        assert_eq!(short_thread_id("mux-9abc12-builder"), "mux-9abc");
    }

    #[test]
    fn truncate_for_col_truncates_and_ellipsizes() {
        assert_eq!(truncate_for_col("hello", 10), "hello");
        let s = truncate_for_col("this is a long string", 10);
        assert!(s.chars().count() <= 10);
        assert!(s.ends_with('…'));
    }

    fn make_thread(thread_id: &str, owner: Option<&str>) -> ThreadState {
        ThreadState {
            thread_id: thread_id.into(),
            cli: crate::state::Cli::Claude,
            activity: Activity::Idle,
            liveness: Liveness::Live,
            first_seen: 0.0,
            last_event: 0.0,
            last_check: 0.0,
            pid: 1,
            pid_start: String::new(),
            in_plan_mode: false,
            plan_progress: None,
            last_tool_name: None,
            last_tool_target: None,
            subtitle: None,
            cwd: None,
            transcript_path: None,
            owner_product: owner.map(|_| "muxra".into()),
            owner_thread_id: owner.map(|s| s.into()),
            supersedes: None,
            superseded_by: None,
            recent_events: Default::default(),
        }
    }

    #[test]
    fn display_thread_id_prefers_owner_then_truncates() {
        let claude_uuid = "c0bd0747-1234-5678-9abc-def012345678";
        // Unowned: first 8 chars of native UUID.
        let unowned = make_thread(claude_uuid, None);
        assert_eq!(display_thread_id(&unowned), "c0bd0747");
        // Owned by Muxra: first 8 chars of owner_thread_id.
        let researcher = make_thread(claude_uuid, Some("mux-4c43c2-researcher"));
        let builder = make_thread(claude_uuid, Some("mux-9abc12-builder"));
        assert_eq!(display_thread_id(&researcher), "mux-4c43");
        assert_eq!(display_thread_id(&builder), "mux-9abc");
        assert_ne!(display_thread_id(&researcher), display_thread_id(&builder));
    }

    #[test]
    fn sort_order_awaiting_first() {
        let make = |a: Activity, ts: f64| ThreadState {
            thread_id: format!("{ts:?}"),
            cli: crate::state::Cli::Claude,
            activity: a,
            liveness: Liveness::Live,
            first_seen: 0.0,
            last_event: ts,
            last_check: 0.0,
            pid: 1,
            pid_start: String::new(),
            in_plan_mode: false,
            plan_progress: None,
            last_tool_name: None,
            last_tool_target: None,
            subtitle: None,
            cwd: None,
            transcript_path: None,
            owner_product: None,
            owner_thread_id: None,
            supersedes: None,
            superseded_by: None,
            recent_events: Default::default(),
        };
        let idle_new = make(Activity::Idle, 100.0);
        let working_new = make(Activity::Working, 90.0);
        let awaiting_old = make(Activity::AwaitingInput, 10.0);
        let mut all = vec![&idle_new, &working_new, &awaiting_old];
        sort_threads(&mut all);
        assert_eq!(all[0].activity, Activity::AwaitingInput);
        assert_eq!(all[1].activity, Activity::Working);
        assert_eq!(all[2].activity, Activity::Idle);
    }
}
