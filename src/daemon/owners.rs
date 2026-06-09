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

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::install::atomic_write;
use crate::state::{Cli, ThreadKey};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRecord {
    pub owner_product: Option<String>,
    pub owner_thread_id: Option<String>,
    pub cwd: Option<String>,
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

/// Insert or update an owner record. Atomic write.
pub fn register(path: &Path, cli: Cli, thread_id: &str, record: OwnerRecord) -> Result<(), String> {
    let mut current = load(path).unwrap_or_default();
    current.insert((cli, thread_id.to_string()), record);
    write(path, &current)
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
            },
        )
        .unwrap();
        let m = load(&p).unwrap();
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
