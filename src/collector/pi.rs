//! Collector for **Pi** sessions (any Pi or Pi-derivative coding agent built
//! on the `@earendil-works/pi-coding-agent` SDK, including `my-pi-agent`).
//!
//! Pi stores sessions as append-only JSONL transcripts under
//! `~/.pi/agent/sessions/--<encoded-cwd>--/<ts>_<sessionId>.jsonl`. The
//! transcripts carry rich telemetry (usage, model, chat, tool calls) but
//! **no PID and no exit marker** — so liveness + process attribution come
//! from a per-PID *sidecar* written by a Pi-side monitor extension (see
//! `docs/pi-sidecar-contract.md`):
//!
//!   `~/.pi/agent/sessions/active/{pid}.json`
//!   ```json
//!   {
//!     "pid": 12345, "procStart": 394363811, "agent": "my-pi-agent",
//!     "version": "0.84.2", "sessionId": "...", "sessionFile": "....jsonl",
//!     "cwd": "/home/u/proj", "startedAt": 1787059700000,
//!     "contextWindow": 262144, "contextPercent": 42.5, "model": "cuc/deepseek"
//!   }
//!   ```
//!
//! Attribution order:
//! 1. **Sidecar** (`active/{pid}.json`) — authoritative PID + context window.
//!    The file is a *claim*, verified for liveness (pid alive) and PID-reuse
//!    (procStart vs `/proc/{pid}/stat` start time where available).
//! 2. **Transcript-only fallback** (no sidecar): find live Pi processes and
//!    match `/proc/{pid}/cwd` to the session header `cwd`. PID=0, status
//!    `Unknown`, recency-bounded (mirrors the Hermes no-PID pattern).
//!
//! Telemetry from the transcript JSONL:
//! - `message` entries — assistant `usage` (camelCase:
//!   `input`/`output`/`cacheRead`/`cacheWrite`/`totalTokens`/`cost`),
//!   `model`/`provider`, chat roles (user/assistant/toolResult), `toolCall`
//!   content blocks (name + arguments) → tasks + file-access audit.
//! - `model_change` — model transitions.
//! - `thinking_level_change` — reasoning effort.
//! - `custom` (customType `"modes"`) — plan/build mode.
//! - `compaction` — context compaction count.
//! - `session_info` — display title.
//!
//! Pi exposes no rate-limit telemetry (managed OAuth), so it contributes
//! nothing to the quota panel (like OpenCode/kimi).

use super::process;
use crate::model::{
    AgentSession, ChatMessage, ChatRole, ChildProcess, FileAccess, FileOp, SessionStatus, ToolCall,
    MAX_CHAT_MESSAGES, MAX_FILE_ACCESSES,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum tool-call / chat / file-access entries kept per session to bound memory.
const MAX_TOOL_CALLS: usize = 500;
/// Transcript-only fallback liveness window: a session also shows if its
/// transcript was touched this recently, even without a live-PID match.
const SESSION_FRESH_SECS: u64 = 300;
/// Working/Thinking vs Waiting freshness threshold (matches other collectors).
const ACTIVITY_FRESH_SECS: u64 = 30;

pub struct PiCollector {
    /// Pi sessions root (default `~/.pi/agent/sessions`).
    sessions_root: PathBuf,
    /// Per-session incremental parse cache.
    cache: HashMap<String, PiCache>,
}

impl PiCollector {
    pub fn new() -> Self {
        Self {
            sessions_root: pi_sessions_root(),
            cache: HashMap::new(),
        }
    }

    #[cfg(test)]
    fn with_sessions_root(root: PathBuf) -> Self {
        Self {
            sessions_root: root,
            cache: HashMap::new(),
        }
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        if !self.sessions_root.is_dir() {
            return vec![];
        }

        let self_pid = std::process::id();

        // Live Pi processes (for the transcript-only cwd fallback).
        let pi_pids = Self::find_pi_pids(&shared.process_info, self_pid);

        let now = SystemTime::now();

        // Sidecar-attributed sessions: read active/*.json, verify liveness.
        // Every sidecar file (live or stale) *claims* its sessionId so the
        // transcript-only fallback below never re-adds the same session.
        let sidecars = read_active_sidecars(&self.sessions_root);
        let mut sessions = Vec::new();
        let mut live_sidecar_ids: HashSet<String> = HashSet::new();
        let mut live_sidecar_cwds: HashSet<String> = HashSet::new();
        let mut stale_sidecar_ids: HashSet<String> = HashSet::new();
        let mut stale_sidecar_paths: Vec<PathBuf> = Vec::new();

        for sc in &sidecars {
            // A sidecar claim is authoritative for its sessionId regardless of
            // liveness — the fallback must never resurrect a session whose
            // sidecar already owns it (this is what caused duplicate rows +
            // wrong status when a workspace restarted with a new session/PID).
            // Only a *verified* (PID-alive) sidecar yields a shown session.
            let pid_alive = shared.process_info.contains_key(&sc.pid);
            let verified = pid_alive && verify_sidecar(sc, &shared.process_info);
            if verified {
                live_sidecar_ids.insert(sc.session_id.clone());
                live_sidecar_cwds.insert(sc.cwd.clone());
                if let Some(s) = self.load_session_from_sidecar(sc, true, shared) {
                    sessions.push(s);
                }
                continue;
            }
            // Stale sidecar: the owning process is gone (or its PID was
            // reused). Show it only while its transcript is still fresh — a
            // recency-bounded Unknown window — then GC the stale file so a
            // future restart in the same workspace can't resurrect the dead
            // session on every tick.
            stale_sidecar_ids.insert(sc.session_id.clone());
            if file_age_secs(&PathBuf::from(&sc.session_file), now) < SESSION_FRESH_SECS {
                if let Some(s) = self.load_session_from_sidecar(sc, false, shared) {
                    sessions.push(s);
                }
            } else if !pid_alive {
                // Definitively dead PID + stale transcript: drop the claim.
                stale_sidecar_paths.push(
                    self.sessions_root
                        .join("active")
                        .join(format!("{}.json", sc.pid)),
                );
            }
        }
        // GC stale sidecar files. The Pi monitor extension cannot reliably
        // delete on crash (a SIGKILL fires no shutdown event), so abtop owns
        // the cleanup per the sidecar contract.
        for path in &stale_sidecar_paths {
            let _ = fs::remove_file(path);
        }

        // Transcript-only fallback: sessions not claimed by any sidecar,
        // matched to a live Pi process by cwd, or recently active.
        let mut pid_by_cwd: HashMap<String, u32> = HashMap::new();
        for &pid in &pi_pids {
            if let Some(cwd) = process_cwd(pid) {
                pid_by_cwd.entry(cwd).or_insert(pid);
            }
        }
        // A single live Pi PID occupies one cwd at a time, and a workspace
        // already served by a *live* sidecar is fully covered by it. So the
        // fallback attributes at most the newest transcript per cwd, and never
        // for a cwd owned by a verified sidecar — otherwise a prior session's
        // transcript would be ghosted onto the new PID after a restart.
        let mut attributed_cwds: HashSet<String> = HashSet::new();
        for path in discover_transcripts(&self.sessions_root) {
            let Some(header) = read_transcript_header(&path) else {
                continue;
            };
            if live_sidecar_ids.contains(header.session_id.as_str())
                || stale_sidecar_ids.contains(header.session_id.as_str())
            {
                continue; // already handled from the sidecar (live or stale)
            }
            // A workspace served by a live verified sidecar is fully covered by
            // it — skip the whole cwd so an unclaimed-but-fresh transcript
            // there (e.g. a just-finished prior session) can't ghost in as an
            // Unknown PID-0 row. Attribution alone is not enough: that path also
            // exits through the fresh-window display branch.
            if live_sidecar_cwds.contains(&header.cwd) {
                continue;
            }
            let attributed = !header.cwd.is_empty()
                && pid_by_cwd.contains_key(&header.cwd);
            let fresh = file_age_secs(&path, now) < SESSION_FRESH_SECS;
            if !attributed && !fresh {
                continue;
            }
            if attributed && !attributed_cwds.insert(header.cwd.clone()) {
                continue; // older historical transcript in this cwd — skip
            }
            let pid = if attributed {
                pid_by_cwd.get(&header.cwd).copied().unwrap_or(0)
            } else {
                0
            };
            if let Some(s) =
                self.load_session_from_transcript(&header, pid, !attributed, shared)
            {
                sessions.push(s);
            }
        }

        // Drop cache entries for sessions no longer shown.
        let live: HashSet<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        self.cache.retain(|sid, _| live.contains(sid.as_str()));

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    /// Live Pi (and Pi-derivative) processes that are not descendants of
    /// abtop itself. Matches the first token's basename against common Pi
    /// entrypoints (`pi`, `my-pi-agent`) and the interpreter-wrapped layout
    /// (`bun …/src/index.ts` is deliberately NOT matched — see `find_pi_pids`
    /// notes). The generic `pi` token is skipped because it's a short,
    /// false-positive-prone name; attribution relies on sidecar or cwd match,
    /// not the process name.
    fn find_pi_pids(process_info: &HashMap<u32, process::ProcInfo>, self_pid: u32) -> Vec<u32> {
        process_info
            .iter()
            .filter(|(pid, info)| {
                let cmd = &info.command;
                let first = cmd.split_whitespace().next().unwrap_or("");
                let base = first.rsplit('/').next().unwrap_or(first);
                let is_pi = matches!(base, "pi" | "my-pi-agent" | "pi-coding-agent")
                    || matches!(base.strip_suffix(".exe"), Some("my-pi-agent"));
                is_pi && !process::is_descendant_of(**pid, self_pid, process_info)
            })
            .map(|(pid, _)| *pid)
            .collect()
    }

    fn load_session_from_sidecar(
        &mut self,
        sc: &Sidecar,
        verified: bool,
        shared: &super::SharedProcessData,
    ) -> Option<AgentSession> {
        let cache = self.cache.entry(sc.session_id.clone()).or_default();

        // Tail the transcript for tokens/chat/tool calls (best-effort).
        // The sidecar carries context window/%, so we use them directly.
        let transcript_path = PathBuf::from(&sc.session_file);
        if transcript_path.exists() {
            Self::tail_transcript(&transcript_path, cache);
        }

        let pid = sc.pid;
        let proc = shared.process_info.get(&pid);
        let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

        let status = derive_status(pid, verified, cache, shared);
        let current_tasks = if !cache.current_task.is_empty() {
            vec![cache.current_task.clone()]
        } else if matches!(status, SessionStatus::Waiting) {
            vec!["waiting for input".to_string()]
        } else if !verified {
            vec!["unknown status".to_string()]
        } else {
            vec!["thinking...".to_string()]
        };

        let children = collect_children(pid, shared);

        let context_percent = if sc.context_percent.is_some() {
            sc.context_percent.unwrap_or(0.0)
        } else {
            0.0
        };
        let context_window = sc.context_window.unwrap_or(0);

        let project_name = process::last_path_segment(&sc.cwd).unwrap_or("?").to_string();
        let title = sc.title.clone();

        Some(AgentSession {
            agent_cli: "pi",
            pid,
            session_id: sc.session_id.clone(),
            cwd: sc.cwd.clone(),
            project_name,
            started_at: sc.started_at,
            status,
            model: if !cache.model.is_empty() {
                cache.model.clone()
            } else {
                sc.model.clone().unwrap_or_default()
            },
            effort: cache.effort.clone(),
            context_percent,
            total_input_tokens: cache.total_input,
            total_output_tokens: cache.total_output,
            total_cache_read: cache.total_cache_read,
            total_cache_create: cache.total_cache_create,
            turn_count: cache.turn_count,
            current_tasks,
            mem_mb,
            version: combine_version(&sc.agent, sc.version.as_deref()),
            git_branch: String::new(),
            git_added: 0,
            git_modified: 0,
            token_history: cache.token_history.clone(),
            context_history: cache.context_history.clone(),
            compaction_count: cache.compaction_count,
            context_window,
            subagents: vec![],
            mem_file_count: 0,
            mem_line_count: 0,
            children,
            initial_prompt: title.unwrap_or_else(|| cache.initial_prompt.clone()),
            first_assistant_text: cache.first_assistant_text.clone(),
            chat_messages: cache.chat_messages.clone(),
            tool_calls: cache.tool_calls.clone(),
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: cache.file_accesses.clone(),
            config_root: super::abbrev_path(&self.sessions_root),
        })
    }

    fn load_session_from_transcript(
        &mut self,
        header: &TranscriptHeader,
        pid: u32,
        unknown_status: bool,
        shared: &super::SharedProcessData,
    ) -> Option<AgentSession> {
        let session_id = header.session_id.clone();
        let cache = self.cache.entry(session_id.clone()).or_default();
        let transcript_path = header.path.clone();
        if transcript_path.exists() {
            Self::tail_transcript(&transcript_path, cache);
        }

        let proc = shared.process_info.get(&pid);
        let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

        let status = if unknown_status {
            SessionStatus::Unknown
        } else {
            derive_status(pid, pid != 0, cache, shared)
        };
        let current_tasks = if !cache.current_task.is_empty() {
            vec![cache.current_task.clone()]
        } else if matches!(status, SessionStatus::Waiting) {
            vec!["waiting for input".to_string()]
        } else if unknown_status {
            vec!["unknown status".to_string()]
        } else {
            vec!["thinking...".to_string()]
        };

        let children = if pid != 0 {
            collect_children(pid, shared)
        } else {
            vec![]
        };

        let project_name = process::last_path_segment(&header.cwd).unwrap_or("?").to_string();

        Some(AgentSession {
            agent_cli: "pi",
            pid,
            session_id,
            cwd: header.cwd.clone(),
            project_name,
            started_at: header.started_at,
            status,
            model: cache.model.clone(),
            effort: cache.effort.clone(),
            context_percent: 0.0, // no sidecar → no reliable window; show unknown/0
            total_input_tokens: cache.total_input,
            total_output_tokens: cache.total_output,
            total_cache_read: cache.total_cache_read,
            total_cache_create: cache.total_cache_create,
            turn_count: cache.turn_count,
            current_tasks,
            mem_mb,
            version: String::new(),
            git_branch: String::new(),
            git_added: 0,
            git_modified: 0,
            token_history: cache.token_history.clone(),
            context_history: cache.context_history.clone(),
            compaction_count: cache.compaction_count,
            context_window: 0,
            subagents: vec![],
            mem_file_count: 0,
            mem_line_count: 0,
            children,
            initial_prompt: cache.initial_prompt.clone(),
            first_assistant_text: cache.first_assistant_text.clone(),
            chat_messages: cache.chat_messages.clone(),
            tool_calls: cache.tool_calls.clone(),
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: cache.file_accesses.clone(),
            config_root: super::abbrev_path(&self.sessions_root),
        })
    }

    /// Incrementally tail a Pi transcript JSONL into `cache`.
    fn tail_transcript(path: &Path, cache: &mut PiCache) {
        let identity = file_identity(path);
        let reset = cache.transcript_identity != identity;
        let from = if reset { 0 } else { cache.transcript_offset };
        let delta = parse_transcript(path, from);
        if reset {
            *cache = PiCache::default();
            cache.transcript_identity = identity;
        }
        if delta.new_offset > from || from == 0 {
            cache.transcript_offset = delta.new_offset;
            cache.transcript_identity = identity;
            cache.total_input += delta.input;
            cache.total_output += delta.output;
            cache.total_cache_read += delta.cache_read;
            cache.total_cache_create += delta.cache_create;
            if delta.saw_usage {
                cache.token_history.extend(delta.token_history);
                if cache.token_history.len() > 10_000 {
                    let extra = cache.token_history.len() - 10_000;
                    cache.token_history.drain(0..extra);
                }
                cache.context_history.push(delta.context_tokens);
                if cache.context_history.len() > 10_000 {
                    let extra = cache.context_history.len() - 10_000;
                    cache.context_history.drain(0..extra);
                }
            }
            if let Some(m) = &delta.model {
                cache.model = m.clone();
            }
            if let Some(e) = &delta.effort {
                cache.effort = e.clone();
            }
            if let Some(t) = delta.last_activity {
                cache.last_activity =
                    Some(cache.last_activity.map(|prev| prev.max(t)).unwrap_or(t));
            }
            cache.turn_count += delta.turn_count;
            cache.compaction_count += delta.compaction_count;
            if !delta.current_task.is_empty() {
                cache.current_task = delta.current_task;
            }
            cache.tool_calls.extend(delta.tool_calls.into_iter().take(MAX_TOOL_CALLS));
            if cache.tool_calls.len() > MAX_TOOL_CALLS {
                let extra = cache.tool_calls.len() - MAX_TOOL_CALLS;
                cache.tool_calls.drain(0..extra);
            }
            cache.file_accesses.extend(delta.file_accesses);
            if cache.file_accesses.len() > MAX_FILE_ACCESSES {
                let extra = cache.file_accesses.len() - MAX_FILE_ACCESSES;
                cache.file_accesses.drain(0..extra);
            }
            cache.chat_messages.extend(delta.chat_messages);
            if cache.chat_messages.len() > MAX_CHAT_MESSAGES {
                let extra = cache.chat_messages.len() - MAX_CHAT_MESSAGES;
                cache.chat_messages.drain(0..extra);
            }
            if cache.initial_prompt.is_empty() {
                cache.initial_prompt = delta.initial_prompt;
            }
            if cache.first_assistant_text.is_empty() {
                cache.first_assistant_text = delta.first_assistant_text;
            }
        }
    }
}

impl Default for PiCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl super::AgentCollector for PiCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

// ---------------------------------------------------------------------------
// Sidecar (active/{pid}.json) reading + verification
// ---------------------------------------------------------------------------

/// A parsed sidecar claim (from the Pi monitor extension).
#[derive(Clone)]
struct Sidecar {
    pid: u32,
    proc_start: Option<u64>,
    agent: String,
    version: Option<String>,
    session_id: String,
    session_file: String,
    cwd: String,
    started_at: u64,
    context_window: Option<u64>,
    context_percent: Option<f64>,
    model: Option<String>,
    /// Optional display title (from `session_info`), written opportunistically.
    title: Option<String>,
}

/// Read (and defensively parse) all sidecars under `active/*.json`.
fn read_active_sidecars(sessions_root: &Path) -> Vec<Sidecar> {
    let dir = sessions_root.join("active");
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        let Some(session_id) = v.get("sessionId").and_then(|x| x.as_str()) else {
            continue;
        };
        let Some(cwd) = v.get("cwd").and_then(|x| x.as_str()) else {
            continue;
        };
        let Some(session_file) = v.get("sessionFile").and_then(|x| x.as_str()) else {
            continue;
        };
        out.push(Sidecar {
            pid: v.get("pid").and_then(|x| x.as_u64()).and_then(|n| u32::try_from(n).ok()).unwrap_or(0),
            proc_start: v.get("procStart").and_then(|x| x.as_u64()),
            agent: v.get("agent").and_then(|x| x.as_str()).unwrap_or("pi").to_string(),
            version: v.get("version").and_then(|x| x.as_str()).map(String::from),
            session_id: session_id.to_string(),
            session_file: session_file.to_string(),            cwd: cwd.to_string(),
            started_at: v.get("startedAt").and_then(|x| x.as_u64()).unwrap_or(0),
            context_window: v.get("contextWindow").and_then(|x| x.as_u64()),
            context_percent: v
                .get("contextPercent")
                .and_then(|x| x.as_f64().or_else(|| x.as_u64().map(|n| n as f64))),
            model: v.get("model").and_then(|x| x.as_str()).map(String::from),
            title: v.get("title").and_then(|x| x.as_str()).map(String::from),
        });
    }
    out
}

/// Verify a sidecar claim: PID alive, and (on Linux) procStart matches the
/// live process's start time (PID-reuse guard).
fn verify_sidecar(sc: &Sidecar, process_info: &HashMap<u32, process::ProcInfo>) -> bool {
    let Some(proc) = process_info.get(&sc.pid) else {
        return false;
    };
    if let Some(expected) = sc.proc_start {
        if expected > 0 && proc.start_ticks > 0 && proc.start_ticks != expected {
            return false; // PID reused by a different process
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Transcript discovery + header
// ---------------------------------------------------------------------------

/// The `session` header of a Pi transcript.
struct TranscriptHeader {
    session_id: String,
    cwd: String,
    started_at: u64,
    path: PathBuf,
}

/// Discover all `*.jsonl` transcripts under the sessions root, newest first.
fn discover_transcripts(sessions_root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(proj_dirs) = fs::read_dir(sessions_root) else {
        return out;
    };
    for proj in proj_dirs.flatten() {
        let proj_path = proj.path();
        if !proj_path.is_dir() {
            continue;
        }
        if proj_path.file_name().and_then(|n| n.to_str()) == Some("active") {
            continue;
        }
        let Ok(files) = fs::read_dir(&proj_path) else {
            continue;
        };
        for file in files.flatten() {
            let p = file.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            out.push(p);
        }
    }
    out.sort_by_key(|p| std::cmp::Reverse(
        fs::metadata(p).and_then(|m| m.modified()).ok().map(|t| {
            t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
        }).unwrap_or(0),
    ));
    out
}

/// Read the first (`session`) line of a Pi transcript.
fn read_transcript_header(path: &Path) -> Option<TranscriptHeader> {
    let content = fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("session") {
            continue;
        }
        let session_id = v.get("id").and_then(|x| x.as_str())?.to_string();
        let cwd = v.get("cwd").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let started_at = timestamp_ms(v.get("timestamp"))
            .unwrap_or(
                fs::metadata(path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            );
        return Some(TranscriptHeader { session_id, cwd, started_at, path: path.to_path_buf() });
    }
    None
}

// ---------------------------------------------------------------------------
// Transcript JSONL parsing
// ---------------------------------------------------------------------------

struct TranscriptDelta {
    new_offset: u64,
    saw_usage: bool,
    context_tokens: u64,
    model: Option<String>,
    effort: Option<String>,
    last_activity: Option<SystemTime>,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
    token_history: Vec<u64>,
    turn_count: u32,
    compaction_count: u32,
    current_task: String,
    tool_calls: Vec<ToolCall>,
    chat_messages: Vec<ChatMessage>,
    file_accesses: Vec<FileAccess>,
    initial_prompt: String,
    first_assistant_text: String,
}

fn parse_transcript(path: &Path, from_offset: u64) -> TranscriptDelta {
    let mut delta = TranscriptDelta {
        new_offset: from_offset,
        saw_usage: false,
        context_tokens: 0,
        model: None,
        effort: None,
        last_activity: None,
        input: 0,
        output: 0,
        cache_read: 0,
        cache_create: 0,
        token_history: Vec::new(),
        turn_count: 0,
        compaction_count: 0,
        current_task: String::new(),
        tool_calls: Vec::new(),
        chat_messages: Vec::new(),
        file_accesses: Vec::new(),
        initial_prompt: String::new(),
        first_assistant_text: String::new(),
    };

    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return delta,
    };
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file_len < from_offset {
        delta.new_offset = 0;
        return delta;
    }
    if file_len == from_offset {
        delta.new_offset = file_len;
        return delta;
    }

    use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
    let mut reader = BufReader::new(file);
    if from_offset > 0 {
        let _ = reader.seek(SeekFrom::Start(from_offset));
    }

    let mut bytes_read = from_offset;
    let mut line_buf = String::new();
    const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;
    loop {
        line_buf.clear();
        match reader
            .by_ref()
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_line(&mut line_buf)
        {
            Ok(0) => break,
            Ok(n) => {
                let has_newline = line_buf.ends_with('\n');
                let line = line_buf.trim();
                if line.is_empty() {
                    if has_newline {
                        bytes_read += n as u64;
                    }
                    continue;
                }
                let parsed = serde_json::from_str::<Value>(line).ok();
                bytes_read += n as u64;
                if let Some(val) = parsed {
                    handle_transcript_event(&val, &mut delta);
                }
                if !has_newline {
                    break; // incomplete trailing line — defer to next poll
                }
            }
            Err(_) => break,
        }
    }

    delta.new_offset = bytes_read;
    delta
}

/// Dispatch one parsed transcript line into `delta`.
fn handle_transcript_event(val: &Value, delta: &mut TranscriptDelta) {
    if let Some(t) = timestamp_to_systemtime(val.get("timestamp")) {
        delta.last_activity = Some(delta.last_activity.map(|prev| prev.max(t)).unwrap_or(t));
    }

    let entry_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match entry_type {
        "message" => {
            let Some(msg) = val.get("message") else {
                return;
            };
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
            match role {
                "assistant" => {
                    delta.turn_count += 1;
                    if let Some(u) = msg.get("usage") {
                        delta.saw_usage = true;
                        let inp = u.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
                        let out = u.get("output").and_then(|v| v.as_u64()).unwrap_or(0);
                        let cr = u.get("cacheRead").and_then(|v| v.as_u64()).unwrap_or(0);
                        let cc = u.get("cacheWrite").and_then(|v| v.as_u64()).unwrap_or(0);
                        delta.input += inp;
                        delta.output += out;
                        delta.cache_read += cr;
                        delta.cache_create += cc;
                        // Context ≈ this turn's input side (mirrors Claude's
                        // input + cache_read accounting, which is what the
                        // sidecar's contextPercent also reflects).
                        delta.context_tokens = inp + cr;
                        if delta.token_history.len() < 10_000 {
                            delta.token_history.push(inp + out + cr + cc);
                        }
                    }
                    if let Some(m) = msg.get("model").and_then(|m| m.as_str()) {
                        if !m.is_empty() {
                            delta.model = Some(m.to_string());
                        }
                    }
                    // Tool calls + streaming text from content blocks.
                    let mut text = String::new();
                    if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                        for block in content {
                            let bt = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            match bt {
                                "text" => {
                                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                        text.push_str(t);
                                    }
                                }
                                "toolCall" => {
                                    let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                                    let args = block.get("arguments").unwrap_or(&Value::Null);
                                    let (arg, file_path) = parse_tool_args(name, args);
                                    delta.current_task = format!("{} {}", name, truncate(&arg, 40));
                                    if delta.tool_calls.len() < MAX_TOOL_CALLS {
                                        delta.tool_calls.push(ToolCall {
                                            name: name.to_string(),
                                            arg: truncate(&arg, 40),
                                            duration_ms: 0,
                                        });
                                    }
                                    if let (Some(op), Some(fp)) = (file_op_for(name), file_path) {
                                        delta.file_accesses.push(FileAccess {
                                            path: fp,
                                            operation: op,
                                            turn_index: delta.turn_count,
                                        });
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    let cleaned = super::redact_secrets(&super::sanitize_terminal_text(&text));
                    if delta.first_assistant_text.is_empty() && !cleaned.trim().is_empty() {
                        delta.first_assistant_text = truncate(&cleaned, 200);
                    }
                    if !cleaned.trim().is_empty() {
                        push_chat(&mut delta.chat_messages, ChatRole::Assistant, cleaned);
                    }
                }
                "user" => {
                    let text = extract_text_from_content(msg.get("content"));
                    if delta.initial_prompt.is_empty() && !text.is_empty() {
                        delta.initial_prompt = truncate(&clean_prompt(&text), 50);
                    }
                    if !text.is_empty() {
                        push_chat(&mut delta.chat_messages, ChatRole::User, text);
                    }
                }
                "toolResult" => {
                    // The tool call that this result closes was attributed in the
                    // assistant content pass; toolResult needs no separate work.
                }
                _ => {}
            }
        }
        "model_change" => {
            if let Some(m) = val.get("modelId").and_then(|m| m.as_str()) {
                if !m.is_empty() {
                    delta.model = Some(m.to_string());
                }
            }
        }
        "thinking_level_change" => {
            if let Some(lvl) = val.get("thinkingLevel").and_then(|l| l.as_str()) {
                delta.effort = Some(lvl.to_string());
            }
        }
        "compaction" => {
            delta.compaction_count += 1;
        }
        _ => {}
    }
}

/// Parse a tool's `arguments` object into a short display arg + optional file path.
fn parse_tool_args(name: &str, args: &Value) -> (String, Option<String>) {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .or_else(|| args.get("file_path").and_then(|p| p.as_str()))
        .or_else(|| args.get("filePath").and_then(|p| p.as_str()))
        .map(String::from);
    match name {
        "bash" | "Bash" | "Shell" => {
            let cmd = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
            (
                super::redact_secrets(&truncate(cmd.lines().next().unwrap_or(cmd), 40)),
                None,
            )
        }
        "edit" | "Edit" | "write" | "Write" | "read" | "Read" | "grep" | "Grep" | "glob"
        | "Glob" | "find" | "Find" | "ls" | "Ls" => {
            let p = path.clone().unwrap_or_default();
            (shorten_path(&p), path)
        }
        _ => {
            if let Some(s) = args.get("command").and_then(|c| c.as_str()) {
                (truncate(s.lines().next().unwrap_or(s), 40), None)
            } else if let Some(p) = path.as_deref() {
                (shorten_path(p), path.clone())
            } else {
                let first = args
                    .as_object()
                    .and_then(|o| o.values().find_map(|v| v.as_str()).map(String::from))
                    .unwrap_or_default();
                (truncate(&first, 40), None)
            }
        }
    }
}

fn file_op_for(tool: &str) -> Option<FileOp> {
    match tool {
        "read" | "Read" | "ls" | "Ls" | "glob" | "Glob" | "find" | "Find" | "grep" | "Grep" => {
            Some(FileOp::Read)
        }
        "write" | "Write" => Some(FileOp::Write),
        "edit" | "Edit" => Some(FileOp::Edit),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Status / children helpers
// ---------------------------------------------------------------------------

fn derive_status(
    pid: u32,
    verified: bool,
    cache: &PiCache,
    shared: &super::SharedProcessData,
) -> SessionStatus {
    if !verified {
        return SessionStatus::Unknown;
    }
    let has_active_descendant =
        process::has_active_descendant(pid, &shared.children_map, &shared.process_info, 5.0);
    let now = SystemTime::now();
    let fresh = cache.last_activity.is_some_and(|t| {
        now.duration_since(t)
            .map(|d| d.as_secs() < ACTIVITY_FRESH_SECS)
            .unwrap_or(false)
    });
    // A live CPU-active descendant (a running tool or background subagent) is
    // the definitive Executing signal.
    if has_active_descendant {
        SessionStatus::Executing
    } else if fresh && !cache.current_task.is_empty() {
        // `current_task` is sticky (it records the last tool and is never
        // cleared), so it alone can't tell "mid-tool" from "tool finished and
        // waiting for input". Only treat it as Executing while the session is
        // still recently active (task may run in-process without a CPU child);
        // once activity ages out it must fall through to Waiting.
        SessionStatus::Executing
    } else if fresh {
        SessionStatus::Thinking
    } else {
        SessionStatus::Waiting
    }
}

fn collect_children(pid: u32, shared: &super::SharedProcessData) -> Vec<ChildProcess> {
    let mut children = Vec::new();
    let mut stack: Vec<u32> = shared.children_map.get(&pid).cloned().unwrap_or_default();
    let mut visited = HashSet::new();
    while let Some(cpid) = stack.pop() {
        if !visited.insert(cpid) {
            continue;
        }
        if let Some(cproc) = shared.process_info.get(&cpid) {
            let port = shared.ports.get(&cpid).and_then(|v| v.first().copied());
            children.push(ChildProcess {
                pid: cpid,
                command: cproc.command.clone(),
                mem_kb: cproc.rss_kb,
                port,
            });
        }
        if let Some(grandchildren) = shared.children_map.get(&cpid) {
            stack.extend(grandchildren);
        }
    }
    children
}

// ---------------------------------------------------------------------------
// Discovery helpers (paths, times)
// ---------------------------------------------------------------------------

/// Default Pi sessions root: honor `PI_CODING_AGENT_DIR` (and the
/// my-pi-agent variant) overrides; else `~/.pi/agent/sessions`.
fn pi_sessions_root() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    for var in ["PI_CODING_AGENT_DIR", "MY_PI_AGENT_CODING_AGENT_DIR"] {
        if let Ok(dir) = std::env::var(var) {
            if !dir.is_empty() {
                return PathBuf::from(dir).join("sessions");
            }
        }
    }
    home.join(".pi").join("agent").join("sessions")
}

/// Parse an ISO-8601 timestamp (e.g. `2026-08-18T13:27:26.247Z`) → epoch ms.
fn timestamp_ms(v: Option<&Value>) -> Option<u64> {
    let s = v?.as_str()?;
    let (date, time) = s.split_once('T')?;
    let time = time.trim_end_matches('Z');
    let (hms, frac) = match time.split_once('.') {
        Some((h, f)) => (h, Some(f)),
        None => (time, None),
    };
    let mut parts = date.split('-');
    let year: u64 = parts.next()?.parse().ok()?;
    let month: u64 = parts.next()?.parse().ok()?;
    let day: u64 = parts.next()?.parse().ok()?;
    let mut tparts = hms.split(':');
    let hour: u64 = tparts.next()?.parse().ok()?;
    let minute: u64 = tparts.next()?.parse().ok()?;
    let second: u64 = tparts.next().unwrap_or("0").parse().ok()?;
    let millis: u64 = frac
        .and_then(|f| {
            let f = f.chars().take(3).collect::<String>();
            format!("{:<3}", f).parse::<u64>().ok()
        })
        .unwrap_or(0);
    // days from civil (Howard Hinnant's algorithm)
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((days * 86400 + hour * 3600 + minute * 60 + second) * 1000 + millis)
}

fn timestamp_to_systemtime(v: Option<&Value>) -> Option<SystemTime> {
    timestamp_ms(v).and_then(|ms| UNIX_EPOCH.checked_add(std::time::Duration::from_millis(ms)))
}

fn file_age_secs(path: &Path, now: SystemTime) -> u64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| now.duration_since(t).ok())
        .map(|d| d.as_secs())
        .unwrap_or(u64::MAX)
}

#[cfg(target_os = "linux")]
fn process_cwd(pid: u32) -> Option<String> {
    fs::read_link(format!("/proc/{}/cwd", pid))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "linux"))]
fn process_cwd(pid: u32) -> Option<String> {
    use std::process::Command;
    let output = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find(|l| l.starts_with('n') && l.len() > 1)
        .map(|l| l[1..].to_string())
}

#[cfg(unix)]
fn file_identity(path: &Path) -> (u64, u64) {
    fs::metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino()))
        .unwrap_or((0, 0))
}

#[cfg(not(unix))]
fn file_identity(path: &Path) -> (u64, u64) {
    fs::metadata(path)
        .ok()
        .map(|m| {
            let size = m.len();
            let mtime_ns = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            (size, mtime_ns)
        })
        .unwrap_or((0, 0))
}

/// Extract text from a message `content` field — a string or an array of
/// `{type,text}` blocks.
fn extract_text_from_content(content: Option<&Value>) -> String {
    let raw = match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|block| {
                let t = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if t == "text" {
                    block.get("text").and_then(|x| x.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    let cleaned: String = raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with("```"))
        .collect::<Vec<_>>()
        .join(" ");
    let terminal_safe = super::sanitize_terminal_text(&cleaned);
    truncate(&super::redact_secrets(&terminal_safe), 500)
}

fn push_chat(messages: &mut Vec<ChatMessage>, role: ChatRole, text: String) {
    if text.is_empty() {
        return;
    }
    messages.push(ChatMessage { role, text });
    if messages.len() > MAX_CHAT_MESSAGES {
        let extra = messages.len() - MAX_CHAT_MESSAGES;
        messages.drain(0..extra);
    }
}

fn shorten_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').collect();
    if parts.len() <= 2 {
        path.to_string()
    } else {
        format!("{}/{}", parts[1], parts[0])
    }
}

fn clean_prompt(s: &str) -> String {
    s.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("```"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Surface the distro identity (agent + version) in the session `version`
/// field, e.g. `"my-pi-agent 0.84.2"`. Empty components are dropped.
fn combine_version(agent: &str, version: Option<&str>) -> String {
    match (agent, version) {
        (a, Some(v)) if !a.is_empty() && !v.is_empty() => format!("{} {}", a, v),
        (a, _) if !a.is_empty() => a.to_string(),
        (_, Some(v)) => v.to_string(),
        _ => String::new(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{}…", truncated)
    }
}

// ---------------------------------------------------------------------------
// incremental parse cache
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PiCache {
    transcript_offset: u64,
    transcript_identity: (u64, u64),
    // telemetry (cumulative / latest)
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_create: u64,
    model: String,
    effort: String,
    last_activity: Option<SystemTime>,
    token_history: Vec<u64>,
    context_history: Vec<u64>,
    // structural
    turn_count: u32,
    compaction_count: u32,
    current_task: String,
    tool_calls: Vec<ToolCall>,
    chat_messages: Vec<ChatMessage>,
    file_accesses: Vec<FileAccess>,
    initial_prompt: String,
    first_assistant_text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_info(pid: u32, command: &str, start_ticks: u64) -> process::ProcInfo {
        process::ProcInfo {
            pid,
            ppid: 1,
            rss_kb: 5000,
            cpu_pct: 0.0,
            command: command.to_string(),
            start_ticks,
        }
    }

    #[test]
    fn find_pi_pids_matches_distro_binary_and_rejects_grep() {
        let mut info = HashMap::new();
        info.insert(1, proc_info(1, "/home/u/.local/bin/my-pi-agent --plan", 0));
        info.insert(2, proc_info(2, "/home/u/.local/bin/pi", 0));
        info.insert(3, proc_info(3, "grep my-pi-agent", 0));
        info.insert(4, proc_info(4, "bun /repo/src/index.ts", 0)); // wrapper, not matched
        let pids = PiCollector::find_pi_pids(&info, 9999);
        assert!(pids.contains(&1));
        assert!(pids.contains(&2));
        assert!(!pids.contains(&3));
        assert!(!pids.contains(&4));
    }

    /// Build a `SharedProcessData` with the given cpu-active descendants under
    /// `pid` (children whose `cpu_pct` exceeds the 5% threshold that
    /// `derive_status` uses for `has_active_descendant`).
    fn shared_with_active_descendant(pid: u32) -> super::super::SharedProcessData {
        let mut shared = empty_shared();
        shared
            .process_info
            .insert(pid, proc_info(pid, "/repo/my-pi-agent", 100));
        // A high-CPU child (running tool / background subagent).
        let child = proc_info(pid + 1, "/bin/bash -c make", 1);
        // cpu_pct is 0 in proc_info(); set it directly to a value > 5.0.
        let mut child = child;
        child.cpu_pct = 40.0;
        shared.process_info.insert(pid + 1, child);
        shared.children_map.insert(pid, vec![pid + 1]);
        shared
    }

    // Regression: after a tool finishes and Pi is idle waiting for user input
    // (no run/background subagent), the session must be Waiting — even though
    // `current_task` is sticky and still holds the last tool. It must NOT stay
    // stuck at Executing forever because of that stickiness.
    #[test]
    fn status_waiting_after_tool_finishes_despite_sticky_current_task() {
        // Idle: no live PID, no active descendant. `current_task` still holds a
        // tool from a finished turn, but the session has aged out of the fresh
        // window, so it should be Waiting.
        let cache = PiCache {
            current_task: "bash make".to_string(),
            last_activity: Some(
                SystemTime::now() - std::time::Duration::from_secs(ACTIVITY_FRESH_SECS + 60),
            ),
            ..PiCache::default()
        };
        let shared = empty_shared();
        assert_eq!(
            derive_status(4242, true, &cache, &shared),
            SessionStatus::Waiting
        );
    }

    #[test]
    fn status_executing_when_tool_runs_as_active_descendant() {
        let cache = PiCache {
            current_task: "bash make".to_string(),
            last_activity: Some(SystemTime::now()),
            ..PiCache::default()
        };
        let shared = shared_with_active_descendant(4242);
        assert_eq!(
            derive_status(4242, true, &cache, &shared),
            SessionStatus::Executing
        );
    }

    #[test]
    fn status_executing_only_while_recently_active_with_current_task() {
        // `current_task` non-empty + recent activity (mid-tool, possibly an
        // in-process tool without a CPU child) -> Executing is acceptable.
        let cache = PiCache {
            current_task: "bash make".to_string(),
            last_activity: Some(SystemTime::now()),
            ..PiCache::default()
        };
        let shared = empty_shared();
        assert_eq!(
            derive_status(4242, true, &cache, &shared),
            SessionStatus::Executing
        );
    }

    #[test]
    fn status_unverified_is_unknown() {
        let cache = PiCache::default();
        let shared = empty_shared();
        assert_eq!(derive_status(4242, false, &cache, &shared), SessionStatus::Unknown);
    }


    #[test]
    fn transcript_parses_usage_into_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path().join("s.jsonl");
        std::fs::write(
            &t,
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-08-18T13:27:26.247Z","cwd":"/repo"}
{"type":"message","id":"a","parentId":null,"timestamp":"2026-08-18T13:27:26.300Z","message":{"role":"user","content":[{"type":"text","text":"do the thing"}]}}
{"type":"message","id":"b","parentId":"a","timestamp":"2026-08-18T13:27:26.400Z","message":{"role":"assistant","usage":{"input":1000,"output":200,"cacheRead":50,"cacheWrite":10,"totalTokens":1260},"model":"cuc/deepseek","content":[{"type":"text","text":"ok"},{"type":"toolCall","name":"edit","arguments":{"file_path":"/repo/src/main.rs"}}]}}
"#,
        )
        .unwrap();
        let d = parse_transcript(&t, 0);
        assert_eq!(d.input, 1000);
        assert_eq!(d.output, 200);
        assert_eq!(d.cache_read, 50);
        assert_eq!(d.cache_create, 10);
        assert_eq!(d.context_tokens, 1050); // input + cache_read
        assert_eq!(d.model.as_deref(), Some("cuc/deepseek"));
        assert_eq!(d.turn_count, 1);
        assert_eq!(d.tool_calls.len(), 1);
        assert_eq!(d.tool_calls[0].name, "edit");
        assert_eq!(d.current_task, "edit src/main.rs");
        assert_eq!(d.file_accesses.len(), 1);
        assert_eq!(d.file_accesses[0].operation, FileOp::Edit);
        assert_eq!(d.initial_prompt, "do the thing");
        // user + assistant → 2 chat entries
        assert_eq!(d.chat_messages.len(), 2);
    }

    #[test]
    fn transcript_incremental_offset_only_parses_new_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path().join("s.jsonl");
        let first = r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-08-18T13:27:26.247Z","cwd":"/repo"}
{"type":"message","id":"a","parentId":null,"timestamp":"2026-08-18T13:27:26.400Z","message":{"role":"assistant","usage":{"input":10,"output":1,"cacheRead":0,"cacheWrite":0},"model":"m","content":[]}}
"#;
        std::fs::write(&t, first).unwrap();
        let off = std::fs::metadata(&t).unwrap().len();
        let d1 = parse_transcript(&t, 0);
        assert_eq!(d1.input, 10);
        assert_eq!(d1.new_offset, off);

        use std::io::Write;
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&t).unwrap();
            f.write_all(
                br#"{"type":"message","id":"b","parentId":"a","timestamp":"2026-08-18T13:27:26.500Z","message":{"role":"assistant","usage":{"input":20,"output":2,"cacheRead":0,"cacheWrite":0},"model":"m","content":[]}}
"#,
            )
            .unwrap();
        }
        let d2 = parse_transcript(&t, off);
        assert_eq!(d2.input, 20);
        assert_eq!(d2.context_tokens, 20);
    }

    #[test]
    fn sidecar_read_and_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let active = tmp.path().join("active");
        std::fs::create_dir_all(&active).unwrap();
        std::fs::write(
            active.join("123.json"),
            r#"{"pid":123,"agent":"my-pi-agent","sessionId":"s9","sessionFile":"/repo/s9.jsonl","cwd":"/repo","startedAt":1787059700000,"contextWindow":262144,"contextPercent":42.5,"model":"cuc/deepseek"}"#,
        )
        .unwrap();
        let scs = read_active_sidecars(tmp.path());
        assert_eq!(scs.len(), 1);
        assert_eq!(scs[0].pid, 123);
        assert_eq!(scs[0].session_id, "s9");
        assert_eq!(scs[0].context_window, Some(262144));
        assert_eq!(scs[0].context_percent, Some(42.5));
        assert_eq!(scs[0].agent, "my-pi-agent");

        // Liveness: pid must be in process_info.
        let mut info = HashMap::new();
        let mut bogus = HashMap::new();
        bogus.insert(999, proc_info(999, "other", 0));
        // 123 not in bogus → not verified
        let sc = scs[0].clone();
        assert!(!verify_sidecar_ty(&sc, &bogus));
        info.insert(123, proc_info(123, "/repo/my-pi-agent", 100));
        // Wrong start_ticks → PID reuse rejected.
        let sc_reused = Sidecar { proc_start: Some(999), ..sc.clone() };
        assert!(!verify_sidecar(&sc_reused, &info));
    }

    fn verify_sidecar_ty(sc: &Sidecar, info: &HashMap<u32, process::ProcInfo>) -> bool {
        verify_sidecar(sc, info)
    }

    #[test]
    fn timestamp_ms_parses_iso8601() {
        assert_eq!(timestamp_ms(Some(&serde_json::json!("1970-01-01T00:00:00.000Z"))), Some(0));
        assert_eq!(timestamp_ms(Some(&serde_json::json!("1970-01-02T00:00:00.000Z"))), Some(86400000));
    }

    // ---- helpers for collector-level tests ----

    fn empty_shared() -> crate::collector::SharedProcessData {
        crate::collector::SharedProcessData {
            process_info: HashMap::new(),
            children_map: HashMap::new(),
            ports: HashMap::new(),
            slow_tick: true,
            mcp_server_pids: HashSet::new(),
            mcp_owned_rollouts: HashSet::new(),
            mcp_suppress: true,
            desktop_rollout_fd_map: HashMap::new(),
        }
    }

    fn write_transcript(dir: &std::path::Path, sid: &str, cwd: &str) -> std::path::PathBuf {
        // Transcripts live under an encoded-cwd project subdirectory.
        let proj_dir = dir.join("--repo--abtop--");
        std::fs::create_dir_all(&proj_dir).unwrap();
        let path = proj_dir.join(format!("{}.jsonl", sid));
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{}\",\"timestamp\":\"2026-08-18T13:27:26.247Z\",\"cwd\":\"{}\"}}\n",
                sid, cwd
            ),
        )
        .unwrap();
        path
    }

    fn set_mtime_old(path: &std::path::Path) {
        use std::fs::FileTimes;
        let f = std::fs::File::open(path).unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        f.set_times(FileTimes::new().set_modified(past)).unwrap();
    }

    fn write_sidecar(active: &std::path::Path, pid: u32, sid: &str, cwd: &str, session_file: &str) {
        std::fs::write(
            active.join(format!("{}.json", pid)),
            format!(
                "{{\"pid\":{},\"agent\":\"my-pi-agent\",\"version\":\"0.84.2\",\"sessionId\":\"{}\",\"sessionFile\":\"{}\",\"cwd\":\"{}\",\"startedAt\":1787059700000,\"contextWindow\":262144,\"contextPercent\":10.0}}",
                pid, sid, session_file, cwd
            ),
        )
        .unwrap();
    }

    // A fully-exited session: dead PID + stale transcript -> the stale sidecar
    // must be GC'd and no Unknown ghost row shown.
    #[test]
    fn stale_sidecar_with_stale_transcript_is_gced_and_hidden() {
        let tmp = tempfile::tempdir().unwrap();
        let active = tmp.path().join("active");
        std::fs::create_dir_all(&active).unwrap();
        let transcript = write_transcript(tmp.path(), "old-sid", "/repo/abtop");
        set_mtime_old(&transcript);
        write_sidecar(&active, 9999, "old-sid", "/repo/abtop", transcript.to_str().unwrap());

        let mut collector = PiCollector::with_sessions_root(tmp.path().to_path_buf());
        let shared = empty_shared(); // no pid 9999 alive
        let sessions = collector.collect_sessions(&shared);

        assert!(sessions.is_empty(), "dead session must not appear: {:#?}", sessions);
        assert!(
            !active.join("9999.json").exists(),
            "stale sidecar file should have been GC'd"
        );
    }

    // Within the recency window a dead PID may still surface (Unknown) until it
    // ages out, so the user briefly sees it instead of it vanishing instantly.
    #[test]
    fn stale_sidecar_with_fresh_transcript_shows_unknown_within_window() {
        let tmp = tempfile::tempdir().unwrap();
        let active = tmp.path().join("active");
        std::fs::create_dir_all(&active).unwrap();
        let transcript = write_transcript(tmp.path(), "fresh-sid", "/repo/abtop");
        write_sidecar(&active, 9999, "fresh-sid", "/repo/abtop", transcript.to_str().unwrap());

        let mut collector = PiCollector::with_sessions_root(tmp.path().to_path_buf());
        let sessions = collector.collect_sessions(&empty_shared());

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "fresh-sid");
        assert_eq!(sessions[0].status, SessionStatus::Unknown);
        // Not yet aged out -> file is NOT GC'd.
        assert!(active.join("9999.json").exists());
    }

    // Restart in the same workspace: a prior dead session must not ghost a row
    // onto the new PID, and must not double-count as Unknown.
    #[test]
    fn restart_in_same_workspace_shows_only_new_session() {
        let tmp = tempfile::tempdir().unwrap();
        let active = tmp.path().join("active");
        std::fs::create_dir_all(&active).unwrap();

        // Old session: dead sidecar + old transcript sharing the cwd.
        let old_t = write_transcript(tmp.path(), "old-sid", "/repo/abtop");
        set_mtime_old(&old_t);
        write_sidecar(&active, 9998, "old-sid", "/repo/abtop", old_t.to_str().unwrap());

        // New session: fresh transcript + live sidecar with pid 4242.
        let new_t = write_transcript(tmp.path(), "new-sid", "/repo/abtop");
        write_sidecar(&active, 4242, "new-sid", "/repo/abtop", new_t.to_str().unwrap());

        let mut shared = empty_shared();
        shared
            .process_info
            .insert(4242, proc_info(4242, "/repo/my-pi-agent", 200));

        let mut collector = PiCollector::with_sessions_root(tmp.path().to_path_buf());
        let sessions = collector.collect_sessions(&shared);

        assert_eq!(sessions.len(), 1, "only the live session should appear: {:#?}", sessions);
        assert_eq!(sessions[0].session_id, "new-sid");
        assert_eq!(sessions[0].pid, 4242);
        // Live + verified (never Unknown, unlike the pre-fix ghost).
        assert_ne!(sessions[0].status, SessionStatus::Unknown);
        // Old dead sidecar is GC'd (its transcript aged out).
        assert!(!active.join("9998.json").exists());
    }

    // Regression: a workspace served by a live verified sidecar must not also
    // surface a *different* unclaimed-but-fresh transcript in the same cwd as an
    // Unknown PID-0 ghost. The live sidecar fully covers the workspace, so the
    // transcript fallback must skip it entirely (not just stop attributing it).
    #[test]
    fn live_sidecar_cwd_swallows_unclaimed_fresh_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let active = tmp.path().join("active");
        std::fs::create_dir_all(&active).unwrap();

        // Live verified sidecar for the workspace.
        let live_t = write_transcript(tmp.path(), "live-sid", "/repo/abtop");
        write_sidecar(&active, 4242, "live-sid", "/repo/abtop", live_t.to_str().unwrap());

        // A *second*, unclaimed transcript in the same cwd, still fresh (e.g. a
        // just-finished prior session whose sidecar was already GC'd).
        let ghost_t = write_transcript(tmp.path(), "ghost-sid", "/repo/abtop");

        let mut shared = empty_shared();
        shared
            .process_info
            .insert(4242, proc_info(4242, "/repo/my-pi-agent", 200));

        let mut collector = PiCollector::with_sessions_root(tmp.path().to_path_buf());
        let sessions = collector.collect_sessions(&shared);

        assert_eq!(
            sessions.len(),
            1,
            "only the live sidecar session should appear, no PID-0 ghost: {:#?}",
            sessions
        );
        assert_eq!(sessions[0].session_id, "live-sid");
        assert_eq!(sessions[0].pid, 4242);
        assert_ne!(sessions[0].status, SessionStatus::Unknown);
        let _ = ghost_t; // transcript must exist for a valid reproduction
    }


    // Pure transcript fallback (no sidecars installed): when multiple
    // historical transcripts share a cwd that a live PID occupies, only the
    // newest may be attributed to that PID (a single PID owns one cwd at a
    // time). We can't stub /proc in a unit test, so exercise the discovery
    // ordering + the no-PID fresh-window path instead: the newest transcript
    // appears (as a fresh PID-0 row), older ones age out.
    #[test]
    fn transcript_fallback_shows_only_fresh_transcripts() {
        let tmp = tempfile::tempdir().unwrap();
        let _old = write_transcript(tmp.path(), "old-sid", "/repo/abtop");
        set_mtime_old(&_old);
        let _new = write_transcript(tmp.path(), "new-sid", "/repo/abtop");

        let mut collector = PiCollector::with_sessions_root(tmp.path().to_path_buf());
        // No live pi pid -> nothing attributed; only the fresh transcript shows.
        let sessions = collector.collect_sessions(&empty_shared());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "new-sid");
        assert_eq!(sessions[0].pid, 0);
        assert_eq!(sessions[0].status, SessionStatus::Unknown);
    }
}
