//! Claude Code installer: safe-edit of `~/.claude/settings.json`.
//!
//! Ported from Codezilla `src-tauri/src/claude_hooks/mod.rs` with these changes:
//! - Drop `USER_DISABLED` marker file and `cli_detect` gating (per Heed spec
//!   §8: the gate is *directory existence*, not CLI-on-PATH).
//! - Drop Tauri-specific embedding; scripts come from `include_str!`.
//! - Rename `~/.codezilla/` references to `~/.heed/`.
//! - Atomic-write helper lives in `install::mod`.

use serde_json::{json, Value};
use std::fs;
use std::path::Path;

use super::atomic_write;

/// Build the four-entry hooks block we want to merge into settings.json.
pub(super) fn build_hooks_block(scripts_dir: &Path) -> Value {
    let entry = |script: &str| -> Value {
        let path = scripts_dir.join(script);
        json!({
            "matcher": "",
            "hooks": [
                { "type": "command", "command": path.to_string_lossy() }
            ]
        })
    };
    json!({
        "UserPromptSubmit": [entry("user-prompt-submit.sh")],
        "PreToolUse": [entry("pre-tool-use.sh")],
        "PostToolUse": [entry("post-tool-use.sh")],
        "Stop": [entry("stop.sh")],
        "SessionEnd": [entry("session-end.sh")],
    })
}

/// True if a hook-entry table's inner `hooks[*].command` path starts with our
/// scripts dir. Used to identify entries we own across (un)installs.
fn is_our_hook_entry(entry: &Value, scripts_dir_prefix: &str) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|arr| {
            arr.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(|s| s.starts_with(scripts_dir_prefix))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Read settings.json, merge Heed's hooks block in (preserving any third-party
/// entries), atomic-write the result. Returns `true` if we wrote, `false` on
/// no-op self-heal.
///
/// Errors on malformed JSON — we never rewrite a file we can't parse.
pub fn ensure_hooks_in_settings_json(
    settings_path: &Path,
    scripts_dir: &Path,
) -> Result<bool, String> {
    let scripts_dir_str = scripts_dir.to_string_lossy().to_string();

    let existing = if settings_path.exists() {
        let raw = fs::read_to_string(settings_path)
            .map_err(|e| format!("read {:?}: {}", settings_path, e))?;
        // Empty file → start from {}; otherwise parse strictly.
        if raw.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str::<Value>(&raw)
                .map_err(|e| format!("{:?} is malformed: {}", settings_path, e))?
        }
    } else {
        if let Some(parent) = settings_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("create_dir_all {:?}: {}", parent, e))?;
        }
        json!({})
    };

    let mut merged = existing.clone();
    let merged_obj = merged
        .as_object_mut()
        .ok_or_else(|| format!("{:?} root is not a JSON object", settings_path))?;

    let desired = build_hooks_block(scripts_dir);

    let hooks_entry = merged_obj
        .entry("hooks".to_string())
        .or_insert_with(|| json!({}));
    let hooks_obj = hooks_entry
        .as_object_mut()
        .ok_or_else(|| "settings.json `hooks` key is not an object".to_string())?;

    for (event_name, desired_entries) in desired.as_object().unwrap() {
        let arr_entry = hooks_obj
            .entry(event_name.clone())
            .or_insert_with(|| json!([]));
        let arr = arr_entry
            .as_array_mut()
            .ok_or_else(|| format!("settings.json `hooks.{}` is not an array", event_name))?;
        // Drop any stale Heed entries (prefix match), keep third-party entries.
        arr.retain(|entry| !is_our_hook_entry(entry, &scripts_dir_str));
        for new_entry in desired_entries.as_array().unwrap() {
            arr.push(new_entry.clone());
        }
    }

    if merged == existing {
        return Ok(false);
    }

    let serialized = serde_json::to_string_pretty(&merged)
        .map_err(|e| format!("serialize settings.json: {}", e))?;
    // Match the on-disk convention of `~/.claude/settings.json` (Claude
    // writes it with a trailing newline).
    let mut bytes = serialized.into_bytes();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    atomic_write(settings_path, &bytes)?;
    Ok(true)
}

/// Remove Heed entries from settings.json. No-op if file missing or our
/// entries aren't present.
pub fn remove_hooks_from_settings_json(
    settings_path: &Path,
    scripts_dir: &Path,
) -> Result<bool, String> {
    if !settings_path.exists() {
        return Ok(false);
    }
    let scripts_dir_str = scripts_dir.to_string_lossy().to_string();

    let raw = fs::read_to_string(settings_path)
        .map_err(|e| format!("read {:?}: {}", settings_path, e))?;
    if raw.trim().is_empty() {
        return Ok(false);
    }
    let existing: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("{:?} is malformed: {}", settings_path, e))?;

    let mut merged = existing.clone();
    let Some(merged_obj) = merged.as_object_mut() else {
        return Ok(false);
    };
    let Some(hooks) = merged_obj.get_mut("hooks") else {
        return Ok(false);
    };
    let Some(hooks_obj) = hooks.as_object_mut() else {
        return Ok(false);
    };

    for (_event, val) in hooks_obj.iter_mut() {
        if let Some(arr) = val.as_array_mut() {
            arr.retain(|entry| !is_our_hook_entry(entry, &scripts_dir_str));
        }
    }
    // Drop event arrays we emptied, and drop `hooks` if it's now empty.
    hooks_obj.retain(|_k, v| v.as_array().map(|a| !a.is_empty()).unwrap_or(true));
    if hooks_obj.is_empty() {
        merged_obj.remove("hooks");
    }

    if merged == existing {
        return Ok(false);
    }

    let serialized = serde_json::to_string_pretty(&merged)
        .map_err(|e| format!("serialize settings.json: {}", e))?;
    let mut bytes = serialized.into_bytes();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    atomic_write(settings_path, &bytes)?;
    Ok(true)
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn build_hooks_block_shape() {
        let scripts = PathBuf::from("/home/test/.heed/claude-hooks");
        let block = build_hooks_block(&scripts);
        let obj = block.as_object().unwrap();
        assert!(obj.contains_key("UserPromptSubmit"));
        assert!(obj.contains_key("PreToolUse"));
        assert!(obj.contains_key("PostToolUse"));
        assert!(obj.contains_key("Stop"));
        let stop_arr = obj["Stop"].as_array().unwrap();
        let cmd = stop_arr[0]["hooks"].as_array().unwrap()[0]["command"]
            .as_str()
            .unwrap();
        assert_eq!(cmd, "/home/test/.heed/claude-hooks/stop.sh");
    }

    #[test]
    fn is_our_hook_entry_detection() {
        let prefix = "/home/test/.heed";
        let ours = json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": "/home/test/.heed/claude-hooks/stop.sh" }]
        });
        let theirs = json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": "/some/third-party/hook.sh" }]
        });
        assert!(is_our_hook_entry(&ours, prefix));
        assert!(!is_our_hook_entry(&theirs, prefix));
    }
}
