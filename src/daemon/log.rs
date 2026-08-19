//! Event-log retention. Port of Codezilla
//! `src-tauri/src/claude_hooks/mod.rs::maybe_truncate_event_log` (lines 163–204).
//!
//! Trigger at 1 MiB; trim to the last 6000 lines AND at most 512 KiB, atomic
//! tmp+fsync+rename. Called on daemon start (before the watcher seeks to EOF)
//! and periodically from the watcher thread — race window between read and
//! rename drops any events emitted by hooks in that ~ms gap, which is
//! acceptable for an activity log.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const EVENT_LOG_MAX_BYTES: u64 = 1_048_576;
pub const EVENT_LOG_KEEP_LINES: usize = 6000;
/// Post-trim byte budget. Heed event lines average ~440 bytes, so 6000 lines
/// alone is ~2.6 MiB — a line-only cap never gets the file back under the
/// 1 MiB trigger and truncation would fire on every check. Trimming to half
/// the trigger gives hysteresis.
pub const EVENT_LOG_KEEP_BYTES: usize = 524_288;

pub fn maybe_truncate_event_log(log_path: &Path) -> Result<bool, String> {
    let size = match fs::metadata(log_path) {
        Ok(m) => m.len(),
        Err(_) => return Ok(false),
    };
    if size <= EVENT_LOG_MAX_BYTES {
        return Ok(false);
    }
    // Decode lossily: a single invalid-UTF-8 byte (a partial/garbled hook
    // write) must not make truncation fail — that left the oversized, corrupt
    // log in place and the watcher then wedged on it. Lossy decode replaces bad
    // bytes with U+FFFD, and rewriting the kept lines scrubs them from disk.
    let bytes = fs::read(log_path).map_err(|e| format!("read {:?}: {}", log_path, e))?;
    let raw = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = raw.lines().collect();
    let mut kept: &[&str] = if lines.len() > EVENT_LOG_KEEP_LINES {
        &lines[lines.len() - EVENT_LOG_KEEP_LINES..]
    } else {
        &lines[..]
    };
    // Enforce the byte budget on top of the line cap: drop oldest kept lines
    // until the suffix fits in EVENT_LOG_KEEP_BYTES.
    let mut total: usize = kept.iter().map(|l| l.len() + 1).sum();
    while total > EVENT_LOG_KEEP_BYTES && kept.len() > 1 {
        total -= kept[0].len() + 1;
        kept = &kept[1..];
    }
    let mut new_contents = kept.join("\n");
    if !new_contents.is_empty() {
        new_contents.push('\n');
    }
    let tmp_path: PathBuf = {
        let mut p = log_path.as_os_str().to_owned();
        p.push(".heed.tmp");
        PathBuf::from(p)
    };
    {
        let mut tmp =
            fs::File::create(&tmp_path).map_err(|e| format!("create tmp {:?}: {}", tmp_path, e))?;
        tmp.write_all(new_contents.as_bytes())
            .map_err(|e| format!("write tmp: {}", e))?;
        tmp.sync_all().map_err(|e| format!("fsync tmp: {}", e))?;
    }
    fs::rename(&tmp_path, log_path).map_err(|e| format!("rename: {}", e))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn smaller_than_cap_is_noop() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("events.jsonl");
        fs::write(&p, "one\ntwo\nthree\n").unwrap();
        let trimmed = maybe_truncate_event_log(&p).unwrap();
        assert!(!trimmed);
        assert_eq!(fs::read_to_string(&p).unwrap(), "one\ntwo\nthree\n");
    }

    #[test]
    fn over_cap_keeps_last_n_lines() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("events.jsonl");

        // Build a file > 1 MiB. Each line is ~200 bytes, so 10000 lines is ~2 MiB.
        let line = "x".repeat(200);
        let mut buf = String::new();
        for i in 0..10_000 {
            buf.push_str(&format!("{i:05} {line}\n"));
        }
        fs::write(&p, &buf).unwrap();
        assert!(fs::metadata(&p).unwrap().len() > EVENT_LOG_MAX_BYTES);

        let trimmed = maybe_truncate_event_log(&p).unwrap();
        assert!(trimmed);
        let after = fs::read_to_string(&p).unwrap();
        let after_lines: Vec<&str> = after.lines().collect();
        assert!(after_lines.len() <= EVENT_LOG_KEEP_LINES);
        assert!(after.len() <= EVENT_LOG_KEEP_BYTES);
        // The newest suffix is what survives.
        assert!(after_lines.last().unwrap().starts_with("09999 "));
        // Each kept line is ~207 bytes, so the byte budget keeps ~2500 lines.
        assert!(
            after_lines.len() > 2_000,
            "kept {} lines",
            after_lines.len()
        );
    }

    #[test]
    fn trimmed_log_lands_under_the_trigger_threshold() {
        // Regression: with long lines, a line-count-only trim left the file
        // above EVENT_LOG_MAX_BYTES forever, so the "cap" never held.
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("events.jsonl");
        let line = "x".repeat(440); // realistic heed event line length
        let mut buf = String::new();
        for _ in 0..7_000 {
            buf.push_str(&line);
            buf.push('\n');
        }
        fs::write(&p, &buf).unwrap();

        assert!(maybe_truncate_event_log(&p).unwrap());
        let size = fs::metadata(&p).unwrap().len();
        assert!(size <= EVENT_LOG_KEEP_BYTES as u64, "still {size} bytes");
        // And therefore a second pass is a no-op.
        assert!(!maybe_truncate_event_log(&p).unwrap());
    }

    #[test]
    fn truncation_tolerates_invalid_utf8() {
        // An oversized log containing invalid UTF-8 must still truncate (rather
        // than erroring and leaving the corrupt file in place). Regression for
        // the daemon stall: `read_to_string` rejected the whole file.
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("events.jsonl");

        let valid_line = format!("{}\n", "x".repeat(200));
        let mut buf: Vec<u8> = Vec::new();
        for i in 0..10_000 {
            buf.extend_from_slice(format!("{i:05} ").as_bytes());
            buf.extend_from_slice(valid_line.as_bytes());
            // Sprinkle in a raw invalid-UTF-8 byte every so often.
            if i % 500 == 0 {
                buf.extend_from_slice(&[0xff, 0xfe, b'\n']);
            }
        }
        fs::write(&p, &buf).unwrap();
        assert!(fs::metadata(&p).unwrap().len() > EVENT_LOG_MAX_BYTES);

        let trimmed = maybe_truncate_event_log(&p).unwrap();
        assert!(
            trimmed,
            "oversized corrupt log should be truncated, not errored"
        );
        // The rewritten file is valid UTF-8 and within the line cap.
        let after = fs::read_to_string(&p).expect("rewritten log is valid UTF-8");
        assert!(after.lines().count() <= EVENT_LOG_KEEP_LINES);
    }

    #[test]
    fn missing_file_is_noop() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("nonexistent.jsonl");
        assert!(!maybe_truncate_event_log(&p).unwrap());
    }
}
