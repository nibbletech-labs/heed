//! Persisted watcher offset (`~/.heed/watcher.state`).
//!
//! Lets `heed daemon` resume from where it left off after a restart instead
//! of jumping to EOF and missing events emitted while it was down.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::install::{atomic_write, heed_dir};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WatcherState {
    pub events_offset: u64,
}

pub fn path(home: &Path) -> PathBuf {
    heed_dir(home).join("watcher.state")
}

pub fn load(home: &Path) -> WatcherState {
    let p = path(home);
    let Ok(raw) = std::fs::read_to_string(&p) else {
        return WatcherState::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save(home: &Path, state: &WatcherState) -> Result<(), String> {
    let p = path(home);
    let mut bytes = serde_json::to_vec(state).map_err(|e| format!("serialize: {e}"))?;
    bytes.push(b'\n');
    atomic_write(&p, &bytes)
}

/// Reconcile the saved offset against the actual log size. If the log has
/// shrunk below the saved offset (rotation/truncation), reset to 0. Returns
/// the effective offset to start the watcher from.
pub fn effective_offset(saved: u64, current_size: u64) -> u64 {
    if saved > current_size {
        0
    } else {
        saved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn default_when_missing() {
        let tmp = tempdir().unwrap();
        let s = load(tmp.path());
        assert_eq!(s.events_offset, 0);
    }

    #[test]
    fn roundtrip_offset() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".heed")).unwrap();
        save(
            tmp.path(),
            &WatcherState {
                events_offset: 12345,
            },
        )
        .unwrap();
        assert_eq!(load(tmp.path()).events_offset, 12345);
    }

    #[test]
    fn malformed_falls_back_to_default() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".heed")).unwrap();
        std::fs::write(path(tmp.path()), "not json").unwrap();
        assert_eq!(load(tmp.path()).events_offset, 0);
    }

    #[test]
    fn effective_offset_resets_on_shrink() {
        assert_eq!(effective_offset(1000, 500), 0);
        assert_eq!(effective_offset(0, 500), 0);
        assert_eq!(effective_offset(500, 1000), 500);
        assert_eq!(effective_offset(500, 500), 500);
    }
}
