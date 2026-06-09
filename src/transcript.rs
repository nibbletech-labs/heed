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
