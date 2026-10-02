//! `~/.heed/owners.json` overlay: optional metadata identifying which product
//! (Codezilla, Muxra) spawned a thread.
//!
//! Schema per SPEC §4.1:
//! ```json
//! {
//!   "claude:ce4f...": {
//!     "owner_product": "codezilla",
//!     "owner_thread_id": "abc-123",
//!     "cwd": "/Users/tom/Local_Projects/builder"
//!   }
//! }
//! ```

use ::log::{error, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::install::atomic_write;
use crate::state::{Cli, ThreadKey};

/// The overlay, keyed by native session.
pub type Owners = HashMap<ThreadKey, OwnerRecord>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRecord {
    pub owner_product: Option<String>,
    pub owner_thread_id: Option<String>,
    pub cwd: Option<String>,
    /// Unix seconds the record's session was last known to be in use: set on
    /// register/transfer and refreshed by [`age`]. Absent on records written
    /// before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_at: Option<u64>,
}

/// A record whose session hasn't been seen for this long is dropped. Products
/// re-register on every spawn, so this only clears sessions nobody will
/// resume — without it the overlay grows by one entry per thread, forever.
pub const OWNER_RETAIN_SECS: u64 = 14 * 86_400;
/// `seen_at` is refreshed at most this often, so a live session doesn't
/// rewrite owners.json on every aging pass.
const SEEN_REFRESH_SECS: u64 = 86_400;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compose the JSON key `"<cli>:<thread_id>"` used in owners.json.
pub fn owner_key(cli: Cli, thread_id: &str) -> String {
    format!("{cli}:{thread_id}")
}

pub fn parse_thread_key(key: &str) -> Option<ThreadKey> {
    let (cli_str, thread) = key.split_once(':')?;
    let cli = match cli_str {
        "claude" => Cli::Claude,
        "codex" => Cli::Codex,
        _ => return None,
    };
    Some((cli, thread.to_string()))
}

/// Read and parse owners.json. Returns an empty map on missing/empty file.
/// Returns an error only on truly malformed JSON.
pub fn load(path: &Path) -> Result<HashMap<ThreadKey, OwnerRecord>, String> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let raw = fs::read_to_string(path).map_err(|e| format!("read {:?}: {}", path, e))?;
    if raw.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let value: Value =
        serde_json::from_str(&raw).map_err(|e| format!("{:?} malformed: {}", path, e))?;
    let obj = value
        .as_object()
        .ok_or_else(|| format!("{:?} root is not a JSON object", path))?;
    let mut out = HashMap::new();
    for (key, rec) in obj.iter() {
        let Some(thread_key) = parse_thread_key(key) else {
            continue;
        };
        let record: OwnerRecord = match serde_json::from_value(rec.clone()) {
            Ok(r) => r,
            Err(_) => continue,
        };
        out.insert(thread_key, record);
    }
    Ok(out)
}

/// Load owners for the daemon overlay, self-healing past a corrupt file. On a
/// malformed `owners.json` the bad file is renamed aside
/// (`owners.json.corrupt-<unix_ms>`), an error is logged, and an empty overlay
/// is returned — so a single bad byte can't freeze ownership for every thread
/// or fail every reload. Already-applied owner tags on live threads survive
/// (the overlay only sets, never clears), and the next `heed owner register`
/// writes a fresh file. Missing/empty files are not corrupt and pass through.
pub fn load_or_quarantine(path: &Path) -> HashMap<ThreadKey, OwnerRecord> {
    match load(path) {
        Ok(map) => map,
        Err(e) => {
            error!("owners.json unreadable ({e}); quarantining and continuing with no overlay");
            if let Err(qe) = quarantine(path) {
                warn!("could not quarantine corrupt owners.json: {qe}");
            }
            HashMap::new()
        }
    }
}

/// Move a corrupt file aside to `<name>.corrupt-<unix_ms>`, preserving the bytes
/// for debugging while letting the daemon continue.
fn quarantine(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".corrupt-{ts}"));
    let dest = path.with_file_name(name);
    fs::rename(path, &dest)
}

/// Insert or update an owner record. Atomic write.
pub fn register(path: &Path, cli: Cli, thread_id: &str, record: OwnerRecord) -> Result<(), String> {
    let mut current = load(path).unwrap_or_default();
    let record = OwnerRecord {
        seen_at: Some(now_secs()),
        ..record
    };
    current.insert((cli, thread_id.to_string()), record);
    write(path, &current)
}

/// Move an owner record from one native session to another in a single atomic
/// write, so the overlay never momentarily names both (a consumer would flap
/// between them) or neither (the thread would vanish from its product).
pub fn transfer(
    path: &Path,
    cli: Cli,
    from_thread_id: &str,
    to_thread_id: &str,
    record: OwnerRecord,
) -> Result<(), String> {
    let mut current = load(path).unwrap_or_default();
    current.remove(&(cli, from_thread_id.to_string()));
    let record = OwnerRecord {
        seen_at: Some(now_secs()),
        ..record
    };
    current.insert((cli, to_thread_id.to_string()), record);
    write(path, &current)
}

/// Age the overlay in place. `last_active` gives a record's session's latest
/// activity (unix seconds) when the daemon tracks it — `now` for a live one —
/// and `None` when it doesn't. A record takes the later of that and its
/// `seen_at`; one with neither is stamped `now` (it predates `seen_at`), and
/// one last seen over [`OWNER_RETAIN_SECS`] ago is removed. Returns whether
/// anything changed.
pub fn age(
    map: &mut HashMap<ThreadKey, OwnerRecord>,
    last_active: impl Fn(&ThreadKey) -> Option<u64>,
    now: u64,
) -> bool {
    let mut changed = false;
    map.retain(|key, rec| {
        let seen = rec.seen_at.max(last_active(key));
        let Some(seen) = seen else {
            rec.seen_at = Some(now);
            changed = true;
            return true;
        };
        if now.saturating_sub(seen) > OWNER_RETAIN_SECS {
            changed = true;
            return false;
        }
        if seen >= rec.seen_at.unwrap_or(0) + SEEN_REFRESH_SECS {
            rec.seen_at = Some(seen);
            changed = true;
        }
        true
    });
    changed
}

/// [`age`] owners.json on disk, re-read fresh so a registration written since
/// the daemon's last reload isn't lost. Returns the aged map when it changed
/// (and was written) with how many records it dropped, `None` when nothing
/// needed doing.
pub fn age_file(
    path: &Path,
    last_active: impl Fn(&ThreadKey) -> Option<u64>,
    now: u64,
) -> Result<Option<(Owners, usize)>, String> {
    let mut current = load(path)?;
    let before = current.len();
    if !age(&mut current, last_active, now) {
        return Ok(None);
    }
    write(path, &current)?;
    let dropped = before - current.len();
    Ok(Some((current, dropped)))
}

/// Remove an owner entry. No-op if it doesn't exist. Atomic write.
pub fn unregister(path: &Path, cli: Cli, thread_id: &str) -> Result<bool, String> {
    let mut current = load(path).unwrap_or_default();
    let removed = current.remove(&(cli, thread_id.to_string())).is_some();
    if removed {
        write(path, &current)?;
    }
    Ok(removed)
}

fn write(path: &Path, map: &HashMap<ThreadKey, OwnerRecord>) -> Result<(), String> {
    let mut obj = serde_json::Map::new();
    let mut keys: Vec<_> = map.keys().collect();
    keys.sort();
    for k in keys {
        let kstr = owner_key(k.0, &k.1);
        obj.insert(
            kstr,
            serde_json::to_value(&map[k]).map_err(|e| format!("serialize: {e}"))?,
        );
    }
    let mut bytes =
        serde_json::to_vec_pretty(&Value::Object(obj)).map_err(|e| format!("serialize: {e}"))?;
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    atomic_write(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_missing_returns_empty() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("owners.json");
        assert!(load(&p).unwrap().is_empty());
    }

    #[test]
    fn load_empty_returns_empty() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("owners.json");
        fs::write(&p, "").unwrap();
        assert!(load(&p).unwrap().is_empty());
    }

    #[test]
    fn load_parses_well_formed_map() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("owners.json");
        let raw = r#"{
          "claude:ce4f-abc": {"owner_product":"codezilla","owner_thread_id":"abc-123","cwd":"/work"},
          "codex:9a1b-def": {"owner_product":"muxra","owner_thread_id":"xyz"}
        }"#;
        fs::write(&p, raw).unwrap();
        let m = load(&p).unwrap();
        assert_eq!(m.len(), 2);
        let claude_record = m.get(&(Cli::Claude, "ce4f-abc".into())).unwrap();
        assert_eq!(claude_record.owner_product.as_deref(), Some("codezilla"));
        assert_eq!(claude_record.cwd.as_deref(), Some("/work"));
        let codex_record = m.get(&(Cli::Codex, "9a1b-def".into())).unwrap();
        assert_eq!(codex_record.owner_product.as_deref(), Some("muxra"));
        assert_eq!(codex_record.cwd, None);
    }

    #[test]
    fn malformed_json_returns_error() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("owners.json");
        fs::write(&p, "{ not valid").unwrap();
        assert!(load(&p).is_err());
    }

    #[test]
    fn load_or_quarantine_moves_corrupt_aside_and_returns_empty() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("owners.json");
        fs::write(&p, "{ not valid json").unwrap();

        let m = load_or_quarantine(&p);
        assert!(m.is_empty());
        assert!(!p.exists(), "corrupt file should have been renamed aside");

        let quarantined: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("owners.json.corrupt-")
            })
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantine copy expected");
    }

    #[test]
    fn load_or_quarantine_passes_through_valid_and_missing() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("owners.json");
        // Missing → empty, no quarantine.
        assert!(load_or_quarantine(&p).is_empty());

        fs::write(&p, r#"{"claude:abc":{"owner_product":"codezilla"}}"#).unwrap();
        let m = load_or_quarantine(&p);
        assert_eq!(m.len(), 1);
        assert!(p.exists(), "valid file must be left in place");
    }

    #[test]
    fn unknown_cli_prefix_silently_skipped() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("owners.json");
        fs::write(
            &p,
            r#"{"gemini:abc":{"owner_product":"foo"},"claude:bar":{}}"#,
        )
        .unwrap();
        let m = load(&p).unwrap();
        assert_eq!(m.len(), 1);
        assert!(m.contains_key(&(Cli::Claude, "bar".into())));
    }

    #[test]
    fn register_and_unregister_round_trip() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("owners.json");
        register(
            &p,
            Cli::Claude,
            "abc",
            OwnerRecord {
                owner_product: Some("codezilla".into()),
                owner_thread_id: Some("t-1".into()),
                cwd: Some("/work".into()),
                seen_at: None,
            },
        )
        .unwrap();
        let m = load(&p).unwrap();
        assert!(m[&(Cli::Claude, "abc".into())].seen_at.is_some());
        assert_eq!(
            m.get(&(Cli::Claude, "abc".into()))
                .unwrap()
                .owner_product
                .as_deref(),
            Some("codezilla")
        );
        let removed = unregister(&p, Cli::Claude, "abc").unwrap();
        assert!(removed);
        assert!(load(&p).unwrap().is_empty());
    }

    const DAY: u64 = 86_400;
    const NOW: u64 = 100 * DAY;

    fn rec(seen_at: Option<u64>) -> OwnerRecord {
        OwnerRecord {
            owner_product: Some("codezilla".into()),
            owner_thread_id: Some("t".into()),
            cwd: None,
            seen_at,
        }
    }

    fn key(id: &str) -> ThreadKey {
        (Cli::Codex, id.to_string())
    }

    #[test]
    fn age_stamps_legacy_records_instead_of_dropping_them() {
        let mut m = HashMap::from([(key("legacy"), rec(None))]);
        assert!(age(&mut m, |_| None, NOW));
        assert_eq!(m[&key("legacy")].seen_at, Some(NOW));
    }

    #[test]
    fn age_drops_records_unseen_past_retention() {
        let mut m = HashMap::from([
            (key("old"), rec(Some(NOW - 15 * DAY))),
            (key("recent"), rec(Some(NOW - 13 * DAY))),
        ]);
        assert!(age(&mut m, |_| None, NOW));
        assert!(!m.contains_key(&key("old")));
        assert!(m.contains_key(&key("recent")));
    }

    #[test]
    fn age_keeps_and_refreshes_a_tracked_session() {
        // Registered long ago, but its session is live right now.
        let mut m = HashMap::from([(key("live"), rec(Some(NOW - 30 * DAY)))]);
        assert!(age(&mut m, |_| Some(NOW), NOW));
        assert_eq!(m[&key("live")].seen_at, Some(NOW));
    }

    #[test]
    fn age_leaves_a_freshly_seen_record_untouched() {
        let mut m = HashMap::from([(key("fresh"), rec(Some(NOW - DAY / 2)))]);
        assert!(!age(&mut m, |_| Some(NOW), NOW));
        assert_eq!(m[&key("fresh")].seen_at, Some(NOW - DAY / 2));
    }

    #[test]
    fn age_file_writes_only_when_something_changed() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("owners.json");
        register(&p, Cli::Codex, "a", rec(None)).unwrap();
        assert!(age_file(&p, |_| None, now_secs()).unwrap().is_none());
        let (aged, dropped) = age_file(&p, |_| None, now_secs() + 15 * DAY)
            .unwrap()
            .unwrap();
        assert!(aged.is_empty());
        assert_eq!(dropped, 1);
        assert!(load(&p).unwrap().is_empty());
    }

    #[test]
    fn parse_thread_key_round_trips() {
        assert_eq!(
            parse_thread_key("claude:abc-123"),
            Some((Cli::Claude, "abc-123".to_string()))
        );
        assert_eq!(
            parse_thread_key("codex:xyz"),
            Some((Cli::Codex, "xyz".to_string()))
        );
        assert_eq!(parse_thread_key("no-colon"), None);
        assert_eq!(parse_thread_key("foo:bar"), None); // unknown cli
    }
}
