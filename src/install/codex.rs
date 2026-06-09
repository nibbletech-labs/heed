//! Codex installer: safe-edit of `~/.codex/config.toml`.
//!
//! Ported from Codezilla `src-tauri/src/codex_hooks/mod.rs` with Heed changes
//! mirroring `install/claude.rs`: drop USER_DISABLED, drop cli_detect-on-PATH
//! gating, embed scripts via `include_str!`, write via `install::atomic_write`.

use std::fs;
use std::path::Path;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

use super::atomic_write;

/// (event_name, matcher, script_name)
/// Empty matcher means "fire on every event of this kind". PermissionRequest
/// re-uses `pre-tool-use.sh`; the script branches on `hook_event_name` to
/// emit a synthetic `tool_name = "PermissionRequest"`.
const HOOK_REGISTRATIONS: &[(&str, &str, &str)] = &[
    ("UserPromptSubmit", "", "user-prompt-submit.sh"),
    ("PreToolUse", ".*", "pre-tool-use.sh"),
    ("PostToolUse", ".*", "post-tool-use.sh"),
    ("Stop", "", "stop.sh"),
    ("SessionEnd", "", "session-end.sh"),
    ("PermissionRequest", "", "pre-tool-use.sh"),
];

/// True if this `[[hooks.<Event>]]` table's inner `hooks = [{...}]` array
/// references a command path under our scripts dir prefix.
fn table_contains_our_command(table: &Table, scripts_prefix: &str) -> bool {
    let Some(item) = table.get("hooks") else {
        return false;
    };
    if let Some(arr) = item.as_array() {
        return arr.iter().any(|v| {
            v.as_inline_table()
                .and_then(|t| t.get("command"))
                .and_then(|c| c.as_str())
                .map(|s| s.starts_with(scripts_prefix))
                .unwrap_or(false)
        });
    }
    if let Some(aot) = item.as_array_of_tables() {
        return aot.iter().any(|t| {
            t.get("command")
                .and_then(|v| v.as_str())
                .map(|s| s.starts_with(scripts_prefix))
                .unwrap_or(false)
        });
    }
    false
}

/// Merge Heed entries into `config.toml`. Returns `true` if we wrote, `false`
/// on no-op self-heal. Atomic write via tmp+rename.
pub fn ensure_hooks_in_config_toml(config_path: &Path, scripts_dir: &Path) -> Result<bool, String> {
    let scripts_prefix = scripts_dir.to_string_lossy().to_string();

    let existing = if config_path.exists() {
        fs::read_to_string(config_path).map_err(|e| format!("read {:?}: {}", config_path, e))?
    } else {
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("create_dir_all {:?}: {}", parent, e))?;
        }
        String::new()
    };

    let merged = merge_config_toml(&existing, scripts_dir, &scripts_prefix)?;
    if merged == existing {
        return Ok(false);
    }
    atomic_write(config_path, merged.as_bytes())?;
    Ok(true)
}

/// Pure transformation — extracted from [`ensure_hooks_in_config_toml`] so it
/// can be unit-tested without touching the filesystem.
pub(super) fn merge_config_toml(
    source: &str,
    scripts_dir: &Path,
    scripts_prefix: &str,
) -> Result<String, String> {
    let mut doc: DocumentMut = source
        .parse()
        .map_err(|e| format!("config.toml is malformed: {}", e))?;

    // [features].hooks = true and migrate away from deprecated codex_hooks.
    {
        let features_item = doc.entry("features").or_insert(Item::Table(Table::new()));
        let features_table = features_item
            .as_table_mut()
            .ok_or_else(|| "[features] is not a table".to_string())?;
        features_table.set_implicit(false);
        features_table.remove("codex_hooks");
        features_table["hooks"] = value(true);
    }

    // [hooks] table containing one [[hooks.<Event>]] array-of-tables per event.
    let hooks_item = doc.entry("hooks").or_insert(Item::Table(Table::new()));
    let hooks_table = hooks_item
        .as_table_mut()
        .ok_or_else(|| "[hooks] is not a table".to_string())?;
    hooks_table.set_implicit(false);

    for (event_name, matcher, script_name) in HOOK_REGISTRATIONS {
        let script_path = scripts_dir.join(script_name);
        let script_path_str = script_path.to_string_lossy().to_string();

        let event_item = hooks_table
            .entry(event_name)
            .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
        let event_aot = event_item
            .as_array_of_tables_mut()
            .ok_or_else(|| format!("[[hooks.{}]] is not an array-of-tables", event_name))?;

        // Drop stale Heed entries, keep third-party ones.
        let kept: Vec<Table> = event_aot
            .iter()
            .filter(|t| !table_contains_our_command(t, scripts_prefix))
            .cloned()
            .collect();
        while !event_aot.is_empty() {
            event_aot.remove(event_aot.len() - 1);
        }
        for t in kept {
            event_aot.push(t);
        }

        let mut entry = Table::new();
        if !matcher.is_empty() {
            entry["matcher"] = value(*matcher);
        }
        let mut inner_aot = ArrayOfTables::new();
        let mut inner = Table::new();
        inner["type"] = value("command");
        inner["command"] = value(script_path_str.clone());
        inner["timeout"] = value(30i64);
        inner_aot.push(inner);
        entry.insert("hooks", Item::ArrayOfTables(inner_aot));

        event_aot.push(entry);
    }

    Ok(doc.to_string())
}

pub fn remove_hooks_from_config_toml(
    config_path: &Path,
    scripts_dir: &Path,
) -> Result<bool, String> {
    if !config_path.exists() {
        return Ok(false);
    }
    let scripts_prefix = scripts_dir.to_string_lossy().to_string();

    let existing =
        fs::read_to_string(config_path).map_err(|e| format!("read {:?}: {}", config_path, e))?;

    let mut doc: DocumentMut = existing
        .parse()
        .map_err(|e| format!("{:?} is malformed: {}", config_path, e))?;

    if let Some(hooks_item) = doc.get_mut("hooks") {
        if let Some(hooks_table) = hooks_item.as_table_mut() {
            for (_event, item) in hooks_table.iter_mut() {
                if let Some(aot) = item.as_array_of_tables_mut() {
                    let kept: Vec<Table> = aot
                        .iter()
                        .filter(|t| !table_contains_our_command(t, &scripts_prefix))
                        .cloned()
                        .collect();
                    while !aot.is_empty() {
                        aot.remove(aot.len() - 1);
                    }
                    for t in kept {
                        aot.push(t);
                    }
                }
            }
        }
    }

    let serialized = doc.to_string();
    if serialized == existing {
        return Ok(false);
    }
    atomic_write(config_path, serialized.as_bytes())?;
    Ok(true)
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use std::path::PathBuf;

    fn scripts() -> PathBuf {
        PathBuf::from("/home/test/.heed/codex-hooks")
    }

    #[test]
    fn merge_into_empty_config_adds_feature_flag_and_hooks() {
        let merged = merge_config_toml("", &scripts(), "/home/test/.heed/codex-hooks").unwrap();
        assert!(
            merged.contains("hooks = true"),
            "missing feature flag:\n{merged}"
        );
        for event in [
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "Stop",
            "SessionEnd",
            "PermissionRequest",
        ] {
            assert!(
                merged.contains(&format!("[[hooks.{event}]]")),
                "missing {event}:\n{merged}"
            );
        }
        assert!(merged.contains("pre-tool-use.sh"));
        assert!(merged.contains("stop.sh"));
        // PermissionRequest re-uses pre-tool-use.sh.
        let pre_count = merged.matches("/pre-tool-use.sh").count();
        assert_eq!(
            pre_count, 2,
            "pre-tool-use.sh should appear twice (PreToolUse + PermissionRequest)"
        );
    }

    #[test]
    fn merge_preserves_unrelated_user_keys() {
        let source = r#"
[model]
provider = "anthropic"
name = "claude-sonnet-4-5"

[approval]
mode = "trusted"
"#;
        let merged = merge_config_toml(source, &scripts(), "/home/test/.heed/codex-hooks").unwrap();
        assert!(merged.contains("[model]"));
        assert!(merged.contains("provider = \"anthropic\""));
        assert!(merged.contains("[approval]"));
        assert!(merged.contains("hooks = true"));
    }

    #[test]
    fn merge_preserves_third_party_hook_entries() {
        let source = r#"
[features]
hooks = true

[[hooks.PreToolUse]]
matcher = "^Bash$"
hooks = [{ type = "command", command = "/usr/local/bin/their-hook.sh", timeout = 5 }]
"#;
        let merged = merge_config_toml(source, &scripts(), "/home/test/.heed/codex-hooks").unwrap();
        assert!(merged.contains("/usr/local/bin/their-hook.sh"));
        assert!(merged.contains("/home/test/.heed/codex-hooks/pre-tool-use.sh"));
    }

    #[test]
    fn merge_replaces_stale_entries_idempotently() {
        let once = merge_config_toml("", &scripts(), "/home/test/.heed/codex-hooks").unwrap();
        let twice = merge_config_toml(&once, &scripts(), "/home/test/.heed/codex-hooks").unwrap();
        let stop_count = twice
            .matches("/home/test/.heed/codex-hooks/stop.sh")
            .count();
        assert_eq!(
            stop_count, 1,
            "stop.sh appears {stop_count} times:\n{twice}"
        );
    }

    #[test]
    fn merge_migrates_deprecated_codex_hooks_flag() {
        let source = "[features]\ncodex_hooks = true\n";
        let merged = merge_config_toml(source, &scripts(), "/home/test/.heed/codex-hooks").unwrap();
        assert!(merged.contains("hooks = true"));
        assert!(!merged.contains("codex_hooks = true"));
    }

    #[test]
    fn merge_preserves_hooks_state_table() {
        let source = r#"
[features]
hooks = true

[hooks.state]
some_existing_hash = "abc123"
"#;
        let merged = merge_config_toml(source, &scripts(), "/home/test/.heed/codex-hooks").unwrap();
        assert!(
            merged.contains("[hooks.state]"),
            "lost [hooks.state]:\n{merged}"
        );
        assert!(merged.contains("some_existing_hash"));
    }

    #[test]
    fn malformed_toml_returns_error_without_rewriting() {
        let bad = "this is not [valid toml";
        let err = merge_config_toml(bad, &scripts(), "/home/test/.heed/codex-hooks").unwrap_err();
        assert!(err.contains("malformed"), "wrong error: {err}");
    }
}
