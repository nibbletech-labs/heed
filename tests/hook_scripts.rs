//! Shell-fixture tests for the bundled hook scripts.
//!
//! Each test pipes a captured (or synthetic) hook payload into one of the
//! scripts via `bash`, redirects `$HOME` to a tempdir so the script's
//! `$HOME/.heed/events.jsonl` lands somewhere we control, then parses the
//! resulting JSONL line and asserts on the fields the daemon will care about.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture(name: &str) -> String {
    let path = repo_root().join("tests/fixtures/hooks").join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e))
}

fn script(cli: &str, name: &str) -> PathBuf {
    repo_root()
        .join("resources")
        .join(format!("{cli}-hooks"))
        .join(name)
}

/// Run a hook script with the given stdin and a fresh `$HOME` tempdir.
/// Returns the single JSON line that was appended to `$HOME/.heed/events.jsonl`,
/// parsed as a `serde_json::Value`.
fn run_hook(script_path: &Path, stdin: &str) -> serde_json::Value {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();

    let mut child = Command::new("bash")
        .arg(script_path)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bash");

    child
        .stdin
        .as_mut()
        .expect("child stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("wait child");
    assert!(
        output.status.success(),
        "{}: status={:?} stderr={}",
        script_path.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let log_path = home.join(".heed/events.jsonl");
    let contents = fs::read_to_string(&log_path).unwrap_or_else(|e| {
        panic!(
            "expected {} to exist: {} (stderr: {})",
            log_path.display(),
            e,
            String::from_utf8_lossy(&output.stderr)
        )
    });

    let line = contents
        .lines()
        .next()
        .unwrap_or_else(|| panic!("event log empty after {}", script_path.display()));
    serde_json::from_str(line).unwrap_or_else(|e| panic!("parse {:?}: {}", line, e))
}

/// Spawn a hook and assert that NO line was written (silent exit-0).
fn run_hook_expect_silent(script_path: &Path, stdin: &str) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();

    let mut child = Command::new("bash")
        .arg(script_path)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bash");

    child
        .stdin
        .as_mut()
        .expect("child stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("wait child");
    assert!(output.status.success(), "expected silent exit-0");

    let log_path = home.join(".heed/events.jsonl");
    // The script may or may not have created the directory before bailing.
    // What matters is that no events were appended.
    if log_path.exists() {
        let contents = fs::read_to_string(&log_path).unwrap();
        assert!(
            contents.is_empty(),
            "expected empty event log; got: {contents}",
        );
    }
}

// --- Claude --------------------------------------------------------------

#[test]
fn claude_user_prompt_submit_emits_turn_start() {
    let v = run_hook(
        &script("claude", "user-prompt-submit.sh"),
        &fixture("claude-user-prompt-submit.json"),
    );
    assert_eq!(v["event"], "turn_start");
    assert_eq!(v["cli"], "claude");
    assert_eq!(v["thread_id"], "72881721-ea1f-4824-a7a4-33d4011f2106");
    assert_eq!(v["cwd"], "/Users/tom/Local_Projects/heed");
    assert!(v["transcript_path"].as_str().unwrap().ends_with(".jsonl"));
    assert!(v["ts"].as_f64().unwrap() > 0.0);
    assert!(v["pid"].as_u64().unwrap() > 0);
    // pid_start may be empty under unusual ps configurations but should
    // normally be a non-empty string on macOS/Linux.
    assert!(v["pid_start"].is_string());
}

#[test]
fn claude_pre_tool_use_bash_carries_command_target() {
    let v = run_hook(
        &script("claude", "pre-tool-use.sh"),
        &fixture("claude-pre-tool-use-bash.json"),
    );
    assert_eq!(v["event"], "pre_tool_use");
    assert_eq!(v["cli"], "claude");
    assert_eq!(v["extra"]["tool_name"], "Bash");
    assert_eq!(v["extra"]["tool_target"], "cargo test");
}

#[test]
fn claude_pre_tool_use_read_carries_file_path_target() {
    let v = run_hook(
        &script("claude", "pre-tool-use.sh"),
        &fixture("claude-pre-tool-use-read.json"),
    );
    assert_eq!(v["extra"]["tool_name"], "Read");
    assert_eq!(
        v["extra"]["tool_target"],
        "/Users/tom/Local_Projects/heed/Cargo.toml"
    );
}

#[test]
fn claude_pre_tool_use_extracts_field_after_nested_object() {
    // Regression: the old `[^}]*` constraint scoped to "tool_input" terminated
    // at the first inner `}`, silently emptying tool_target whenever upstream
    // ordered any object-typed field before our target. Synthesise that shape
    // explicitly so this stays caught even if real captures don't trip it.
    let payload = r#"{"session_id":"s","cwd":"/tmp","transcript_path":"/tmp/t.jsonl","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"opts":{"timeout":30,"shell":"/bin/bash"},"command":"cargo test"}}"#;
    let v = run_hook(&script("claude", "pre-tool-use.sh"), payload);
    assert_eq!(v["extra"]["tool_name"], "Bash");
    assert_eq!(v["extra"]["tool_target"], "cargo test");
}

#[test]
fn claude_post_tool_use_todowrite_ignores_tool_response_echoes() {
    // Regression: counting "status":"..." across the full payload would
    // double-count if upstream's tool_response echoes the todos (which it
    // does in real Claude payloads as oldTodos/newTodos). Scope to the
    // tool_input slice only.
    let payload = r#"{"session_id":"s","cwd":"/tmp","transcript_path":"/tmp/t.jsonl","hook_event_name":"PostToolUse","tool_name":"TodoWrite","tool_input":{"todos":[{"content":"a","status":"completed","activeForm":"x"},{"content":"b","status":"pending","activeForm":"y"}]},"tool_response":{"oldTodos":[{"content":"a","status":"pending","activeForm":"x"},{"content":"b","status":"pending","activeForm":"y"}],"newTodos":[{"content":"a","status":"completed","activeForm":"x"},{"content":"b","status":"pending","activeForm":"y"}]}}"#;
    let v = run_hook(&script("claude", "post-tool-use.sh"), payload);
    assert_eq!(v["extra"]["tool_name"], "TodoWrite");
    assert_eq!(v["extra"]["todos_total"], 2);
    assert_eq!(v["extra"]["todos_done"], 1);
}

#[test]
fn claude_pre_tool_use_ask_user_question_has_no_target() {
    let v = run_hook(
        &script("claude", "pre-tool-use.sh"),
        &fixture("claude-pre-tool-use-ask-user-question.json"),
    );
    assert_eq!(v["extra"]["tool_name"], "AskUserQuestion");
    assert!(v["extra"].get("tool_target").is_none());
}

#[test]
fn claude_post_tool_use_bash_emits_tool_use() {
    let v = run_hook(
        &script("claude", "post-tool-use.sh"),
        &fixture("claude-post-tool-use-bash.json"),
    );
    assert_eq!(v["event"], "tool_use");
    assert_eq!(v["extra"]["tool_name"], "Bash");
    assert_eq!(v["extra"]["tool_target"], "cargo test");
}

#[test]
fn claude_post_tool_use_taskupdate_carries_status() {
    let v = run_hook(
        &script("claude", "post-tool-use.sh"),
        &fixture("claude-post-tool-use-taskupdate-completed.json"),
    );
    assert_eq!(v["extra"]["tool_name"], "TaskUpdate");
    assert_eq!(v["extra"]["task_status"], "completed");
}

#[test]
fn claude_post_tool_use_todowrite_counts_todos() {
    let v = run_hook(
        &script("claude", "post-tool-use.sh"),
        &fixture("claude-post-tool-use-todowrite.json"),
    );
    assert_eq!(v["extra"]["tool_name"], "TodoWrite");
    assert_eq!(v["extra"]["todos_total"], 3);
    assert_eq!(v["extra"]["todos_done"], 1);
}

#[test]
fn claude_stop_emits_turn_end() {
    let v = run_hook(&script("claude", "stop.sh"), &fixture("claude-stop.json"));
    assert_eq!(v["event"], "turn_end");
    assert_eq!(v["cli"], "claude");
    assert_eq!(v["thread_id"], "72881721-ea1f-4824-a7a4-33d4011f2106");
    assert!(v["transcript_path"].is_string());
}

#[test]
fn claude_session_end_emits_session_end() {
    let v = run_hook(
        &script("claude", "session-end.sh"),
        &fixture("claude-stop.json"), // reuses the same session_id payload shape
    );
    assert_eq!(v["event"], "session_end");
    assert_eq!(v["cli"], "claude");
    assert_eq!(v["thread_id"], "72881721-ea1f-4824-a7a4-33d4011f2106");
}

#[test]
fn codex_session_end_emits_session_end() {
    let v = run_hook(
        &script("codex", "session-end.sh"),
        &fixture("codex-user-prompt-submit.json"),
    );
    assert_eq!(v["event"], "session_end");
    assert_eq!(v["cli"], "codex");
    assert_eq!(v["thread_id"], "019e1c62-3bf1-7322-b3e5-da99891037c3");
}

#[test]
fn claude_missing_session_id_exits_silently() {
    let payload = r#"{"cwd":"/tmp","prompt":"hi"}"#;
    for s in &[
        "user-prompt-submit.sh",
        "pre-tool-use.sh",
        "post-tool-use.sh",
        "stop.sh",
        "session-end.sh",
    ] {
        run_hook_expect_silent(&script("claude", s), payload);
    }
}

// --- Codex ---------------------------------------------------------------

#[test]
fn codex_user_prompt_submit_emits_turn_start() {
    let v = run_hook(
        &script("codex", "user-prompt-submit.sh"),
        &fixture("codex-user-prompt-submit.json"),
    );
    assert_eq!(v["event"], "turn_start");
    assert_eq!(v["cli"], "codex");
    assert_eq!(v["thread_id"], "019e1c62-3bf1-7322-b3e5-da99891037c3");
}

#[test]
fn codex_pre_tool_use_bash_carries_command() {
    let v = run_hook(
        &script("codex", "pre-tool-use.sh"),
        &fixture("codex-pre-tool-use-bash.json"),
    );
    assert_eq!(v["event"], "pre_tool_use");
    assert_eq!(v["cli"], "codex");
    assert_eq!(v["extra"]["tool_name"], "Bash");
    assert_eq!(v["extra"]["tool_target"], "ls -la");
}

#[test]
fn codex_permission_request_synthesizes_tool_name() {
    let v = run_hook(
        &script("codex", "pre-tool-use.sh"),
        &fixture("codex-permission-request.json"),
    );
    assert_eq!(v["event"], "pre_tool_use");
    assert_eq!(v["extra"]["tool_name"], "PermissionRequest");
    // Permission requests carry no tool_target.
    assert!(v["extra"].get("tool_target").is_none());
}

#[test]
fn codex_missing_session_id_exits_silently() {
    let payload = r#"{"cwd":"/tmp"}"#;
    for s in &[
        "user-prompt-submit.sh",
        "pre-tool-use.sh",
        "post-tool-use.sh",
        "stop.sh",
        "session-end.sh",
    ] {
        run_hook_expect_silent(&script("codex", s), payload);
    }
}

// --- Output shape invariants --------------------------------------------

#[test]
fn output_is_exactly_one_line_per_invocation() {
    let v = run_hook(
        &script("claude", "user-prompt-submit.sh"),
        &fixture("claude-user-prompt-submit.json"),
    );
    // run_hook already asserts a parseable first line. Re-run and check that
    // the file contains exactly one trailing newline.
    let tmp = tempfile::tempdir().unwrap();
    let mut child = Command::new("bash")
        .arg(script("claude", "user-prompt-submit.sh"))
        .env("HOME", tmp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(fixture("claude-user-prompt-submit.json").as_bytes())
        .unwrap();
    let _ = child.wait_with_output().unwrap();
    let contents = fs::read_to_string(tmp.path().join(".heed/events.jsonl")).unwrap();
    assert_eq!(contents.lines().count(), 1, "expected one line");
    assert!(contents.ends_with('\n'), "expected trailing newline");
    // Smoke check that v is well-formed.
    assert!(v["event"].is_string());
}
