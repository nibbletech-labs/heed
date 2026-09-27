use super::*;
use serde_json::json;
use std::io::Write;
use tempfile::tempdir;

fn root(id: &str) -> ThreadState {
    state::initial_state(
        &serde_json::from_value::<HookEvent>(json!({
            "event":"turn_start", "ts":1, "cli":"codex", "thread_id":id,
            "pid":123, "pid_start":"process-start", "cwd":"/repo"
        }))
        .unwrap(),
    )
}
fn meta(id: &str, parent: &str, name: &str) -> Value {
    json!({"timestamp":"2026-09-27T13:00:00Z", "type":"session_meta", "payload":{
        "id":id,"cwd":"/repo","agent_path":name,
        "source":{"subagent":{"thread_spawn":{"parent_thread_id":parent,"agent_path":name}}}
    }})
}
fn ev(second: u32, payload: Value) -> Value {
    json!({"timestamp":format!("2026-09-27T13:00:{second:02}Z"),"type":"event_msg","payload":payload})
}
fn item(second: u32, value: Value) -> Value {
    ev(second, json!({"type":"item_completed","item":value}))
}
fn write(path: &Path, values: &[Value]) {
    std::fs::write(
        path,
        values.iter().map(|v| format!("{v}\n")).collect::<String>(),
    )
    .unwrap();
}
fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

#[test]
fn codex_names_worktrees_writes_and_lifecycle_use_the_claude_contract() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout-child.jsonl");
    write(
        &path,
        &[
            meta("child", "root", "/root/rs1343_build_devices"),
            ev(1, json!({"type":"task_started"})),
            item(
                2,
                json!({"type":"CommandExecution","cwd":"file:///repo/.haven-run/RS-1343","command":["/bin/zsh","-lc","cargo test"]}),
            ),
            item(
                3,
                json!({"type":"FileChange","status":"completed","changes":{"src/app.ts":{"type":"update"}}}),
            ),
        ],
    );
    let mut scanner = Scanner::new(dir.path().into());
    let roots = [root("root")];
    let states = scanner.scan(&roots);
    assert_eq!(states.len(), 1);
    let s = &states[0];
    assert_eq!(s.kind, state::NodeKind::Subagent);
    assert_eq!(s.agent_id.as_deref(), Some("child"));
    assert_eq!(s.thread_id, "root:child");
    assert_eq!(s.parent_thread_id.as_deref(), Some("root"));
    assert_eq!(s.agent_name.as_deref(), Some("/root/rs1343_build_devices"));
    assert!(s.agent_description.is_none());
    assert_eq!(s.activity, Activity::Working);
    assert!(s
        .recent_events
        .iter()
        .any(|e| e.tool_target.as_deref() == Some("/repo/.haven-run/RS-1343")));
    assert_eq!(s.last_tool_name.as_deref(), Some("Edit"));
    assert_eq!(
        s.last_tool_target.as_deref(),
        Some("/repo/.haven-run/RS-1343/src/app.ts")
    );
    assert!(s.owner_product.is_none());
    assert!(
        scanner.scan(&roots).is_empty(),
        "unchanged scans emit nothing"
    );
    append(
        &path,
        &format!("{}\n", ev(4, json!({"type":"task_complete"}))),
    );
    assert_eq!(scanner.scan(&roots)[0].activity, Activity::Idle);
    append(
        &path,
        &format!("{}\n", ev(5, json!({"type":"task_started"}))),
    );
    assert_eq!(scanner.scan(&roots)[0].activity, Activity::Working);
    append(
        &path,
        &format!("{}\n", ev(6, json!({"type":"turn_aborted"}))),
    );
    assert_eq!(scanner.scan(&roots)[0].activity, Activity::Idle);
}

#[test]
fn explicit_parent_links_flatten_descendants_and_isolate_same_cwd_sessions() {
    let dir = tempdir().unwrap();
    write(
        &dir.path().join("rollout-a.jsonl"),
        &[meta("a", "root", "/root/build")],
    );
    write(
        &dir.path().join("rollout-b.jsonl"),
        &[meta("b", "a", "/root/build/verify")],
    );
    write(
        &dir.path().join("rollout-c.jsonl"),
        &[meta("c", "other", "/root/build")],
    );
    write(
        &dir.path().join("rollout-cycle.jsonl"),
        &[meta("cycle", "cycle", "/root/cycle")],
    );
    let mut scanner = Scanner::new(dir.path().into());
    // A child hook may already have made an unowned session called "a".
    let states = scanner.scan(&[root("root"), root("a")]);
    assert_eq!(states.len(), 2);
    assert!(states
        .iter()
        .all(|s| s.parent_thread_id.as_deref() == Some("root")));
    assert!(states
        .iter()
        .any(|s| s.agent_name.as_deref() == Some("/root/build/verify")));
}

#[test]
fn partial_tail_and_malformed_lines_do_not_drop_or_duplicate_events() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout-a.jsonl");
    write(&path, &[meta("a", "root", "/root/worker")]);
    let mut scanner = Scanner::new(dir.path().into());
    let roots = [root("root")];
    scanner.scan(&roots);
    let end = format!("{}\n", ev(3, json!({"type":"task_complete"})));
    append(&path, "bad json\n");
    append(&path, &end[..20]);
    assert!(scanner.scan(&roots).is_empty());
    append(&path, &end[20..]);
    let states = scanner.scan(&roots);
    assert_eq!(states[0].activity, Activity::Idle);
    assert_eq!(states[0].recent_events.len(), 2);
    assert!(scanner.scan(&roots).is_empty());
}

#[test]
fn restart_replays_existing_children_and_parent_death_settles_them() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout-a.jsonl");
    write(
        &path,
        &[
            meta("a", "root", "/root/worker"),
            ev(2, json!({"type":"task_complete"})),
        ],
    );
    let roots = [root("root")];
    for _ in 0..2 {
        let mut scanner = Scanner::new(dir.path().into());
        assert_eq!(scanner.scan(&roots)[0].activity, Activity::Idle);
    }
    append(
        &path,
        &format!("{}\n", ev(3, json!({"type":"task_started"}))),
    );
    let mut scanner = Scanner::new(dir.path().into());
    scanner.scan(&roots);
    let mut dead = root("root");
    dead.liveness = Liveness::Gone;
    let updates = scanner.scan(&[dead]);
    assert_eq!(updates[0].liveness, Liveness::Gone);
    assert_eq!(updates[0].activity, Activity::Idle);
}

#[test]
fn truncation_replacement_and_larger_rewrite_rebuild_state() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout-a.jsonl");
    let start = meta("a", "root", "/root/worker");
    write(
        &path,
        &[start.clone(), ev(2, json!({"type":"task_complete"}))],
    );
    let mut scanner = Scanner::new(dir.path().into());
    let roots = [root("root")];
    assert_eq!(scanner.scan(&roots)[0].activity, Activity::Idle);
    // Rewrite past the old cursor: size alone cannot detect this.
    write(
        &path,
        &[
            start.clone(),
            ev(
                3,
                json!({"type":"task_started","padding":"abcdefghijklmnopqrstuvwxyz"}),
            ),
        ],
    );
    assert_eq!(scanner.scan(&roots)[0].activity, Activity::Working);
    write(&path, std::slice::from_ref(&start));
    assert_eq!(scanner.scan(&roots)[0].recent_events.len(), 1);
    let replacement = dir.path().join("new");
    write(
        &replacement,
        &[start, ev(4, json!({"type":"task_complete"}))],
    );
    std::fs::rename(replacement, &path).unwrap();
    assert_eq!(scanner.scan(&roots)[0].activity, Activity::Idle);
}

#[test]
fn failed_edits_and_encrypted_messages_are_not_claimed_as_writes_or_descriptions() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout-a.jsonl");
    write(
        &path,
        &[
            meta("a", "root", "/root/worker"),
            item(
                1,
                json!({"type":"FileChange","status":"failed","changes":{"bad.ts":{"type":"add"}}}),
            ),
            json!({"timestamp":"2026-09-27T13:00:02Z","type":"response_item","payload":{"type":"message","content":[{"type":"encrypted_content","encrypted_content":"opaque RS-999"}]}}),
            item(
                3,
                json!({"type":"CommandExecution","cwd":"file:///repo/some%20dir/.haven-run/RS-1343","command":["pwd"]}),
            ),
        ],
    );
    let mut scanner = Scanner::new(dir.path().into());
    let updates = scanner.scan(&[root("root")]);
    let s = &updates[0];
    assert!(s.agent_description.is_none());
    assert!(!s
        .recent_events
        .iter()
        .any(|e| e.tool_name.as_deref() == Some("Edit")));
    assert_eq!(s.cwd.as_deref(), Some("/repo/some dir/.haven-run/RS-1343"));
}

#[test]
fn oversized_lines_are_skipped_without_wedging_later_records() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rollout-a.jsonl");
    write(&path, &[meta("a", "root", "/root/worker")]);
    append(&path, &"x".repeat(MAX_LINE + 1));
    append(
        &path,
        &format!("\n{}\n", ev(3, json!({"type":"task_complete"}))),
    );
    let mut scanner = Scanner::new(dir.path().into());
    assert_eq!(scanner.scan(&[root("root")])[0].activity, Activity::Idle);
}
