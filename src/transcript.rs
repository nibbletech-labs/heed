//! Transcript reader for the post-Stop scan.
//!
//! Claude transcripts are JSONL files at `transcript_path`. Each line is an
//! entry like `{"type":"assistant", "message": {"content": [{"type":"text", "text":"..."}]}}`.
//! We read the last ~200 lines, walk backward for the last `type=="assistant"`
//! entry, and apply the `ends_like_question` rule to its concatenated text.
//!
//! Adapted from Codezilla `Terminal.tsx::scanForQuestionPattern` (which scans
//! the live terminal buffer). Heed moves to transcript scanning per SPEC §4.2
//! — the structured transcript skips chrome-handling complexity.

use serde_json::Value;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

use crate::state::PostStopResult;

const LOOKBACK_LINES: usize = 200;
const RETRY_DELAY_MS: u64 = 200;

/// SPEC §4.2 / Codezilla `endsLikeQuestion`: scan the last "meaningful" chars
/// (`[A-Za-z0-9?.!]`) and look at the last one.
pub fn ends_like_question(text: &str) -> PostStopResult {
    let mut last_meaningful: Option<char> = None;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '?' || c == '.' || c == '!' {
            last_meaningful = Some(c);
        }
    }
    match last_meaningful {
        Some('?') => PostStopResult::Question,
        Some('.') | Some('!') => PostStopResult::Statement,
        _ => PostStopResult::Neither,
    }
}

/// Read the final assistant `text` content from a Claude transcript JSONL.
/// Returns `None` if no assistant entry with text is found in the lookback.
pub fn last_assistant_text(transcript_path: &Path) -> Option<String> {
    let raw = fs::read_to_string(transcript_path).ok()?;
    // Take the last `LOOKBACK_LINES` for bounded work.
    let mut lines: Vec<&str> = raw.lines().collect();
    if lines.len() > LOOKBACK_LINES {
        lines = lines[lines.len() - LOOKBACK_LINES..].to_vec();
    }
    for line in lines.iter().rev() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let texts: Vec<String> = entry
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| {
                        if c.get("type").and_then(|v| v.as_str()) == Some("text") {
                            c.get("text").and_then(|t| t.as_str()).map(String::from)
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        if texts.is_empty() {
            continue;
        }
        return Some(texts.join("\n"));
    }
    None
}

/// Incremental reviewer metadata, owned by the background scan worker.
#[derive(Default)]
pub struct ReviewerCache {
    entries: std::collections::HashMap<std::path::PathBuf, ReviewerEntry>,
}

#[derive(Default)]
struct ReviewerEntry {
    identity: (u64, u64),
    offset: u64,
    observed_len: u64,
    boundary: Vec<u8>,
    auto_review: bool,
}

impl ReviewerCache {
    pub fn uses_auto_review(&mut self, path: &Path) -> bool {
        self.read(path).unwrap_or_else(|_| {
            self.entries.remove(path);
            false
        })
    }

    fn read(&mut self, path: &Path) -> std::io::Result<bool> {
        use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
        use std::os::unix::fs::MetadataExt;
        let mut file = fs::File::open(path)?;
        let metadata = file.metadata()?;
        let identity = (metadata.dev(), metadata.ino());
        let entry = self.entries.entry(path.to_path_buf()).or_default();
        // Detect an in-place rewrite that has already grown past our cursor,
        // as well as ordinary truncation and atomic replacement.
        let mut boundary_changed = false;
        if entry.identity == identity
            && metadata.len() >= entry.offset
            && !entry.boundary.is_empty()
        {
            file.seek(SeekFrom::Start(entry.offset - entry.boundary.len() as u64))?;
            let mut boundary = vec![0; entry.boundary.len()];
            file.read_exact(&mut boundary)?;
            boundary_changed = boundary != entry.boundary;
        }
        if entry.identity != identity || metadata.len() < entry.observed_len || boundary_changed {
            *entry = ReviewerEntry {
                identity,
                ..Default::default()
            };
        }
        file.seek(SeekFrom::Start(entry.offset))?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            let count = reader.read_until(b'\n', &mut line)?;
            if count == 0 {
                break;
            }
            // Most records are tools/output. Avoid allocating a JSON tree for
            // them, especially multi-megabyte embedded images.
            if line
                .windows(b"turn_context".len())
                .any(|w| w == b"turn_context")
            {
                if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                    if value.get("type").and_then(Value::as_str) == Some("turn_context") {
                        entry.auto_review = value
                            .pointer("/payload/approvals_reviewer")
                            .and_then(Value::as_str)
                            == Some("auto_review");
                    }
                }
            }
            // Retry the final incomplete record after the writer appends.
            if line.last() != Some(&b'\n') {
                break;
            }
            entry.offset += count as u64;
        }
        let mut file = reader.into_inner();
        let size = entry.offset.min(256) as usize;
        entry.boundary.resize(size, 0);
        file.seek(SeekFrom::Start(entry.offset - size as u64))?;
        file.read_exact(&mut entry.boundary)?;
        entry.observed_len = metadata.len();
        Ok(entry.auto_review)
    }
}

/// Run the post-Stop scan for a Claude transcript. Retries once after
/// `RETRY_DELAY_MS` if the transcript hasn't been flushed yet (SPEC §4.2 edge).
pub fn scan_post_stop(transcript_path: &Path) -> PostStopResult {
    if let Some(text) = last_assistant_text(transcript_path) {
        return ends_like_question(&text);
    }
    thread::sleep(Duration::from_millis(RETRY_DELAY_MS));
    if let Some(text) = last_assistant_text(transcript_path) {
        return ends_like_question(&text);
    }
    PostStopResult::Neither
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_transcript(lines: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f.flush().unwrap();
        f
    }

    #[test]
    fn auto_review_uses_latest_context_across_read_boundaries() {
        let mut file = NamedTempFile::new().unwrap();
        let mut cache = ReviewerCache::default();
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"turn_context",
            "payload":{"approvals_reviewer":"auto_review"}})
        )
        .unwrap();
        // One oversized record and enough ordinary records to cross chunks.
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"other", "text":"x".repeat(100_000)})
        )
        .unwrap();
        for _ in 0..5000 {
            writeln!(file, "{{\"type\":\"other\"}}").unwrap();
        }
        assert!(cache.uses_auto_review(file.path()));
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"turn_context",
            "payload":{"approvals_reviewer":"user"}})
        )
        .unwrap();
        assert!(!cache.uses_auto_review(file.path()));
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"turn_context", "payload":{}})
        )
        .unwrap();
        assert!(!cache.uses_auto_review(file.path()));
    }

    #[test]
    fn reviewer_cache_reads_appends_and_retries_partial_records() {
        let mut file = NamedTempFile::new().unwrap();
        let mut cache = ReviewerCache::default();
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"turn_context",
            "payload":{"approvals_reviewer":"auto_review"}})
        )
        .unwrap();
        assert!(cache.uses_auto_review(file.path()));
        let offset = cache.entries[file.path()].offset;
        assert!(cache.uses_auto_review(file.path()));
        assert_eq!(cache.entries[file.path()].offset, offset);
        write!(file, "{{\"type\":\"turn_context\",\"payload\":").unwrap();
        assert!(cache.uses_auto_review(file.path()));
        assert_eq!(cache.entries[file.path()].offset, offset);
        writeln!(file, "{{\"approvals_reviewer\":\"user\"}}}}").unwrap();
        assert!(!cache.uses_auto_review(file.path()));
        assert_eq!(
            cache.entries[file.path()].offset,
            file.as_file().metadata().unwrap().len()
        );
    }

    #[test]
    fn reviewer_cache_resets_on_truncation_replacement_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let auto = serde_json::json!({"type":"turn_context",
            "payload":{"approvals_reviewer":"auto_review"}})
        .to_string()
            + "\n";
        let mut cache = ReviewerCache::default();
        fs::write(&path, &auto).unwrap();
        assert!(cache.uses_auto_review(&path));
        fs::write(&path, "{}\n").unwrap();
        assert!(!cache.uses_auto_review(&path));
        fs::write(&path, &auto).unwrap();
        assert!(cache.uses_auto_review(&path));
        let replacement = dir.path().join("replacement");
        fs::write(&replacement, "{}\n".repeat(100)).unwrap();
        fs::rename(replacement, &path).unwrap();
        assert!(!cache.uses_auto_review(&path));
        fs::remove_file(&path).unwrap();
        assert!(!cache.uses_auto_review(&path));
        assert!(!cache.entries.contains_key(&path));
    }

    #[test]
    fn auto_review_unknown_transcript_is_not_assumed() {
        let mut cache = ReviewerCache::default();
        let file = NamedTempFile::new().unwrap();
        assert!(!cache.uses_auto_review(file.path()));
        assert!(!cache.uses_auto_review(Path::new("/nonexistent/heed-transcript")));
    }

    #[test]
    fn ends_like_question_basic() {
        assert_eq!(
            ends_like_question("Sure, what do you want?"),
            PostStopResult::Question
        );
        assert_eq!(ends_like_question("Done."), PostStopResult::Statement);
        assert_eq!(ends_like_question("Boom!"), PostStopResult::Statement);
        assert_eq!(ends_like_question("..."), PostStopResult::Statement);
    }

    #[test]
    fn ends_like_question_ignores_trailing_decoration() {
        // emojis and whitespace after the question mark should not change the verdict.
        assert_eq!(
            ends_like_question("Want to do X? 🤔"),
            PostStopResult::Question
        );
        assert_eq!(ends_like_question("Done. 🎉"), PostStopResult::Statement);
    }

    #[test]
    fn ends_like_question_bare_question_mark_in_middle_does_not_block() {
        // Codezilla's spec call-out: `Run grep for "foo?"` should not be a question.
        assert_eq!(
            ends_like_question(r#"Run grep for "foo?" and report back."#),
            PostStopResult::Statement
        );
    }

    #[test]
    fn ends_like_question_empty_returns_neither() {
        assert_eq!(ends_like_question(""), PostStopResult::Neither);
        assert_eq!(ends_like_question("   "), PostStopResult::Neither);
        assert_eq!(ends_like_question("───"), PostStopResult::Neither);
    }

    #[test]
    fn last_assistant_text_picks_final_assistant_entry() {
        let lines = vec![
            r#"{"type":"user","message":{"content":[{"type":"text","text":"hi"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"first reply"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"again"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"final reply"}]}}"#,
        ];
        let f = write_transcript(&lines);
        let text = last_assistant_text(f.path()).unwrap();
        assert_eq!(text, "final reply");
    }

    #[test]
    fn last_assistant_text_concatenates_text_blocks() {
        // Real Claude transcripts are one entry per line — multi-line JSON
        // wouldn't parse here. Pack everything onto one line.
        let lines = vec![
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"part one"},{"type":"tool_use","name":"Bash"},{"type":"text","text":"part two"}]}}"#,
        ];
        let f = write_transcript(&lines);
        let text = last_assistant_text(f.path()).unwrap();
        assert_eq!(text, "part one\npart two");
    }

    #[test]
    fn last_assistant_text_returns_none_for_no_assistant() {
        let lines = vec![r#"{"type":"user","message":{"content":[{"type":"text","text":"hi"}]}}"#];
        let f = write_transcript(&lines);
        assert!(last_assistant_text(f.path()).is_none());
    }

    #[test]
    fn last_assistant_text_skips_assistant_entries_without_text() {
        // An assistant entry that's purely tool_use should be skipped.
        let lines = vec![
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"only text reply"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
        ];
        let f = write_transcript(&lines);
        let text = last_assistant_text(f.path()).unwrap();
        assert_eq!(text, "only text reply");
    }

    #[test]
    fn scan_post_stop_question_then_statement() {
        let q = write_transcript(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"What next?"}]}}"#,
        ]);
        assert_eq!(scan_post_stop(q.path()), PostStopResult::Question);

        let s = write_transcript(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"All done."}]}}"#,
        ]);
        assert_eq!(scan_post_stop(s.path()), PostStopResult::Statement);
    }

    #[test]
    fn scan_post_stop_missing_file_returns_neither() {
        let path = std::path::PathBuf::from("/tmp/heed-test-nonexistent-transcript.jsonl");
        let _ = std::fs::remove_file(&path);
        // Two retries × RETRY_DELAY_MS = 400ms — acceptable for test runtime.
        assert_eq!(scan_post_stop(&path), PostStopResult::Neither);
    }
}
