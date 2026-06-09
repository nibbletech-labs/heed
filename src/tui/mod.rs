//! `heed tui` — interactive two-pane TUI per SPEC §6.3.

use std::io;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::Backend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};

use crate::daemon::state_writer::StateFile;
use crate::state::{Activity, Liveness, RecentEvent, ThreadState};

pub fn run() -> Result<(), String> {
    enable_raw_mode().map_err(|e| format!("enable_raw_mode: {e}"))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .map_err(|e| format!("enter alt screen: {e}"))?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| format!("terminal: {e}"))?;

    let mut app = App::new();
    let result = app.run(&mut terminal);

    disable_raw_mode().map_err(|e| format!("disable_raw_mode: {e}"))?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .map_err(|e| format!("leave alt screen: {e}"))?;
    terminal.show_cursor().ok();
    result
}

struct App {
    /// Selected thread, by `<cli>:<thread_id>` key. Persists across refreshes
    /// so the cursor doesn't jump when other threads come or go.
    selected_key: Option<String>,
    show_gone: bool,
    /// Modal: show recent events for the selected thread.
    show_events: bool,
}

impl App {
    fn new() -> Self {
        Self {
            selected_key: None,
            show_gone: false,
            show_events: false,
        }
    }

    fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<(), String> {
        let tick = Duration::from_millis(500);
        let mut last_tick = Instant::now();
        loop {
            let state = load_state();
            let threads = visible_threads(&state, self.show_gone);

            if self.selected_key.is_none() || !threads.iter().any(|t| self.matches_selected(t)) {
                self.selected_key = threads.first().map(|t| key_for(t));
            }

            terminal
                .draw(|f| self.render(f, &state, &threads))
                .map_err(|e| format!("draw: {e}"))?;

            let timeout = tick.saturating_sub(last_tick.elapsed());
            if event::poll(timeout).map_err(|e| format!("poll: {e}"))? {
                if let Event::Key(key) = event::read().map_err(|e| format!("read: {e}"))? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('q') => return Ok(()),
                        KeyCode::Esc => {
                            if self.show_events {
                                self.show_events = false;
                            } else {
                                return Ok(());
                            }
                        }
                        KeyCode::Char('s') => self.show_gone = !self.show_gone,
                        KeyCode::Char('e') => self.show_events = !self.show_events,
                        KeyCode::Down | KeyCode::Char('j') => self.move_cursor(&threads, 1),
                        KeyCode::Up | KeyCode::Char('k') => self.move_cursor(&threads, -1),
                        _ => {}
                    }
                }
            }
            if last_tick.elapsed() >= tick {
                last_tick = Instant::now();
            }
        }
    }

    fn matches_selected(&self, t: &ThreadState) -> bool {
        self.selected_key.as_deref() == Some(key_for(t).as_str())
    }

    fn move_cursor(&mut self, threads: &[&ThreadState], delta: isize) {
        if threads.is_empty() {
            return;
        }
        let cur_idx = threads
            .iter()
            .position(|t| self.matches_selected(t))
            .unwrap_or(0) as isize;
        let next = (cur_idx + delta).clamp(0, threads.len() as isize - 1);
        self.selected_key = threads.get(next as usize).map(|t| key_for(t));
    }

    fn render(&self, f: &mut Frame, state: &Option<StateFile>, threads: &[&ThreadState]) {
        let area = f.size();
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);
        let panes = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(outer[0]);

        render_list(f, panes[0], threads, &self.selected_key, state);
        let selected = threads.iter().find(|t| self.matches_selected(t)).copied();
        render_detail(f, panes[1], selected, self.show_events);

        let footer = Paragraph::new("↑/↓ navigate · e events · s toggle stale · q quit")
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(footer, outer[1]);
    }
}

fn key_for(t: &ThreadState) -> String {
    format!("{}:{}", t.cli, t.thread_id)
}

fn load_state() -> Option<StateFile> {
    let home = std::env::var("HOME").ok().map(std::path::PathBuf::from)?;
    let path = crate::install::state_path(&home);
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn visible_threads(state: &Option<StateFile>, show_gone: bool) -> Vec<&ThreadState> {
    let Some(state) = state else {
        return Vec::new();
    };
    let mut threads: Vec<&ThreadState> = state.threads.values().collect();
    if !show_gone {
        threads.retain(|t| t.liveness == Liveness::Live);
    }
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
    threads
}

fn render_list(
    f: &mut Frame,
    area: Rect,
    threads: &[&ThreadState],
    selected_key: &Option<String>,
    state: &Option<StateFile>,
) {
    let needs_attention = threads
        .iter()
        .filter(|t| t.activity == Activity::AwaitingInput)
        .count();
    let updated = state.as_ref().map(|s| s.updated_at).unwrap_or(0.0);
    let title = if needs_attention > 0 {
        format!(
            " THREADS ({} · {} needs attention) ",
            threads.len(),
            needs_attention
        )
    } else {
        format!(" THREADS ({}) ", threads.len())
    };
    let _ = updated; // could surface in title if you want
    let mut list_state = ListState::default();
    let idx = threads
        .iter()
        .position(|t| Some(key_for(t)) == *selected_key);
    list_state.select(idx);

    let items: Vec<ListItem> = threads
        .iter()
        .map(|t| ListItem::new(format_thread_row(t)))
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, area, &mut list_state);
}

fn format_thread_row(t: &ThreadState) -> Line<'static> {
    let (glyph, color) = match t.activity {
        Activity::AwaitingInput => ("⚡", Color::Yellow),
        Activity::Working => ("▶", Color::Green),
        Activity::Idle => ("○", Color::DarkGray),
    };
    let short_thread = display_thread_id(t);
    let subtitle = t.subtitle.clone().unwrap_or_default();
    let age = humanize_age(t.last_event);
    let gone_marker = if t.liveness == Liveness::Gone {
        " (gone)"
    } else {
        ""
    };
    Line::from(vec![
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
        Span::raw(format!("{:<12} ", short_thread)),
        Span::styled(format!("{:<7} ", t.cli), Style::default().fg(Color::Blue)),
        Span::raw(format!("{:<32}", truncate_for_col(&subtitle, 32))),
        Span::styled(format!(" {:>5}", age), Style::default().fg(Color::DarkGray)),
        Span::styled(
            gone_marker.to_string(),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

fn render_detail(f: &mut Frame, area: Rect, selected: Option<&ThreadState>, show_events: bool) {
    let Some(t) = selected else {
        let p = Paragraph::new("No thread selected.")
            .block(Block::default().borders(Borders::ALL).title(" DETAIL "));
        f.render_widget(p, area);
        return;
    };

    let title = format!(" DETAIL · {} ", display_thread_id(t));
    let lines = if show_events {
        build_events_lines(t)
    } else {
        build_detail_lines(t)
    };
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn build_detail_lines(t: &ThreadState) -> Vec<Line<'static>> {
    let owner = match (&t.owner_product, &t.owner_thread_id) {
        (Some(p), Some(id)) => format!("Owner: {p}/{id}"),
        (Some(p), None) => format!("Owner: {p}"),
        _ => "Owner: (unowned)".into(),
    };
    let mut lines = vec![
        Line::from(format!(
            "CLI: {} · State: {:?} · Liveness: {:?}",
            t.cli, t.activity, t.liveness
        )),
        Line::from(format!(
            "{owner} · PID: {} ({})",
            t.pid,
            short_pid_start(&t.pid_start)
        )),
        Line::from(format!(
            "Cwd: {}",
            t.cwd.clone().unwrap_or_else(|| "(unknown)".into())
        )),
        Line::from(format!(
            "Transcript: {}",
            t.transcript_path
                .clone()
                .unwrap_or_else(|| "(unknown)".into())
        )),
        Line::from(format!(
            "Started: {} · Last event: {}",
            short_lstart(&t.pid_start),
            humanize_age(t.last_event)
        )),
        Line::from(""),
    ];
    if let Some(name) = &t.last_tool_name {
        let tgt = t.last_tool_target.as_deref().unwrap_or("");
        lines.push(Line::from(format!("Last tool: {name} {tgt}")));
    }
    if let Some(p) = t.plan_progress {
        lines.push(Line::from(format!("Plan progress: {}/{}", p.done, p.total)));
    }
    if t.in_plan_mode {
        lines.push(Line::from(Span::styled(
            "Plan mode",
            Style::default().fg(Color::Cyan),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from("Recent events:"));
    for ev in t.recent_events.iter().rev() {
        lines.push(render_recent_event(ev));
    }
    lines
}

fn build_events_lines(t: &ThreadState) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!("Events for {} (newest first)", display_thread_id(t)),
        Style::default().fg(Color::Cyan),
    ))];
    lines.push(Line::from(""));
    for ev in t.recent_events.iter().rev() {
        lines.push(render_recent_event(ev));
    }
    lines
}

fn render_recent_event(ev: &RecentEvent) -> Line<'static> {
    let kind = event_kind_label(ev.event);
    let tool = match (&ev.tool_name, &ev.tool_target) {
        (Some(n), Some(t)) => format!(" {n} → {t}"),
        (Some(n), None) => format!(" {n}"),
        _ => String::new(),
    };
    Line::from(format!("  {:>12.3}  {:<14}{tool}", ev.ts, kind))
}

fn event_kind_label(k: crate::state::HookEventKind) -> &'static str {
    use crate::state::HookEventKind::*;
    match k {
        TurnStart => "turn_start",
        PreToolUse => "pre_tool_use",
        ToolUse => "tool_use",
        TurnEnd => "turn_end",
        SessionEnd => "session_end",
    }
}

fn display_thread_id(t: &ThreadState) -> String {
    let source = t.owner_thread_id.as_deref().unwrap_or(&t.thread_id);
    short_thread_id(source)
}

fn short_thread_id(id: &str) -> String {
    // First 8 chars of whichever ID we're displaying. Native UUIDs render as
    // hex like "c0bd0747"; owner-style IDs like "mux-4c43c2-researcher" keep
    // enough prefix ("mux-4c43") to stay distinguishable between siblings.
    id.chars().take(8).collect()
}

fn short_pid_start(s: &str) -> &str {
    if s.is_empty() {
        "unknown lstart"
    } else {
        "live"
    }
}

fn short_lstart(s: &str) -> String {
    if s.is_empty() {
        "(unknown)".into()
    } else {
        s.to_string()
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Cli, HookEventKind, RecentEvent};
    use std::collections::VecDeque;

    fn t(activity: Activity, last_event: f64, id: &str) -> ThreadState {
        ThreadState {
            thread_id: id.into(),
            cli: Cli::Claude,
            activity,
            liveness: Liveness::Live,
            first_seen: 0.0,
            last_event,
            last_check: 0.0,
            pid: 1,
            pid_start: String::new(),
            in_plan_mode: false,
            plan_progress: None,
            last_tool_name: None,
            last_tool_target: None,
            subtitle: Some("Working".into()),
            cwd: None,
            transcript_path: None,
            owner_product: None,
            owner_thread_id: None,
            recent_events: VecDeque::new(),
        }
    }

    #[test]
    fn display_thread_id_prefers_owner_then_truncates() {
        let claude_uuid = "c0bd0747-1234-5678-9abc-def012345678";
        let mut unowned = t(Activity::Idle, 0.0, claude_uuid);
        unowned.owner_thread_id = None;
        assert_eq!(display_thread_id(&unowned), "c0bd0747");

        let mut researcher = t(Activity::Idle, 0.0, claude_uuid);
        researcher.owner_thread_id = Some("mux-4c43c2-researcher".into());
        let mut builder = t(Activity::Idle, 0.0, claude_uuid);
        builder.owner_thread_id = Some("mux-9abc12-builder".into());
        assert_eq!(display_thread_id(&researcher), "mux-4c43");
        assert_eq!(display_thread_id(&builder), "mux-9abc");
        assert_ne!(display_thread_id(&researcher), display_thread_id(&builder));
    }

    #[test]
    fn visible_threads_sort_awaiting_first() {
        let mut threads = std::collections::HashMap::new();
        threads.insert("claude:a".into(), t(Activity::Idle, 100.0, "a"));
        threads.insert("claude:b".into(), t(Activity::AwaitingInput, 10.0, "b"));
        threads.insert("claude:c".into(), t(Activity::Working, 50.0, "c"));
        let state = Some(StateFile {
            schema_version: 1,
            updated_at: 0.0,
            heed_version: "test".into(),
            hook_script_versions: crate::daemon::state_writer::HookScriptVersions {
                claude: "1".into(),
                codex: "1".into(),
            },
            threads,
        });
        let v = visible_threads(&state, false);
        assert_eq!(v[0].activity, Activity::AwaitingInput);
        assert_eq!(v[1].activity, Activity::Working);
        assert_eq!(v[2].activity, Activity::Idle);
    }

    #[test]
    fn move_cursor_clamps_to_bounds() {
        let mut app = App::new();
        let ts: Vec<ThreadState> = vec![
            t(Activity::Working, 1.0, "a"),
            t(Activity::Working, 2.0, "b"),
        ];
        let refs: Vec<&ThreadState> = ts.iter().collect();
        app.selected_key = Some("claude:a".into());
        app.move_cursor(&refs, 1);
        assert_eq!(app.selected_key.as_deref(), Some("claude:b"));
        app.move_cursor(&refs, 1);
        assert_eq!(app.selected_key.as_deref(), Some("claude:b")); // clamped
        app.move_cursor(&refs, -10);
        assert_eq!(app.selected_key.as_deref(), Some("claude:a"));
    }

    #[test]
    fn render_recent_event_formats_tool_target() {
        let line = render_recent_event(&RecentEvent {
            event: HookEventKind::ToolUse,
            ts: 12.345,
            tool_name: Some("Bash".into()),
            tool_target: Some("ls".into()),
        });
        let text = line
            .spans
            .iter()
            .map(|s| s.content.clone().into_owned())
            .collect::<String>();
        assert!(text.contains("tool_use"));
        assert!(text.contains("Bash"));
        assert!(text.contains("ls"));
    }
}
