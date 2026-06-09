//! End-to-end: install → daemon → hook scripts → state.json → uninstall.
//!
//! Uses `assert_cmd` to drive the binary against a tempdir HOME. Verifies
//! the SPEC §17.7 "first usable CLI target" works.

use std::io::Write;
use std::process::Command;
use std::time::Duration;

use assert_cmd::cargo::CommandCargoExt;

fn heed_cmd(home: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("heed").unwrap();
    cmd.env("HOME", home);
    cmd
}

/// Spawn the daemon as a background process; return the child so the test can
/// `kill` it when done. The child also writes `~/.heed/heedd.pid`.
fn spawn_daemon(home: &std::path::Path) -> std::process::Child {
    heed_cmd(home)
        .arg("daemon")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn daemon")
}

fn fire_user_prompt_submit(home: &std::path::Path, session_id: &str) {
    let script = home.join(".heed/claude-hooks/user-prompt-submit.sh");
    let payload = format!(
        r#"{{"session_id":"{session_id}","cwd":"/tmp","transcript_path":"/tmp/transcript.jsonl","hook_event_name":"UserPromptSubmit","prompt":"hi"}}"#
    );
    let mut child = Command::new("bash")
        .arg(&script)
        .env("HOME", home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn hook");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    child.wait().unwrap();
}

#[test]
fn install_then_event_arrives_in_state_file() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    // Make a fake ~/.claude/ so the installer writes settings.json.
    std::fs::create_dir_all(home.join(".claude")).unwrap();

    // install --skip-codex --no-spawn (we'll start the daemon ourselves).
    let status = heed_cmd(home)
        .args(["install", "--skip-codex", "--no-spawn"])
        .status()
        .unwrap();
    assert!(status.success());

    assert!(home
        .join(".heed/claude-hooks/user-prompt-submit.sh")
        .exists());
    assert!(home.join(".claude/settings.json").exists());

    let mut daemon = spawn_daemon(home);

    // Wait for daemon to write an initial state.json.
    let state_path = home.join(".heed/state.json");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !state_path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(state_path.exists(), "daemon never wrote state.json");

    fire_user_prompt_submit(home, "thread-1");

    // Wait for state.json to mention thread-1.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut found = false;
    while std::time::Instant::now() < deadline {
        let Ok(raw) = std::fs::read_to_string(&state_path) else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        if raw.contains("\"thread-1\"") {
            let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
            let t = &v["threads"]["claude:thread-1"];
            assert_eq!(t["activity"], "working");
            assert_eq!(t["cli"], "claude");
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(found, "daemon never reflected the thread-1 event");

    // Now exercise heed status --json to confirm the CLI reads correctly.
    let output = heed_cmd(home).args(["status", "--json"]).output().unwrap();
    assert!(output.status.success(), "status --json failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("thread-1"),
        "status --json missed thread: {stdout}"
    );

    // Send SIGTERM to the daemon for a clean shutdown.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(daemon.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let _ = daemon.wait();

    // Uninstall: settings.json should lose our Heed entries.
    let status = heed_cmd(home)
        .args(["install", "--uninstall", "--skip-codex"])
        .status()
        .unwrap();
    assert!(status.success());
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(".claude/settings.json")).unwrap())
            .unwrap();
    // After uninstall on a settings.json that ONLY had Heed hooks, the
    // hooks key should be gone entirely.
    assert!(
        after.get("hooks").is_none(),
        "expected no hooks key post-uninstall: {after}"
    );
}

#[test]
fn session_end_marks_thread_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    heed_cmd(home)
        .args(["install", "--skip-codex", "--no-spawn"])
        .status()
        .unwrap();

    let mut daemon = spawn_daemon(home);
    std::thread::sleep(Duration::from_millis(300));
    fire_user_prompt_submit(home, "ending");

    // Now fire session-end.sh manually.
    let script = home.join(".heed/claude-hooks/session-end.sh");
    let payload = r#"{"session_id":"ending","cwd":"/tmp"}"#;
    let mut child = Command::new("bash")
        .arg(&script)
        .env("HOME", home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    child.wait().unwrap();

    // Wait for liveness == gone.
    let state_path = home.join(".heed/state.json");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut gone = false;
    while std::time::Instant::now() < deadline {
        if let Ok(raw) = std::fs::read_to_string(&state_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                if v["threads"]["claude:ending"]["liveness"] == "gone" {
                    gone = true;
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(gone, "session_end did not flip liveness to gone within 5s");

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(daemon.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let _ = daemon.wait();
}
