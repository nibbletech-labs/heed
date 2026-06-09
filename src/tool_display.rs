//! Tool subtitle derivation. Ported from Codezilla `src/lib/toolDisplay.ts`.
//! Used by `heed status`, `heed watch`, `heed tui` to render the per-thread
//! subtitle ("Reading package.json", "Running npm test", "Editing files").

use crate::state::{Activity, PlanProgress, ThreadState};

const TOOL_VERBS: &[(&str, &str)] = &[
    // Claude tools
    ("Edit", "Editing"),
    ("Write", "Writing"),
    ("Read", "Reading"),
    ("Bash", "Running"),
    ("Grep", "Searching"),
    ("Glob", "Searching"),
    ("WebSearch", "Searching"),
    ("WebFetch", "Fetching"),
    ("Task", "Delegating"),
    ("TodoWrite", "Updating plan"),
    ("TaskCreate", "Planning"),
    ("TaskUpdate", "Executing plan"),
    ("TaskList", "Checking plan"),
    ("TaskGet", "Checking task"),
    // Codex tools
    ("apply_patch", "Editing files"),
    ("PermissionRequest", "Awaiting input"),
];

fn verb_for(name: &str) -> Option<&'static str> {
    TOOL_VERBS
        .iter()
        .find_map(|(k, v)| (*k == name).then_some(*v))
}

fn short_path(p: &str) -> &str {
    p.rsplit_once('/').map_or(p, |(_, tail)| tail)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let prefix: String = s.chars().take(max).collect();
        format!("{prefix}...")
    } else {
        s.to_string()
    }
}

/// Quote-aware shell tokenizer (mirrors `toolDisplay.ts::tokenize`).
fn tokenize(cmd: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in cmd.chars() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                cur.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
        } else if ch == ' ' || ch == '\t' {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

fn last_positional(tokens: &[String]) -> Option<&str> {
    for t in tokens.iter().rev() {
        if !t.is_empty() && !t.starts_with('-') {
            return Some(t.as_str());
        }
    }
    None
}

fn first_positional(tokens: &[String]) -> Option<&str> {
    for t in tokens.iter().skip(1) {
        if !t.is_empty() && !t.starts_with('-') {
            return Some(t.as_str());
        }
    }
    None
}

#[derive(Clone, Debug)]
pub struct Pretty {
    pub verb: String,
    pub target: Option<String>,
}

/// Recognize common Bash shapes. Returns `None` if no pattern matches —
/// callers fall back to "Running <truncated cmd>".
pub fn simplify_bash_command(cmd: &str) -> Option<Pretty> {
    if cmd.is_empty() {
        return None;
    }
    // Take only the first segment of a pipeline / chain.
    let first = cmd.split(['|', '&', ';']).next().unwrap_or(cmd).trim();
    let tokens = tokenize(first);
    if tokens.is_empty() {
        return None;
    }
    let head = tokens[0].as_str();
    match head {
        "cat" | "head" | "tail" | "less" | "more" | "bat" => Some(Pretty {
            verb: "Reading".into(),
            target: last_positional(&tokens).map(|p| short_path(p).to_string()),
        }),
        "sed" => {
            let positionals: Vec<&String> = tokens
                .iter()
                .skip(1)
                .filter(|t| !t.starts_with('-'))
                .collect();
            if positionals.len() < 2 {
                return None;
            }
            Some(Pretty {
                verb: "Reading".into(),
                target: last_positional(&tokens).map(|p| short_path(p).to_string()),
            })
        }
        "rg" | "grep" => {
            if tokens.iter().any(|t| t == "--files") {
                return Some(Pretty {
                    verb: "Listing".into(),
                    target: Some("files".into()),
                });
            }
            Some(Pretty {
                verb: "Searching".into(),
                target: first_positional(&tokens).map(|p| truncate(p, 30)),
            })
        }
        "find" => Some(Pretty {
            verb: "Searching".into(),
            target: first_positional(&tokens).map(|p| short_path(p).to_string()),
        }),
        "ls" => Some(Pretty {
            verb: "Listing".into(),
            target: first_positional(&tokens).map(|p| short_path(p).to_string()),
        }),
        "pwd" => Some(Pretty {
            verb: "Checking cwd".into(),
            target: None,
        }),
        "git" => {
            let sub = tokens.get(1)?;
            if sub.starts_with('-') {
                return None;
            }
            Some(Pretty {
                verb: format!("Git {sub}"),
                target: None,
            })
        }
        "mkdir" => Some(Pretty {
            verb: "Creating".into(),
            target: last_positional(&tokens).map(|p| short_path(p).to_string()),
        }),
        "rm" => Some(Pretty {
            verb: "Removing".into(),
            target: last_positional(&tokens).map(|p| short_path(p).to_string()),
        }),
        "mv" => Some(Pretty {
            verb: "Moving".into(),
            target: first_positional(&tokens).map(|p| short_path(p).to_string()),
        }),
        "cp" => Some(Pretty {
            verb: "Copying".into(),
            target: first_positional(&tokens).map(|p| short_path(p).to_string()),
        }),
        _ => None,
    }
}

/// Format the tool subtitle (mirrors `toolDisplay.ts::formatToolSubtitle`).
pub fn format_tool_subtitle(name: &str, target: Option<&str>) -> String {
    // MCP tools: `mcp__<server>__<tool>` → "Calling <tool>".
    if let Some(rest) = name.strip_prefix("mcp__") {
        let last = rest.rsplit("__").next().unwrap_or(name);
        return format!("Calling {}", truncate(last, 40));
    }
    if name == "Bash" {
        if let Some(t) = target {
            if let Some(pretty) = simplify_bash_command(t) {
                return match pretty.target {
                    Some(target) => format!("{} {}", pretty.verb, target),
                    None => pretty.verb,
                };
            }
            return format!("Running {}", truncate(t, 40));
        }
        return "Running".into();
    }
    let verb = verb_for(name)
        .map(str::to_string)
        .unwrap_or_else(|| format!("Using {name}"));
    match target {
        Some(t) => format!("{verb} {}", short_path(t)),
        None => verb,
    }
}

/// Subtitle for a whole thread state — combines tool subtitle, plan-mode
/// prefix, and the awaiting/idle fallbacks per SPEC §4.4.
pub fn format_for_thread(state: &ThreadState) -> String {
    let plan_prefix = if state.in_plan_mode {
        Some("Plan mode")
    } else {
        None
    };

    let base = match state.activity {
        Activity::AwaitingInput => "Awaiting input".to_string(),
        Activity::Idle => "Idle".to_string(),
        Activity::Working => match (
            state.last_tool_name.as_deref(),
            state.last_tool_target.as_deref(),
        ) {
            (Some(name), target) => format_tool_subtitle(name, target),
            (None, _) => "Working".to_string(),
        },
    };

    match plan_prefix {
        Some(prefix) => format!("{prefix} · {base}"),
        None => base,
    }
}

/// Compact plan-progress accessor for renderers (`{done}/{total}` style).
pub fn plan_progress_label(p: &PlanProgress) -> String {
    format!("{}/{}", p.done, p.total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_uses_filename() {
        assert_eq!(
            format_tool_subtitle("Read", Some("/path/to/package.json")),
            "Reading package.json"
        );
    }

    #[test]
    fn bash_cat_reads_filename() {
        assert_eq!(
            format_tool_subtitle("Bash", Some("cat package.json")),
            "Reading package.json"
        );
    }

    #[test]
    fn bash_unknown_falls_back_to_running_truncated() {
        let cmd = "frobnicate --some --flags arg1 arg2 arg3 arg4 arg5 arg6 arg7";
        let subtitle = format_tool_subtitle("Bash", Some(cmd));
        assert!(subtitle.starts_with("Running "));
        // Truncated at 40 chars per shape; the original is > 40.
        assert!(subtitle.contains("..."));
    }

    #[test]
    fn bash_pipeline_uses_first_segment() {
        let s = format_tool_subtitle("Bash", Some("cat foo.json | jq '.x'"));
        assert_eq!(s, "Reading foo.json");
    }

    #[test]
    fn rg_files_lists_files() {
        assert_eq!(
            format_tool_subtitle("Bash", Some("rg --files src/")),
            "Listing files"
        );
    }

    #[test]
    fn rg_with_pattern_searches() {
        let s = format_tool_subtitle("Bash", Some("rg 'foo.*bar'"));
        assert!(s.starts_with("Searching "), "got: {s}");
        assert!(s.contains("foo"));
    }

    #[test]
    fn git_subcommand() {
        assert_eq!(
            format_tool_subtitle("Bash", Some("git status")),
            "Git status"
        );
    }

    #[test]
    fn mcp_tool_renders_calling() {
        assert_eq!(
            format_tool_subtitle("mcp__supabase__list_tables", None),
            "Calling list_tables"
        );
    }

    #[test]
    fn apply_patch_renders_editing_files() {
        assert_eq!(format_tool_subtitle("apply_patch", None), "Editing files");
    }

    #[test]
    fn unknown_tool_uses_fallback_verb() {
        assert_eq!(format_tool_subtitle("CustomTool", None), "Using CustomTool");
        assert_eq!(
            format_tool_subtitle("CustomTool", Some("/foo/bar.txt")),
            "Using CustomTool bar.txt"
        );
    }

    #[test]
    fn awaiting_input_renders_awaiting_input_label() {
        let mut state = ThreadState {
            thread_id: "t".into(),
            cli: crate::state::Cli::Claude,
            activity: Activity::AwaitingInput,
            liveness: crate::state::Liveness::Live,
            first_seen: 0.0,
            last_event: 0.0,
            last_check: 0.0,
            pid: 1,
            pid_start: "".into(),
            in_plan_mode: false,
            plan_progress: None,
            last_tool_name: None,
            last_tool_target: None,
            subtitle: None,
            cwd: None,
            transcript_path: None,
            owner_product: None,
            owner_thread_id: None,
            recent_events: Default::default(),
        };
        assert_eq!(format_for_thread(&state), "Awaiting input");
        state.activity = Activity::Idle;
        assert_eq!(format_for_thread(&state), "Idle");
        state.activity = Activity::Working;
        state.last_tool_name = Some("Bash".into());
        state.last_tool_target = Some("npm test".into());
        assert_eq!(format_for_thread(&state), "Running npm test");
    }

    #[test]
    fn plan_mode_prefix() {
        let mut state = ThreadState {
            thread_id: "t".into(),
            cli: crate::state::Cli::Claude,
            activity: Activity::Working,
            liveness: crate::state::Liveness::Live,
            first_seen: 0.0,
            last_event: 0.0,
            last_check: 0.0,
            pid: 1,
            pid_start: "".into(),
            in_plan_mode: true,
            plan_progress: None,
            last_tool_name: Some("Read".into()),
            last_tool_target: Some("foo.rs".into()),
            subtitle: None,
            cwd: None,
            transcript_path: None,
            owner_product: None,
            owner_thread_id: None,
            recent_events: Default::default(),
        };
        assert_eq!(format_for_thread(&state), "Plan mode · Reading foo.rs");
        state.activity = Activity::AwaitingInput;
        assert_eq!(format_for_thread(&state), "Plan mode · Awaiting input");
    }

    #[test]
    fn plan_progress_label_renders_done_over_total() {
        let p = PlanProgress { total: 4, done: 1 };
        assert_eq!(plan_progress_label(&p), "1/4");
    }
}
