//! Deterministic Codex transcript binder.
//!
//! Codex hook stdin frequently lacks a `transcript_path`. To populate the
//! field after the fact, we scan `~/.codex/sessions/**/rollout-*.jsonl`,
//! parse each rollout's `session_meta` head record for `(id, cwd)`, and
//! match registrations using:
//!   1. Exact `session_id` match (~2M score, near-unbeatable).
//!   2. Otherwise normalized `cwd` match plus a 30s early-skew filter and
//!      a 900s proximity-to-`started_at` window.
//!
//! A claims map prevents one rollout being bound to two threads.
//!
//! Ported from Codezilla `src-tauri/src/transcript/mod.rs` lines 595–763.

use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const CODEX_BIND_MAX_DEPTH: u8 = 4;
pub const CODEX_BIND_CANDIDATE_LIMIT: usize = 200;
pub const CODEX_BIND_EARLY_SKEW_MS: u64 = 30_000;
pub const CODEX_BIND_META_SCAN_LINES: usize = 64;

#[derive(Clone, Debug)]
pub struct Registration {
    pub thread_id: String,
    pub cwd: String,
    pub started_at_ms: u64,
    pub expected_codex_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub path: String,
    pub cwd: String,
    pub session_id: String,
    pub modified_ms: u64,
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn normalize_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn file_modified_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `$CODEX_HOME/sessions` if set, else `~/.codex/sessions`.
pub fn codex_sessions_root(home: &Path) -> PathBuf {
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        return PathBuf::from(codex_home).join("sessions");
    }
    home.join(".codex").join("sessions")
}

/// Recursively collect rollout files under `dir` up to `CODEX_BIND_MAX_DEPTH`.
pub fn collect_rollout_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_inner(dir, 0, &mut out);
    out
}

fn collect_inner(dir: &Path, depth: u8, out: &mut Vec<PathBuf>) {
    if depth > CODEX_BIND_MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_inner(&path, depth + 1, out);
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with("rollout-") && name.ends_with(".jsonl") {
            out.push(path);
        }
    }
}

/// Scan the first `CODEX_BIND_META_SCAN_LINES` lines of `path` for a
/// `session_meta` record. Returns `(session_id, cwd)`.
pub fn parse_session_meta(path: &Path) -> Option<(String, String)> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(CODEX_BIND_META_SCAN_LINES) {
        let Ok(l) = line else { continue };
        if l.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&l) else {
            continue;
        };
        if value.get("type").and_then(|v| v.as_str()) != Some("session_meta") {
            continue;
        }
        let payload = value.get("payload")?;
        let id = payload.get("id").and_then(|v| v.as_str())?;
        let cwd = payload.get("cwd").and_then(|v| v.as_str())?;
        return Some((id.to_string(), cwd.to_string()));
    }
    None
}

/// Load all rollout candidates, sorted by mtime descending, capped at `limit`.
pub fn load_candidates(home: &Path, limit: usize) -> Vec<Candidate> {
    let root = codex_sessions_root(home);
    if !root.exists() {
        return Vec::new();
    }
    let mut files = collect_rollout_files(&root);
    files.sort_by_key(|path| std::cmp::Reverse(file_modified_ms(path)));
    files.truncate(limit);

    let mut out = Vec::new();
    for path in files {
        let Some((session_id, cwd)) = parse_session_meta(&path) else {
            continue;
        };
        out.push(Candidate {
            path: path.to_string_lossy().to_string(),
            cwd: normalize_path(&cwd),
            session_id,
            modified_ms: file_modified_ms(&path),
        });
    }
    out
}

/// Score a candidate against a registration. `None` means the candidate is
/// disqualified (cwd mismatch, too-early skew). Higher = better.
pub fn candidate_score(reg: &Registration, c: &Candidate) -> Option<i64> {
    if let Some(expected) = &reg.expected_codex_id {
        if expected == &c.session_id {
            let diff = c.modified_ms.abs_diff(reg.started_at_ms).min(1_000_000) as i64;
            return Some(2_000_000 - diff);
        }
    }

    if normalize_path(&reg.cwd) != normalize_path(&c.cwd) {
        return None;
    }
    if c.modified_ms + CODEX_BIND_EARLY_SKEW_MS < reg.started_at_ms {
        return None;
    }
    let diff = c.modified_ms.abs_diff(reg.started_at_ms).min(900_000) as i64;
    Some(1_000_000 - diff)
}

/// Pick the best unclaimed candidate. Tiebreaks: higher score → more recent
/// mtime → lexicographically greater path.
pub fn pick_candidate(
    reg: &Registration,
    candidates: &[Candidate],
    claims: &HashMap<String, String>,
) -> Option<Candidate> {
    let mut best: Option<(i64, Candidate)> = None;
    for c in candidates {
        let claimed_by_other = claims
            .get(&c.path)
            .map(|tid| tid != &reg.thread_id)
            .unwrap_or(false);
        if claimed_by_other {
            continue;
        }
        let Some(score) = candidate_score(reg, c) else {
            continue;
        };
        match &best {
            Some((best_score, best_c)) => {
                let better = score > *best_score
                    || (score == *best_score
                        && (c.modified_ms > best_c.modified_ms
                            || (c.modified_ms == best_c.modified_ms && c.path > best_c.path)));
                if better {
                    best = Some((score, c.clone()));
                }
            }
            None => best = Some((score, c.clone())),
        }
    }
    best.map(|(_, c)| c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_rollout(dir: &Path, name: &str, session_id: &str, cwd: &str) -> PathBuf {
        let path = dir.join(name);
        let line = format!(
            r#"{{"type":"session_meta","payload":{{"id":"{session_id}","cwd":"{cwd}","originator":"codex-tui","cli_version":"0.130.0"}}}}"#
        );
        std::fs::write(&path, format!("{line}\n")).unwrap();
        path
    }

    fn reg(
        thread_id: &str,
        cwd: &str,
        started_at_ms: u64,
        expected_codex_id: Option<&str>,
    ) -> Registration {
        Registration {
            thread_id: thread_id.into(),
            cwd: cwd.into(),
            started_at_ms,
            expected_codex_id: expected_codex_id.map(str::to_string),
        }
    }

    fn cand(path: &str, cwd: &str, sid: &str, modified_ms: u64) -> Candidate {
        Candidate {
            path: path.into(),
            cwd: normalize_path(cwd),
            session_id: sid.into(),
            modified_ms,
        }
    }

    #[test]
    fn parse_session_meta_extracts_id_and_cwd() {
        let tmp = tempdir().unwrap();
        let path = write_rollout(tmp.path(), "rollout-1.jsonl", "abc-123", "/work");
        let (id, cwd) = parse_session_meta(&path).unwrap();
        assert_eq!(id, "abc-123");
        assert_eq!(cwd, "/work");
    }

    #[test]
    fn parse_session_meta_returns_none_when_missing() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("rollout-empty.jsonl");
        std::fs::write(&path, "").unwrap();
        assert!(parse_session_meta(&path).is_none());
    }

    #[test]
    fn exact_id_match_wins_over_cwd_match() {
        let r = reg("t1", "/work", 1000, Some("targeted-id"));
        let same_cwd_diff_id = cand("/a/rollout-1.jsonl", "/work", "other-id", 900);
        let id_match_other_cwd = cand("/a/rollout-2.jsonl", "/elsewhere", "targeted-id", 950);
        let picked = pick_candidate(
            &r,
            &[same_cwd_diff_id, id_match_other_cwd.clone()],
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(picked.path, id_match_other_cwd.path);
    }

    #[test]
    fn cwd_must_match_for_non_id_path() {
        let r = reg("t1", "/work", 1000, None);
        let c = cand("/a/rollout-1.jsonl", "/elsewhere", "x", 1000);
        assert!(pick_candidate(&r, &[c], &HashMap::new()).is_none());
    }

    #[test]
    fn early_skew_filter_excludes_too_early_candidates() {
        let r = reg("t1", "/work", 1_000_000, None);
        // candidate modified 31s before start → outside the 30s skew → rejected
        let too_early = cand("/a/rollout-1.jsonl", "/work", "x", 1_000_000 - 31_000);
        assert!(pick_candidate(&r, &[too_early], &HashMap::new()).is_none());
        // within skew (29s before) → accepted
        let within = cand("/a/rollout-2.jsonl", "/work", "x", 1_000_000 - 29_000);
        assert!(pick_candidate(&r, &[within], &HashMap::new()).is_some());
    }

    #[test]
    fn claimed_candidates_skipped_for_other_threads() {
        let r1 = reg("t1", "/work", 1000, None);
        let r2 = reg("t2", "/work", 1000, None);
        let c = cand("/a/rollout-1.jsonl", "/work", "x", 1000);
        let mut claims = HashMap::new();
        claims.insert(c.path.clone(), "t1".to_string());
        // r1's own claim is fine.
        assert!(pick_candidate(&r1, std::slice::from_ref(&c), &claims).is_some());
        // r2 can't pick c — it's claimed by t1.
        assert!(pick_candidate(&r2, &[c], &claims).is_none());
    }

    #[test]
    fn closer_mtime_wins_within_window() {
        let r = reg("t1", "/work", 1_000_000, None);
        let far = cand("/a/rollout-1.jsonl", "/work", "x", 1_000_000 - 20_000);
        let close = cand("/a/rollout-2.jsonl", "/work", "y", 1_000_000 - 1_000);
        let picked = pick_candidate(&r, &[far, close.clone()], &HashMap::new()).unwrap();
        assert_eq!(picked.path, close.path);
    }

    #[test]
    fn load_candidates_walks_nested_dirs() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        let nested = home.join(".codex/sessions/2026/05/12");
        std::fs::create_dir_all(&nested).unwrap();
        write_rollout(&nested, "rollout-a.jsonl", "id-a", "/work");
        write_rollout(&nested, "rollout-b.jsonl", "id-b", "/work");
        // CODEX_HOME override should not leak from earlier tests.
        std::env::remove_var("CODEX_HOME");
        let cands = load_candidates(home, 10);
        let ids: Vec<&str> = cands.iter().map(|c| c.session_id.as_str()).collect();
        assert!(ids.contains(&"id-a"));
        assert!(ids.contains(&"id-b"));
    }
}
