//! Codex child rollouts → the same agent records as Claude's hooks.
//!
//! Only explicit `source.subagent.thread_spawn.parent_thread_id` links establish
//! ownership. Task paths are names, never identities; encrypted prompts are not
//! inspected. A background worker tails complete JSONL records incrementally,
//! including after restart, replacement, truncation and partial writes.

use crate::codex_binder::collect_rollout_files;
use crate::state::{
    self, Activity, Cli, HookEvent, HookEventExtra, HookEventKind, Liveness, ThreadState,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const READ_BUDGET: u64 = 8 * 1024 * 1024;
const MAX_LINE: usize = 4 * 1024 * 1024;

#[derive(Clone)]
struct Meta {
    id: String,
    parent: Option<String>,
    name: Option<String>,
    role: Option<String>,
    cwd: Option<String>,
    ts: f64,
}

fn timestamp(v: &Value) -> Option<f64> {
    let dt = chrono::DateTime::parse_from_rfc3339(v.get("timestamp")?.as_str()?).ok()?;
    Some(dt.timestamp_millis() as f64 / 1000.0)
}

fn string(v: &Value, key: &str) -> Option<String> {
    v.get(key)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn metadata(path: &Path) -> Option<Meta> {
    let mut line = String::new();
    BufReader::new(File::open(path).ok()?.take(MAX_LINE as u64))
        .read_line(&mut line)
        .ok()?;
    // Retry a still-being-written head on the next discovery sweep.
    if !line.ends_with('\n') {
        return None;
    }
    let v: Value = serde_json::from_str(&line).ok()?;
    if v.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    let p = v.get("payload")?;
    let spawn = &p["source"]["subagent"]["thread_spawn"];
    Some(Meta {
        id: string(p, "id").or_else(|| string(p, "session_id"))?,
        parent: string(spawn, "parent_thread_id"),
        name: string(p, "agent_path")
            .or_else(|| string(spawn, "agent_path"))
            .or_else(|| string(spawn, "agent_nickname")),
        role: string(spawn, "agent_role"),
        cwd: string(p, "cwd"),
        ts: timestamp(&v)?,
    })
}

#[derive(Default)]
struct Tail {
    identity: (u64, u64),
    offset: u64,
    boundary: Vec<u8>,
    pending: Vec<u8>,
    oversized: bool,
}

impl Tail {
    /// Returns complete records and whether the file was replaced/rewritten.
    fn read(&mut self, path: &Path) -> std::io::Result<(Vec<Value>, bool)> {
        let mut f = File::open(path)?;
        let m = f.metadata()?;
        let identity = (m.dev(), m.ino());
        let mut reset = identity != self.identity || m.len() < self.offset;
        if !reset && !self.boundary.is_empty() {
            f.seek(SeekFrom::Start(self.offset - self.boundary.len() as u64))?;
            let mut check = vec![0; self.boundary.len()];
            f.read_exact(&mut check)?;
            reset = check != self.boundary;
        }
        if reset {
            *self = Self {
                identity,
                ..Default::default()
            };
        }
        f.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        (&mut f).take(READ_BUDGET).read_to_end(&mut bytes)?;
        self.offset += bytes.len() as u64;
        let mut records = Vec::new();
        for part in bytes.split_inclusive(|b| *b == b'\n') {
            if self.pending.len() + part.len() > MAX_LINE {
                self.pending.clear();
                self.oversized = true;
            }
            if !self.oversized {
                self.pending.extend_from_slice(part);
            }
            if part.ends_with(b"\n") {
                if !self.oversized {
                    if let Ok(v) = serde_json::from_slice(&self.pending) {
                        records.push(v);
                    }
                }
                self.pending.clear();
                self.oversized = false;
            }
        }
        let start = self.offset.saturating_sub(64);
        f.seek(SeekFrom::Start(start))?;
        self.boundary.resize((self.offset - start) as usize, 0);
        f.read_exact(&mut self.boundary)?;
        Ok((records, reset))
    }
}

struct Entry {
    meta: Meta,
    tail: Tail,
    state: Option<ThreadState>,
    emitted: String,
}

pub struct Scanner {
    root: PathBuf,
    entries: HashMap<PathBuf, Entry>,
    discovered: Option<Instant>,
}

impl Scanner {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            entries: HashMap::new(),
            discovered: None,
        }
    }

    /// Called off the reducer thread. Roots are sessions already known from
    /// hooks; unrelated rollouts cannot become owned through cwd coincidence.
    pub fn scan(&mut self, roots: &[ThreadState]) -> Vec<ThreadState> {
        if roots.is_empty() {
            return Vec::new();
        }
        if self
            .discovered
            .map_or(true, |t| t.elapsed() >= Duration::from_secs(5))
        {
            let paths = collect_rollout_files(&self.root);
            let present: HashSet<_> = paths.iter().cloned().collect();
            self.entries.retain(|p, _| present.contains(p));
            for path in paths {
                self.entries.entry(path.clone()).or_insert_with(|| Entry {
                    meta: Meta {
                        id: String::new(),
                        parent: None,
                        name: None,
                        role: None,
                        cwd: None,
                        ts: 0.0,
                    },
                    tail: Tail::default(),
                    state: None,
                    emitted: String::new(),
                });
                // Session metadata is immutable; do not reread every historical
                // rollout's (potentially large) instruction block each sweep.
                if self.entries[&path].meta.id.is_empty() {
                    if let Some(meta) = metadata(&path) {
                        self.entries.get_mut(&path).unwrap().meta = meta;
                    }
                }
            }
            self.discovered = Some(Instant::now());
        }
        let parents: HashMap<_, _> = self
            .entries
            .values()
            .map(|e| (e.meta.id.clone(), e.meta.parent.clone()))
            .collect();
        let roots: HashMap<_, _> = roots.iter().map(|s| (s.thread_id.as_str(), s)).collect();
        let mut updates = Vec::new();
        for (path, entry) in &mut self.entries {
            let Some(mut parent) = entry.meta.parent.clone() else {
                continue;
            };
            let mut visited = HashSet::from([entry.meta.id.clone()]);
            let root = loop {
                if !visited.insert(parent.clone()) {
                    break None;
                }
                if let Some(Some(next)) = parents.get(&parent) {
                    parent = next.clone();
                } else {
                    break roots.get(parent.as_str()).copied();
                }
            };
            let Some(root) = root else { continue };
            let Ok((records, reset)) = entry.tail.read(path) else {
                continue;
            };
            if reset {
                entry.state = None;
                entry.emitted.clear();
                if let Some(meta) = metadata(path) {
                    if meta.id != entry.meta.id || meta.parent != entry.meta.parent {
                        entry.meta = meta;
                        entry.tail = Tail::default();
                        continue; // Resolve the new relationship on the next pass.
                    }
                    entry.meta = meta;
                }
            }
            let state = entry
                .state
                .get_or_insert_with(|| initial(&entry.meta, root, path));
            for record in records {
                reduce(state, &record);
            }
            // Nested agents belong to the visible root session; keep their full
            // task path in the name so siblings and grandchildren stay distinct.
            state.thread_id = state::agent_thread_id(&root.thread_id, &entry.meta.id);
            state.parent_thread_id = Some(root.thread_id.clone());
            state.pid = root.pid;
            state.pid_start = root.pid_start.clone();
            let mut snapshot = state.clone();
            if root.liveness == Liveness::Gone {
                snapshot.liveness = Liveness::Gone;
                snapshot.activity = Activity::Idle;
            }
            snapshot.subtitle = Some(crate::tool_display::format_for_thread(&snapshot));
            let fingerprint = serde_json::to_string(&snapshot).unwrap_or_default();
            if fingerprint != entry.emitted {
                entry.emitted = fingerprint;
                updates.push(snapshot);
            }
        }
        updates
    }
}

fn initial(meta: &Meta, root: &ThreadState, path: &Path) -> ThreadState {
    let ev = HookEvent {
        event: HookEventKind::SubagentStart,
        ts: meta.ts,
        cli: Cli::Codex,
        thread_id: root.thread_id.clone(),
        pid: root.pid,
        pid_start: root.pid_start.clone(),
        cwd: meta.cwd.clone(),
        transcript_path: Some(path.to_string_lossy().into()),
        agent_id: Some(meta.id.clone()),
        agent_type: meta.role.clone(),
        extra: HookEventExtra::default(),
    };
    let mut s = state::apply_agent_event(state::initial_agent_state(&ev), &ev);
    s.agent_name = meta.name.clone().or_else(|| Some("Agent".into()));
    s
}

fn event(
    s: &mut ThreadState,
    ts: f64,
    kind: HookEventKind,
    tool: Option<&str>,
    target: Option<String>,
) {
    if ts < s.last_event {
        return;
    }
    if tool.is_some() && target.is_none() {
        s.last_tool_target = None;
    }
    let ev = HookEvent {
        event: kind,
        ts,
        cli: Cli::Codex,
        thread_id: s.parent_thread_id.clone().unwrap_or_default(),
        pid: s.pid,
        pid_start: s.pid_start.clone(),
        cwd: s.cwd.clone(),
        transcript_path: s.transcript_path.clone(),
        agent_id: s.agent_id.clone(),
        agent_type: s.agent_type.clone(),
        extra: HookEventExtra {
            tool_name: tool.map(str::to_owned),
            tool_target: target,
            ..Default::default()
        },
    };
    *s = state::apply_agent_event(s.clone(), &ev);
}

fn path_text(s: &str) -> String {
    // Rollout tools use file:// URLs; decode percent-escaped UTF-8 paths too.
    let s = s.strip_prefix("file://").unwrap_or(s);
    let mut bytes = Vec::new();
    let mut iter = s.as_bytes().iter().copied();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let a = iter.next();
            let c = iter.next();
            if let (Some(a), Some(c)) = (a, c) {
                if let Ok(hex) = std::str::from_utf8(&[a, c]) {
                    if let Ok(n) = u8::from_str_radix(hex, 16) {
                        bytes.push(n);
                        continue;
                    }
                }
            }
            bytes.push(b);
            bytes.extend(a);
            bytes.extend(c);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn reduce(s: &mut ThreadState, v: &Value) {
    let Some(ts) = timestamp(v) else { return };
    if v["type"] != "event_msg" {
        return;
    }
    let p = &v["payload"];
    match p["type"].as_str().unwrap_or("") {
        "task_started" => event(s, ts, HookEventKind::SubagentStart, None, None),
        "task_complete" | "task_completed" | "turn_aborted" => {
            event(s, ts, HookEventKind::SubagentStop, None, None)
        }
        "item_started" | "item_completed" => {
            let i = &p["item"];
            let kind = if p["type"] == "item_started" {
                HookEventKind::PreToolUse
            } else {
                HookEventKind::ToolUse
            };
            match i["type"].as_str().unwrap_or("") {
                "CommandExecution" => {
                    if let Some(cwd) = i["cwd"].as_str() {
                        s.cwd = Some(path_text(cwd));
                        event(s, ts, kind, Some("Bash"), s.cwd.clone());
                    }
                    let command = i["command"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .flat_map(|s| s.chars().chain(std::iter::once(' ')))
                                .take(200)
                                .collect::<String>()
                        })
                        .or_else(|| string(i, "command"));
                    event(
                        s,
                        ts,
                        kind,
                        Some("Bash"),
                        command.map(|s| s.chars().take(200).collect()),
                    );
                }
                "FileChange" if kind == HookEventKind::ToolUse && i["status"] == "completed" => {
                    if let Some(changes) = i["changes"].as_object() {
                        for (path, change) in changes {
                            let path = path_text(path);
                            let path = if Path::new(&path).is_absolute() {
                                path
                            } else {
                                Path::new(s.cwd.as_deref().unwrap_or(""))
                                    .join(path)
                                    .to_string_lossy()
                                    .into_owned()
                            };
                            event(s, ts, kind, Some("Edit"), Some(path));
                            if let Some(dest) = change["move_path"].as_str() {
                                event(s, ts, kind, Some("Edit"), Some(path_text(dest)));
                            }
                        }
                    }
                }
                "McpToolCall" => {
                    let tool = format!(
                        "mcp__{}__{}",
                        i["server"].as_str().unwrap_or(""),
                        i["tool"].as_str().unwrap_or("")
                    );
                    event(s, ts, kind, Some(&tool), None);
                }
                // Assistant output/thinking is activity, but not a tool. It
                // refreshes liveness without evicting file/worktree evidence.
                "AgentMessage" | "Reasoning" if ts >= s.last_event => {
                    s.last_event = ts;
                    s.activity = Activity::Working;
                    s.liveness = Liveness::Live;
                }
                _ => {}
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "codex_agents_tests.rs"]
mod tests;
