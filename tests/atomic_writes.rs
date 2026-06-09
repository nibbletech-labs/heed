//! SPEC §14.5: stress test for state.json under concurrent read+write load.
//!
//! Spawns 10 reader threads that each parse state.json 100 times while
//! a writer thread rewrites it 100 times. Every read must yield a parseable
//! JSON value — atomic tmp+rename should make this guarantee hold.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use heed::daemon::state_writer::{build_state_file, write};
use heed::state::{Activity, Cli, Liveness, ThreadState};

fn dummy_thread(id: &str) -> ThreadState {
    ThreadState {
        thread_id: id.into(),
        cli: Cli::Claude,
        activity: Activity::Working,
        liveness: Liveness::Live,
        first_seen: 1.0,
        last_event: 2.0,
        last_check: 2.0,
        pid: 100,
        pid_start: "lstart".into(),
        in_plan_mode: false,
        plan_progress: None,
        last_tool_name: Some("Bash".into()),
        last_tool_target: Some("ls".into()),
        subtitle: Some("Running ls".into()),
        cwd: Some("/cwd".into()),
        transcript_path: None,
        owner_product: None,
        owner_thread_id: None,
        recent_events: Default::default(),
    }
}

#[test]
fn concurrent_readers_never_see_partial_state_file() {
    let tmp = tempfile::tempdir().unwrap();
    let state_path = tmp.path().join("state.json");

    // Seed with an initial valid state.
    let mut threads = std::collections::HashMap::new();
    threads.insert("claude:seed".to_string(), dummy_thread("seed"));
    write(&state_path, &build_state_file(threads.clone())).unwrap();

    let stop = Arc::new(AtomicBool::new(false));

    let mut readers = Vec::new();
    for _ in 0..10 {
        let path = state_path.clone();
        let stop = stop.clone();
        readers.push(thread::spawn(move || {
            let mut count = 0u32;
            while !stop.load(Ordering::Relaxed) {
                if let Ok(raw) = std::fs::read_to_string(&path) {
                    let v = serde_json::from_str::<serde_json::Value>(&raw);
                    assert!(
                        v.is_ok(),
                        "reader saw invalid JSON after {count} reads: {raw:?}"
                    );
                }
                count += 1;
                // No sleep — hammer it as hard as possible.
            }
        }));
    }

    let writer_path = state_path.clone();
    let writer = thread::spawn(move || {
        let mut threads = std::collections::HashMap::new();
        for i in 0..200 {
            threads.clear();
            for j in 0..(i % 5 + 1) {
                threads.insert(format!("claude:t-{j}"), dummy_thread(&format!("t-{j}")));
            }
            write(&writer_path, &build_state_file(threads.clone())).unwrap();
            // Don't sleep — match real bursty traffic.
        }
    });

    writer.join().unwrap();
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }
}

#[test]
fn rapid_overwrite_leaves_no_temp_files() {
    let tmp = tempfile::tempdir().unwrap();
    let state_path = tmp.path().join("state.json");

    let mut threads = std::collections::HashMap::new();
    threads.insert("claude:a".to_string(), dummy_thread("a"));

    for _ in 0..50 {
        write(&state_path, &build_state_file(threads.clone())).unwrap();
    }

    let entries: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected only state.json; got {entries:?}"
    );
    assert_eq!(entries[0], "state.json");
}
