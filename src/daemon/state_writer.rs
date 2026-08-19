//! Atomic write of `~/.heed/state.json`.
//!
//! SPEC §7: `{schema_version, updated_at, heed_version, hook_script_versions,
//! threads}` rendered with `serde_json::to_string_pretty`, written via
//! tmp+fsync+rename. Daemon debounces writes at 100ms upstream.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::install::atomic_write;
use crate::state::{Activity, Liveness, ThreadState};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateFile {
    pub schema_version: u32,
    pub updated_at: f64,
    pub heed_version: String,
    pub hook_script_versions: HookScriptVersions,
    /// Keyed by `<cli>:<thread_id>` for stable, language-agnostic lookups.
    pub threads: HashMap<String, ThreadState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HookScriptVersions {
    pub claude: String,
    pub codex: String,
}

pub fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn build_state_file(mut threads: HashMap<String, ThreadState>) -> StateFile {
    // Backstop the gone => idle invariant at the one chokepoint every write
    // passes through, so a future transition path that forgets to clear activity
    // can never persist a `working`/`gone` thread (which renders as a stuck
    // spinner in consumers like Codezilla).
    for state in threads.values_mut() {
        if state.liveness == Liveness::Gone && state.activity != Activity::Idle {
            state.activity = Activity::Idle;
        }
    }
    StateFile {
        schema_version: SCHEMA_VERSION,
        updated_at: now_unix(),
        heed_version: crate::HEED_VERSION.to_string(),
        hook_script_versions: HookScriptVersions {
            claude: crate::CLAUDE_HOOK_SCRIPTS_VERSION.to_string(),
            codex: crate::CODEX_HOOK_SCRIPTS_VERSION.to_string(),
        },
        threads,
    }
}

pub fn write(path: &Path, state: &StateFile) -> Result<(), String> {
    let mut bytes =
        serde_json::to_vec_pretty(state).map_err(|e| format!("serialize state.json: {e}"))?;
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    atomic_write(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Activity, Cli, Liveness, ThreadState};
    use std::collections::VecDeque;
    use tempfile::tempdir;

    fn dummy_thread(thread_id: &str, cli: Cli) -> ThreadState {
        ThreadState {
            thread_id: thread_id.into(),
            cli,
            activity: Activity::Working,
            liveness: Liveness::Live,
            first_seen: 1.0,
            last_event: 2.0,
            last_check: 2.0,
            pid: 100,
            pid_start: "Mon Jan 1 2026".into(),
            in_plan_mode: false,
            plan_progress: None,
            last_tool_name: Some("Bash".into()),
            last_tool_target: Some("ls".into()),
            subtitle: Some("Running ls".into()),
            cwd: Some("/cwd".into()),
            transcript_path: None,
            owner_product: None,
            owner_thread_id: None,
            supersedes: None,
            superseded_by: None,
            recent_events: VecDeque::new(),
        }
    }

    #[test]
    fn write_atomic_with_schema() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("state.json");
        let mut threads = HashMap::new();
        threads.insert("claude:abc".to_string(), dummy_thread("abc", Cli::Claude));
        write(&p, &build_state_file(threads)).unwrap();

        let raw = std::fs::read_to_string(&p).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["heed_version"], crate::HEED_VERSION);
        assert_eq!(
            parsed["hook_script_versions"]["claude"],
            crate::CLAUDE_HOOK_SCRIPTS_VERSION
        );
        assert!(parsed["threads"]["claude:abc"].is_object());
        assert!(raw.ends_with('\n'));
    }

    #[test]
    fn write_overwrites_atomically() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("state.json");
        write(&p, &build_state_file(HashMap::new())).unwrap();
        let first = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut threads = HashMap::new();
        threads.insert("claude:xyz".to_string(), dummy_thread("xyz", Cli::Claude));
        write(&p, &build_state_file(threads)).unwrap();
        let second = std::fs::metadata(&p).unwrap().modified().unwrap();
        assert!(second >= first);
        // No stale tmp file left behind.
        let tmp_path = p.with_extension("json.heed.tmp");
        assert!(!tmp_path.exists());
    }

    #[test]
    fn roundtrip_preserves_thread_fields() {
        let mut threads = HashMap::new();
        threads.insert("claude:abc".to_string(), dummy_thread("abc", Cli::Claude));
        let written = build_state_file(threads);
        let raw = serde_json::to_string(&written).unwrap();
        let parsed: StateFile = serde_json::from_str(&raw).unwrap();
        let thread = parsed.threads.get("claude:abc").unwrap();
        assert_eq!(thread.thread_id, "abc");
        assert_eq!(thread.activity, Activity::Working);
        assert_eq!(thread.pid, 100);
    }
}
