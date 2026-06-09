//! Event-log retention. Port of Codezilla
//! `src-tauri/src/claude_hooks/mod.rs::maybe_truncate_event_log` (lines 163–204).
//!
//! Cap at 1 MiB / 6000 lines, atomic tmp+fsync+rename. Called once on daemon
//! start before the watcher seeks to EOF — race window between read and rename
//! drops any events emitted by hooks in that ~ms gap, which is acceptable for
//! an activity log.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const EVENT_LOG_MAX_BYTES: u64 = 1_048_576;
pub const EVENT_LOG_KEEP_LINES: usize = 6000;

pub fn maybe_truncate_event_log(log_path: &Path) -> Result<bool, String> {
    let size = match fs::metadata(log_path) {
        Ok(m) => m.len(),
        Err(_) => return Ok(false),
    };
    if size <= EVENT_LOG_MAX_BYTES {
        return Ok(false);
    }
    let raw = fs::read_to_string(log_path).map_err(|e| format!("read {:?}: {}", log_path, e))?;
    let lines: Vec<&str> = raw.lines().collect();
    let kept: &[&str] = if lines.len() > EVENT_LOG_KEEP_LINES {
        &lines[lines.len() - EVENT_LOG_KEEP_LINES..]
    } else {
        &lines[..]
    };
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
        assert_eq!(after_lines.len(), EVENT_LOG_KEEP_LINES);
        // First retained line should be 10000 - 6000 = 4000.
        assert!(after_lines[0].starts_with("04000 "));
        // Last is 9999.
        assert!(after_lines.last().unwrap().starts_with("09999 "));
    }

    #[test]
    fn missing_file_is_noop() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("nonexistent.jsonl");
        assert!(!maybe_truncate_event_log(&p).unwrap());
    }
}
