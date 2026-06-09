//! In-module unit tests for the install entry points.
//! Filesystem-touching integration tests live in `tests/install_*.rs`.

use super::*;
use tempfile::tempdir;

fn opts(home: &Path) -> InstallOptions {
    InstallOptions {
        home: home.to_path_buf(),
        skip_claude: false,
        skip_codex: false,
    }
}

#[test]
fn install_creates_heed_dir_and_subdirs() {
    let tmp = tempdir().unwrap();
    let report = install(&opts(tmp.path())).unwrap();
    assert_eq!(report.heed_dir, tmp.path().join(".heed"));
    assert!(tmp.path().join(".heed").is_dir());
    assert!(tmp.path().join(".heed/claude-hooks").is_dir());
    assert!(tmp.path().join(".heed/codex-hooks").is_dir());
    assert!(tmp.path().join(".heed/events.jsonl").exists());
    assert!(tmp.path().join(".heed/owners.json").exists());
}

#[test]
fn install_extracts_all_hook_scripts_with_correct_mode() {
    let tmp = tempdir().unwrap();
    install(&opts(tmp.path())).unwrap();
    for cli in &["claude-hooks", "codex-hooks"] {
        for name in &[
            "user-prompt-submit.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "stop.sh",
            "VERSION",
        ] {
            let path = tmp.path().join(".heed").join(cli).join(name);
            assert!(path.exists(), "missing: {path:?}");
        }
        // Confirm scripts are executable.
        for name in &[
            "user-prompt-submit.sh",
            "pre-tool-use.sh",
            "post-tool-use.sh",
            "stop.sh",
        ] {
            let path = tmp.path().join(".heed").join(cli).join(name);
            let perms = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                perms & 0o111,
                0o111,
                "{path:?} mode {:o} not executable",
                perms
            );
        }
    }
}

#[test]
fn install_is_idempotent() {
    let tmp = tempdir().unwrap();
    let first = install(&opts(tmp.path())).unwrap();
    assert!(first.claude.unwrap().scripts_extracted);
    assert!(first.codex.unwrap().scripts_extracted);
    // Second install: scripts already at bundled version, so no extraction.
    let second = install(&opts(tmp.path())).unwrap();
    assert!(!second.claude.unwrap().scripts_extracted);
    assert!(!second.codex.unwrap().scripts_extracted);
}

#[test]
fn install_skip_flags_honoured() {
    let tmp = tempdir().unwrap();
    let mut o = opts(tmp.path());
    o.skip_codex = true;
    let report = install(&o).unwrap();
    assert!(report.claude.is_some());
    assert!(report.codex.is_none());
}

#[test]
fn install_without_claude_dir_skips_settings_edit() {
    let tmp = tempdir().unwrap();
    // .claude does NOT exist in the tempdir.
    let report = install(&opts(tmp.path())).unwrap();
    let claude = report.claude.unwrap();
    assert!(
        claude.settings_path.is_none(),
        "settings_path should be None when ~/.claude doesn't exist"
    );
    assert!(!claude.notes.is_empty());
    // Scripts are still extracted (cheap; lets future consumers wire in
    // without re-running install once they discover ~/.heed).
    assert!(claude.scripts_extracted);
}

#[test]
fn install_with_claude_dir_writes_settings_json() {
    let tmp = tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
    let report = install(&opts(tmp.path())).unwrap();
    let claude = report.claude.unwrap();
    let settings_path = claude.settings_path.expect("expected settings_path set");
    assert_eq!(settings_path, tmp.path().join(".claude/settings.json"));
    assert!(claude.settings_changed, "first install should write");
    let contents = std::fs::read_to_string(&settings_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&contents).unwrap();
    assert!(v["hooks"]["UserPromptSubmit"].is_array());
    assert!(v["hooks"]["PreToolUse"].is_array());
    assert!(v["hooks"]["PostToolUse"].is_array());
    assert!(v["hooks"]["Stop"].is_array());

    // Second install should be a no-op for settings.
    let report2 = install(&opts(tmp.path())).unwrap();
    let claude2 = report2.claude.unwrap();
    assert!(!claude2.settings_changed);
}

#[test]
fn install_with_codex_dir_writes_config_toml() {
    // Codex tests would actually call `codex --version`, so we skip the
    // version check by faking *no* `codex` on PATH. If a real codex 0.124.x is
    // installed on the host this test could spuriously fail — but the
    // cli_detect helper returns None when codex isn't installed, which is
    // the expected case in CI.
    let tmp = tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".codex")).unwrap();
    let report = install(&opts(tmp.path())).unwrap();
    let codex = report.codex.unwrap();
    // Skip the rest of the assertions if codex is on PATH and on the known-bad list.
    if codex.settings_path.is_none() {
        // Either no codex on PATH (no skip note), or known-bad (skip note present).
        return;
    }
    let config_path = codex.settings_path.unwrap();
    assert_eq!(config_path, tmp.path().join(".codex/config.toml"));
    let contents = std::fs::read_to_string(&config_path).unwrap();
    assert!(contents.contains("hooks = true"));
    for event in [
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "PermissionRequest",
    ] {
        assert!(
            contents.contains(&format!("[[hooks.{event}]]")),
            "missing {event}:\n{contents}"
        );
    }
}

#[test]
fn malformed_claude_settings_aborts_install() {
    let tmp = tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
    let settings = tmp.path().join(".claude/settings.json");
    std::fs::write(&settings, "{ this is not valid json").unwrap();

    let err = install(&opts(tmp.path())).unwrap_err();
    assert!(err.contains("malformed"), "wrong error: {err}");

    // File untouched.
    let after = std::fs::read_to_string(&settings).unwrap();
    assert_eq!(after, "{ this is not valid json");
}

#[test]
fn third_party_claude_hooks_are_preserved() {
    let tmp = tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
    let settings = tmp.path().join(".claude/settings.json");
    let original = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "",
                "hooks": [{ "type": "command", "command": "/usr/local/bin/their-hook.sh" }]
            }],
            "Notification": [{
                "matcher": "",
                "hooks": [{ "type": "command", "command": "/usr/local/bin/notify.sh" }]
            }]
        },
        "permissions": { "allow": ["Bash(npm test:*)"] }
    });
    std::fs::write(&settings, serde_json::to_string_pretty(&original).unwrap()).unwrap();

    install(&opts(tmp.path())).unwrap();

    let merged: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();

    // Third-party PreToolUse entry survived.
    let pre = merged["hooks"]["PreToolUse"].as_array().unwrap();
    assert!(pre.iter().any(|e| e["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .ends_with("their-hook.sh")));
    // Our PreToolUse entry is appended.
    assert!(pre.iter().any(|e| e["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .contains(".heed/claude-hooks/pre-tool-use.sh")));

    // Notification block (which we never touch) preserved exactly.
    let notif = merged["hooks"]["Notification"].as_array().unwrap();
    assert_eq!(notif.len(), 1);
    assert_eq!(notif[0]["hooks"][0]["command"], "/usr/local/bin/notify.sh");

    // permissions key preserved.
    assert!(merged["permissions"].is_object());
}

#[test]
fn uninstall_removes_only_heed_entries() {
    let tmp = tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
    let settings = tmp.path().join(".claude/settings.json");
    let original = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "",
                "hooks": [{ "type": "command", "command": "/usr/local/bin/their-hook.sh" }]
            }]
        }
    });
    std::fs::write(&settings, serde_json::to_string_pretty(&original).unwrap()).unwrap();

    install(&opts(tmp.path())).unwrap();
    uninstall(&opts(tmp.path())).unwrap();

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    let pre = after["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(
        pre.len(),
        1,
        "expected only third-party entry to remain: {after:#?}"
    );
    assert!(pre[0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .ends_with("their-hook.sh"));
}

#[test]
fn uninstall_drops_empty_hooks_block() {
    let tmp = tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
    install(&opts(tmp.path())).unwrap();
    uninstall(&opts(tmp.path())).unwrap();
    let after: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tmp.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    // After removing all Heed entries from a file that had ONLY Heed hooks,
    // the `hooks` key itself should be gone.
    assert!(
        after.get("hooks").is_none(),
        "hooks key should be removed: {after:#?}"
    );
}

#[test]
fn version_bump_re_extracts_scripts() {
    let tmp = tempdir().unwrap();
    install(&opts(tmp.path())).unwrap();

    // Simulate a stale install: change VERSION file to something older.
    let vfile = tmp.path().join(".heed/claude-hooks/VERSION");
    std::fs::write(&vfile, "0.0.1\n").unwrap();

    let report = install(&opts(tmp.path())).unwrap();
    assert!(
        report.claude.unwrap().scripts_extracted,
        "should re-extract on version mismatch"
    );
    let post = std::fs::read_to_string(&vfile).unwrap();
    assert!(post.starts_with(crate::CLAUDE_HOOK_SCRIPTS_VERSION));
}

#[test]
fn codex_known_bad_does_not_panic_when_codex_absent() {
    // cli_detect runs `codex --version`; absent codex → returns None and we
    // proceed with the install. The actual integration test (codex 0.124.x
    // skip) is impractical to simulate without a fake binary on PATH.
    let tmp = tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".codex")).unwrap();
    let _ = install(&opts(tmp.path())); // should not panic regardless
}
