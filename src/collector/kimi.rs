//! Collector for **kimi-code** sessions (launched via the `kimi` CLI).
//!
//! kimi-code stores sessions under
//! `~/.kimi-code/sessions/wd_<base>_<hash>/session_<uuid>/`, each holding
//! `agents/main/wire.jsonl` (the full event stream: tokens, chat, tool calls)
//! and `state.json` (title + timestamps). The config root keeps a
//! `session_index.jsonl` listing every `{sessionId, sessionDir, workDir}` —
//! that index is the authoritative discovery source (kimi's old `<md5(cwd)>`
//! layout was retired when it migrated `~/.kimi` → `~/.kimi-code`).
//!
//! Discovery is **index-driven**, not process-cwd-driven: every `kimi-code`
//! process `chdir`s to the config root, so `/proc/{pid}/cwd` can never yield a
//! project hash. Instead, each live `kimi-code` PID is attributed to a session
//! by walking its ancestor shell chain and matching the shell's cwd (the
//! directory kimi was launched from) against the session's `workDir`. A shared
//! server daemon (`server/lock` → pid) is the global "kimi is running" gate;
//! per-session exit detection stays best-effort (no PID file is written).
//!
//! Telemetry comes from `wire.jsonl`:
//! - `usage.record` — per-turn token breakdown (`inputOther` / `output` /
//!   `inputCacheRead` / `inputCacheCreation`, camelCase) + `model` + `time` (ms).
//!   kimi emits no context-window field, so context % is derived
//!   (last-turn input / hardcoded 262144 window) like the Claude collector.
//! - `context.append_message` — user/assistant chat turns.
//! - `context.append_loop_event` (`tool.call` / `tool.result` / `content.part`
//!   / `step.*`) — tool calls (name + args), streaming assistant text, steps.
//!
//! kimi exposes no rate-limit telemetry (managed OAuth), so it contributes
//! nothing to the quota panel (like OpenCode).

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
/// Hardcoded context window for the kimi-code model (no field is emitted).
const KIMI_CONTEXT_WINDOW: u64 = 262_144;
/// Secondary liveness window: a session also shows if its wire log was touched
/// this recently (the primary signal is attribution to a live PID).
const SESSION_FRESH_SECS: u64 = 300;
/// Working/Thinking vs Waiting freshness threshold (matches other collectors).
const ACTIVITY_FRESH_SECS: u64 = 30;

pub struct KimiCollector {
    /// kimi-code config root (default `~/.kimi-code`).
    config_root: PathBuf,
    /// Per-session incremental parse cache.
    cache: HashMap<String, KimiCache>,
}

impl KimiCollector {
    pub fn new() -> Self {
        Self {
            config_root: kimi_config_root(),
            cache: HashMap::new(),
        }
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        if !is_kimi_root(&self.config_root) {
            return vec![];
        }

        let self_pid = std::process::id();
        let entries = read_session_index(&self.config_root);

        // Global "kimi is running" gate: the shared server daemon is alive, or
        // at least one kimi-code process (server/TUI) is running. If kimi is
        // fully stopped, show nothing (sessions are not live).
        let server_pid = read_server_lock_pid(&self.config_root);
        let live_pids = Self::find_kimi_pids(&shared.process_info, self_pid);
        let kimi_running = server_pid.is_some_and(|p| shared.process_info.contains_key(&p))
            || !live_pids.is_empty();
        if !kimi_running {
            return vec![];
        }

        // Attribute each live kimi-code PID to a *workdir* by matching an
        // ancestor shell's cwd (kimi's launch directory) to a session workDir.
        // The PID's own cwd is skipped — kimi always chdir's to the config root.
        let mut pid_by_workdir: HashMap<String, u32> = HashMap::new();
        for &pid in &live_pids {
            for cwd in ancestor_workdirs(pid, &shared.process_info) {
                pid_by_workdir.entry(cwd).or_insert(pid);
            }
        }

        // kimi has no per-session PID file, so a live process can only be tied
        // to the *directory* it was launched from — not to a specific session.
        // Only the single most-recently-active session under each live workdir
        // is the "current" one. Without this, every historical session that ever
        // ran in a directory would resurface in the monitor whenever ONE
        // kimi-code process is alive in that directory (ghost sessions).
        let live_workdirs: HashSet<String> = pid_by_workdir.keys().cloned().collect();
        let current_sessions = current_session_ids(&entries, &live_workdirs);

        let model_default = read_kimi_model(&self.config_root);
        let now = SystemTime::now();
        let mut sessions = Vec::new();

        for entry in &entries {
            if !entry.session_dir.is_dir() {
                continue;
            }
            let wire_path = entry
                .session_dir
                .join("agents")
                .join("main")
                .join("wire.jsonl");
            let fresh = file_age_secs(&wire_path, now) < SESSION_FRESH_SECS;
            // Primary signal: this is the current session under a live workdir.
            // Fallback: recently active. Historical, non-current sessions in an
            // otherwise-live directory stay hidden unless freshly active.
            let attributed = current_sessions.contains(&entry.session_id);
            if !attributed && !fresh {
                continue;
            }

            let attached_pid = if !entry.work_dir.is_empty() {
                pid_by_workdir
                    .get(&entry.work_dir)
                    .copied()
                    .or(server_pid)
                    .unwrap_or(0)
            } else {
                server_pid.unwrap_or(0)
            };

            if let Some(session) = self.load_session(
                &entry.session_id,
                &entry.work_dir,
                &entry.session_dir,
                attached_pid,
                &model_default,
                shared,
            ) {
                sessions.push(session);
            }
        }

        // Drop cache entries for sessions no longer shown.
        let live: HashSet<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        self.cache.retain(|sid, _| live.contains(sid.as_str()));

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    /// Live `kimi-code` processes that are not descendants of abtop itself.
    /// kimi-cli rewrites argv[0] to `kimi-code` (setproctitle), so we match the
    /// first token's basename precisely (avoids matching `grep kimi-code` etc.).
    fn find_kimi_pids(process_info: &HashMap<u32, process::ProcInfo>, self_pid: u32) -> Vec<u32> {
        process_info
            .iter()
            .filter(|(pid, info)| {
                process::cmd_first_token_has_binary(&info.command, "kimi-code")
                    && !process::is_descendant_of(**pid, self_pid, process_info)
            })
            .map(|(pid, _)| *pid)
            .collect()
    }

    fn load_session(
        &mut self,
        session_id: &str,
        work_dir: &str,
        session_dir: &Path,
        pid: u32,
        model_default: &str,
        shared: &super::SharedProcessData,
    ) -> Option<AgentSession> {
        let session_id = session_id.to_string();

        let wire_path = session_dir.join("agents").join("main").join("wire.jsonl");
        let state_path = session_dir.join("state.json");

        let cache = self.cache.entry(session_id.clone()).or_default();

        // --- wire.jsonl: tokens + context + chat + tool calls (single pass) ---
        if wire_path.exists() {
            let identity = file_identity(&wire_path);
            let reset = cache.wire_identity != identity;
            let from = if reset { 0 } else { cache.wire_offset };
            let delta = parse_wire(&wire_path, from);
            // File replaced/shrank → re-accumulate from scratch.
            if reset {
                *cache = KimiCache::default();
                cache.wire_identity = identity;
            }
            if delta.new_offset > from || from == 0 {
                cache.wire_offset = delta.new_offset;
                cache.wire_identity = identity;
                cache.total_input += delta.input;
                cache.total_output += delta.output;
                cache.total_cache_read += delta.cache_read;
                cache.total_cache_create += delta.cache_create;
                if delta.saw_usage {
                    cache.context_tokens = delta.context_tokens;
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
                if let Some(t) = delta.last_activity {
                    cache.last_activity =
                        Some(cache.last_activity.map(|prev| prev.max(t)).unwrap_or(t));
                }
                cache.turn_count += delta.turn_count;
                if !delta.current_task.is_empty() {
                    cache.current_task = delta.current_task;
                }
                cache
                    .tool_calls
                    .extend(delta.tool_calls.into_iter().take(MAX_TOOL_CALLS));
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

        // --- state.json: title (kimi generates it; no summarizer needed) ---
        // New schema: top-level `title` (+ `isCustomTitle`). The legacy
        // `custom_title`/`plan_mode` fields are gone.
        let title = read_state_title(&state_path).unwrap_or_default();

        let model = if !cache.model.is_empty() {
            cache.model.clone()
        } else {
            model_default.to_string()
        };

        let proc = shared.process_info.get(&pid);
        let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

        // Status: mtime freshness of wire.jsonl + pending tool + descendant CPU.
        let pending_tool = !cache.current_task.is_empty();
        let has_active_descendant =
            process::has_active_descendant(pid, &shared.children_map, &shared.process_info, 5.0);
        let now = SystemTime::now();
        let fresh = cache.last_activity.is_some_and(|t| {
            now.duration_since(t)
                .map(|d| d.as_secs() < ACTIVITY_FRESH_SECS)
                .unwrap_or(false)
        });
        let status = if has_active_descendant || pending_tool {
            SessionStatus::Executing
        } else if fresh {
            SessionStatus::Thinking
        } else {
            SessionStatus::Waiting
        };

        let project_name = process::last_path_segment(work_dir)
            .unwrap_or("?")
            .to_string();

        let current_tasks = if !cache.current_task.is_empty() {
            vec![cache.current_task.clone()]
        } else if matches!(status, SessionStatus::Waiting) {
            vec!["waiting for input".to_string()]
        } else {
            vec!["thinking...".to_string()]
        };

        // Child process tree (ports/memory) — reuses abtop's shared process data.
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

        let context_percent = if KIMI_CONTEXT_WINDOW > 0 {
            (cache.context_tokens as f64 / KIMI_CONTEXT_WINDOW as f64) * 100.0
        } else {
            0.0
        };

        let started_at = wire_mtime_ms(&wire_path).unwrap_or(0);

        Some(AgentSession {
            agent_cli: "kimi",
            pid,
            session_id,
            cwd: work_dir.to_string(),
            project_name,
            started_at,
            status,
            model,
            effort: String::new(),
            context_percent,
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
            compaction_count: 0,
            context_window: KIMI_CONTEXT_WINDOW,
            subagents: vec![],
            mem_file_count: 0,
            mem_line_count: 0,
            children,
            initial_prompt: if !title.is_empty() {
                title.clone()
            } else {
                cache.initial_prompt.clone()
            },
            first_assistant_text: cache.first_assistant_text.clone(),
            chat_messages: cache.chat_messages.clone(),
            tool_calls: cache.tool_calls.clone(),
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: cache.file_accesses.clone(),
            config_root: super::abbrev_path(&self.config_root),
        })
    }
}

impl Default for KimiCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl super::AgentCollector for KimiCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

// ---------------------------------------------------------------------------
// Session index + server lock (discovery sources)
// ---------------------------------------------------------------------------

/// One row of `~/.kimi-code/session_index.jsonl`.
struct IndexEntry {
    session_id: String,
    session_dir: PathBuf,
    work_dir: String,
}

/// The set of session IDs that are the **current** (most-recently-active)
/// session under each *live* workdir.
///
/// kimi writes no per-session PID file, so a running `kimi-code` process can be
/// attributed only to the directory it was launched from (a workDir), not to a
/// specific session. Many historical sessions may share one workDir (kimi mints
/// a new `session_<uuid>/` per run). When a single process is alive in a
/// directory we must surface only the session it is actually driving — the one
/// whose wire log was written most recently — and keep older runs hidden,
/// otherwise every session that ever ran there reappears as a ghost.
fn current_session_ids(entries: &[IndexEntry], live_workdirs: &HashSet<String>) -> HashSet<String> {
    // workdir -> (latest wire mtime, current session id)
    let mut newest_mtime: HashMap<&str, u64> = HashMap::new();
    let mut current_id: HashMap<&str, String> = HashMap::new();
    for entry in entries {
        if entry.work_dir.is_empty() || !live_workdirs.contains(&entry.work_dir) {
            continue;
        }
        let wire = entry
            .session_dir
            .join("agents")
            .join("main")
            .join("wire.jsonl");
        let mtime = wire_mtime_ms(&wire).unwrap_or(0);
        match newest_mtime.get(entry.work_dir.as_str()) {
            Some(&m) if m >= mtime => {}
            _ => {
                newest_mtime.insert(entry.work_dir.as_str(), mtime);
                current_id.insert(entry.work_dir.as_str(), entry.session_id.clone());
            }
        }
    }
    current_id.into_values().collect()
}

/// Parse `session_index.jsonl` into session entries. Falls back to scanning
/// `sessions/wd_*/session_*/` (and the older `ses_*`) dirs when the index is
/// absent so discovery still works on partial installs.
fn read_session_index(root: &Path) -> Vec<IndexEntry> {
    let mut out = Vec::new();
    let index_path = root.join("session_index.jsonl");
    if let Ok(content) = fs::read_to_string(&index_path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(session_dir) = v
                .get("sessionDir")
                .and_then(|x| x.as_str())
                .map(PathBuf::from)
            else {
                continue;
            };
            let session_id = v
                .get("sessionId")
                .and_then(|x| x.as_str())
                .or_else(|| session_dir.file_name().and_then(|n| n.to_str()))
                .unwrap_or("")
                .to_string();
            let work_dir = v
                .get("workDir")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            out.push(IndexEntry {
                session_id,
                session_dir,
                work_dir,
            });
        }
    }
    if out.is_empty() {
        if let Ok(workdir_entries) = fs::read_dir(root.join("sessions")) {
            for wd in workdir_entries.flatten() {
                let wd_path = wd.path();
                if !wd_path.is_dir() {
                    continue;
                }
                let Ok(session_entries) = fs::read_dir(&wd_path) else {
                    continue;
                };
                // Newest session under this workdir by mtime.
                let mut best: Option<(SystemTime, PathBuf)> = None;
                for s in session_entries.flatten() {
                    if s.file_type().map(|ft| ft.is_symlink()).unwrap_or(true) {
                        continue;
                    }
                    let p = s.path();
                    if !p.is_dir() {
                        continue;
                    }
                    let mtime = s
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(UNIX_EPOCH);
                    match &best {
                        Some((b, _)) if &mtime <= b => {}
                        _ => best = Some((mtime, p)),
                    }
                }
                if let Some((_, p)) = best {
                    let session_id = p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();
                    out.push(IndexEntry {
                        session_id,
                        session_dir: p,
                        work_dir: String::new(),
                    });
                }
            }
        }
    }
    out
}

/// Read the shared kimi-code server PID from `server/lock` (`{"pid":N,...}`).
fn read_server_lock_pid(root: &Path) -> Option<u32> {
    let content = fs::read_to_string(root.join("server").join("lock")).ok()?;
    let v: Value = serde_json::from_str(&content).ok()?;
    v.get("pid")
        .and_then(|x| x.as_u64())
        .and_then(|n| u32::try_from(n).ok())
}

/// Walk the ancestor chain of `pid` (starting at its parent — the kimi process
/// itself chdir's to the config root, so its own cwd is meaningless) and return
/// each ancestor's cwd. The shell/pane cwd is kimi's launch directory, which
/// equals the session `workDir`.
fn ancestor_workdirs(pid: u32, process_info: &HashMap<u32, process::ProcInfo>) -> Vec<String> {
    let mut out = Vec::new();
    let Some(info) = process_info.get(&pid) else {
        return out;
    };
    let mut cur = info.ppid;
    let mut visited = HashSet::new();
    while cur != 0 && visited.insert(cur) {
        if let Some(cwd) = process_cwd(cur) {
            out.push(cwd);
        }
        match process_info.get(&cur) {
            Some(info) if info.ppid != 0 && info.ppid != cur => cur = info.ppid,
            _ => break,
        }
    }
    out
}

// ---------------------------------------------------------------------------
// wire.jsonl parsing (usage.record + context.append_message + loop events)
// ---------------------------------------------------------------------------

struct WireDelta {
    new_offset: u64,
    saw_usage: bool,
    context_tokens: u64,
    model: Option<String>,
    last_activity: Option<SystemTime>,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
    token_history: Vec<u64>,
    turn_count: u32,
    current_task: String,
    tool_calls: Vec<ToolCall>,
    chat_messages: Vec<ChatMessage>,
    file_accesses: Vec<FileAccess>,
    initial_prompt: String,
    first_assistant_text: String,
}

fn parse_wire(path: &Path, from_offset: u64) -> WireDelta {
    let mut delta = WireDelta {
        new_offset: from_offset,
        saw_usage: false,
        context_tokens: 0,
        model: None,
        last_activity: None,
        input: 0,
        output: 0,
        cache_read: 0,
        cache_create: 0,
        token_history: Vec::new(),
        turn_count: 0,
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
        // shrank/rotated → caller resets cache; report zero progress.
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
                    handle_wire_event(&val, &mut delta);
                }
                if !has_newline {
                    // incomplete trailing line — defer to next poll.
                    break;
                }
            }
            Err(_) => break,
        }
    }

    delta.new_offset = bytes_read;
    delta
}

/// Dispatch one parsed wire.jsonl line into `delta`.
fn handle_wire_event(val: &Value, delta: &mut WireDelta) {
    // Top-level event time (epoch ms) on every event.
    if let Some(t) = time_to_systemtime(val.get("time")) {
        delta.last_activity = Some(delta.last_activity.map(|prev| prev.max(t)).unwrap_or(t));
    }

    let event_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match event_type {
        "usage.record" => {
            delta.saw_usage = true;
            if let Some(u) = val.get("usage") {
                let inp = u.get("inputOther").and_then(|v| v.as_u64()).unwrap_or(0);
                let out = u.get("output").and_then(|v| v.as_u64()).unwrap_or(0);
                let cr = u
                    .get("inputCacheRead")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let cc = u
                    .get("inputCacheCreation")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                delta.input += inp;
                delta.output += out;
                delta.cache_read += cr;
                delta.cache_create += cc;
                // Context tokens ≈ this turn's input side (mirrors the Claude
                // collector's input + cache_read accounting).
                delta.context_tokens = inp + cr + cc;
                if delta.token_history.len() < 10_000 {
                    delta.token_history.push(inp + out + cr + cc);
                }
            }
            if let Some(m) = val.get("model").and_then(|m| m.as_str()) {
                if !m.is_empty() {
                    delta.model = Some(m.to_string());
                }
            }
        }
        "context.append_message" => {
            let Some(msg) = val.get("message") else {
                return;
            };
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
            let text = extract_text_from_content(msg.get("content"));
            match role {
                "assistant" => {
                    delta.turn_count += 1;
                    if delta.first_assistant_text.is_empty() && !text.is_empty() {
                        delta.first_assistant_text = truncate(&text, 200);
                    }
                    if !text.is_empty() {
                        push_chat(&mut delta.chat_messages, ChatRole::Assistant, text);
                    }
                }
                "user" => {
                    if delta.initial_prompt.is_empty() && !text.is_empty() {
                        delta.initial_prompt = truncate(&clean_prompt(&text), 50);
                    }
                    if !text.is_empty() {
                        push_chat(&mut delta.chat_messages, ChatRole::User, text);
                    }
                }
                _ => {}
            }
        }
        "context.append_loop_event" => {
            let Some(ev) = val.get("event") else {
                return;
            };
            match ev.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "tool.call" => {
                    let name = ev.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                    let args = ev.get("args").unwrap_or(&Value::Null);
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
                "content.part" if delta.first_assistant_text.is_empty() => {
                    // Streaming assistant text — a fallback first-assistant
                    // snapshot when append_message carried no text.
                    if let Some(part) = ev.get("part") {
                        if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                                let cleaned = super::sanitize_terminal_text(t);
                                if !cleaned.trim().is_empty() {
                                    delta.first_assistant_text = truncate(&cleaned, 200);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        "turn.prompt" if delta.initial_prompt.is_empty() => {
            let Some(arr) = val.get("input").and_then(|i| i.as_array()) else {
                return;
            };
            let text: String = arr
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                        b.get("text").and_then(|t| t.as_str()).map(String::from)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            if !text.trim().is_empty() {
                delta.initial_prompt = truncate(&clean_prompt(&text), 50);
            }
        }
        _ => {}
    }
}

/// Convert a wire `time` field (epoch milliseconds, number or string) to SystemTime.
fn time_to_systemtime(v: Option<&Value>) -> Option<SystemTime> {
    let ms = v
        .and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f as u64)))
        .or_else(|| {
            v.and_then(|x| x.as_str())
                .and_then(|s| s.parse::<u64>().ok())
        })?;
    UNIX_EPOCH.checked_add(std::time::Duration::from_millis(ms))
}

// ---------------------------------------------------------------------------
// state.json (title) and config.toml (model)
// ---------------------------------------------------------------------------

fn read_state_title(state_path: &Path) -> Option<String> {
    let val: Value = serde_json::from_str(&fs::read_to_string(state_path).ok()?).ok()?;
    let title = val
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim();
    if title.is_empty() {
        None
    } else {
        Some(super::redact_secrets(&truncate(title, 80)))
    }
}

/// Read the configured model's display name from `~/.kimi-code/config.toml`.
/// Hand-scanned (abtop has no TOML dep): resolves `default_model` → its
/// `[models."<id>"] display_name`. Falls back to the model id, then "kimi-code".
fn read_kimi_model(config_root: &Path) -> String {
    let config = match fs::read_to_string(config_root.join("config.toml")) {
        Ok(c) => c,
        Err(_) => return "kimi-code".to_string(),
    };
    let model_id = config
        .lines()
        .find_map(|l| {
            let l = l.trim();
            let v = l
                .strip_prefix("default_model")?
                .trim_start()
                .strip_prefix('=')?
                .trim()
                .trim_matches('"')
                .trim();
            if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            }
        })
        .unwrap_or_default();
    if model_id.is_empty() {
        return "kimi-code".to_string();
    }
    // Find `[models."<model_id>"]` section, then its display_name.
    let header = format!("[models.\"{}\"]", model_id);
    let mut in_section = false;
    for line in config.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_section = t == header;
            continue;
        }
        if in_section {
            if let Some(v) = t
                .strip_prefix("display_name")
                .and_then(|s| s.trim().strip_prefix('='))
            {
                let v = v.trim().trim_matches('"').trim();
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    model_id
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Default kimi-code config root: `$KIMI_CONFIG_DIR` if set, else prefer
/// `~/.kimi-code` (the post-migration root), falling back to the legacy
/// `~/.kimi` only if `.kimi-code` is absent. Among valid candidates, prefer the
/// one that ships `session_index.jsonl`.
fn kimi_config_root() -> PathBuf {
    if let Ok(dir) = std::env::var("KIMI_CONFIG_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return p;
        }
    }
    let home = dirs::home_dir().unwrap_or_default();
    let candidates = [home.join(".kimi-code"), home.join(".kimi")];
    let mut best: Option<(u8, PathBuf)> = None;
    for c in candidates {
        if is_kimi_root(&c) {
            let score = u8::from(c.join("session_index.jsonl").is_file());
            match &best {
                Some((s, _)) if *s >= score => {}
                _ => best = Some((score, c)),
            }
        }
    }
    best.map(|(_, p)| p)
        .unwrap_or_else(|| home.join(".kimi-code"))
}

fn is_kimi_root(path: &Path) -> bool {
    path.is_dir() && path.join("sessions").is_dir()
}

/// Age of a file's mtime in seconds (`u64::MAX` if missing/unreadable).
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

fn wire_mtime_ms(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
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

/// Extract concatenated text content from a `context.append_message` content
/// field — a string or an array of `{type,text}` / `{type,think}` blocks.
fn extract_text_from_content(content: Option<&Value>) -> String {
    let raw = match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|block| {
                let t = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if t == "text" || t == "think" {
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

/// Parse a tool's arguments object into a short display arg and an optional
/// file path (for the file-access audit). `args` is a JSON object (kimi's
/// `tool.call` event stores it as an object, not a JSON string).
fn parse_tool_args(name: &str, args: &Value) -> (String, Option<String>) {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .or_else(|| args.get("file_path").and_then(|p| p.as_str()))
        .or_else(|| args.get("filePath").and_then(|p| p.as_str()))
        .map(String::from);
    match name {
        "Shell" => {
            let cmd = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
            (
                super::redact_secrets(&truncate(cmd.lines().next().unwrap_or(cmd), 40)),
                None,
            )
        }
        "WriteFile" | "StrReplaceFile" | "ReadFile" | "ReadMediaFile" | "Edit" | "Write"
        | "Read" => {
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
        "ReadFile" | "ReadMediaFile" | "Read" => Some(FileOp::Read),
        "WriteFile" | "Write" => Some(FileOp::Write),
        "StrReplaceFile" | "Edit" => Some(FileOp::Edit),
        _ => None,
    }
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
struct KimiCache {
    wire_offset: u64,
    wire_identity: (u64, u64),
    // telemetry (cumulative / latest from usage.record)
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_create: u64,
    context_tokens: u64,
    model: String,
    last_activity: Option<SystemTime>,
    token_history: Vec<u64>,
    context_history: Vec<u64>,
    // structural (from context.append_message / loop events)
    turn_count: u32,
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

    #[test]
    fn wire_parses_usage_record_into_tokens_and_derived_context() {
        let tmp = tempfile::tempdir().unwrap();
        let wire = tmp.path().join("wire.jsonl");
        std::fs::write(
            &wire,
            r#"{"type":"metadata","protocol_version":"1.10"}
{"type":"usage.record","model":"kimi-code/kimi-for-coding","usage":{"inputOther":3822,"output":858,"inputCacheRead":14336,"inputCacheCreation":0},"usageScope":"turn","time":1781485995669}
{"type":"usage.record","model":"kimi-code/kimi-for-coding","usage":{"inputOther":100,"output":10,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"turn","time":1781485996000}
"#,
        )
        .unwrap();

        let delta = parse_wire(&wire, 0);
        assert!(delta.saw_usage);
        // cumulative token sums
        assert_eq!(delta.input, 3922);
        assert_eq!(delta.output, 868);
        assert_eq!(delta.cache_read, 14336);
        assert_eq!(delta.cache_create, 0);
        // latest turn's input side → derived context tokens (100+0+0)
        assert_eq!(delta.context_tokens, 100);
        assert_eq!(delta.model.as_deref(), Some("kimi-code/kimi-for-coding"));
        assert!(delta.last_activity.is_some());
        assert_eq!(delta.token_history.len(), 2);
    }

    #[test]
    fn wire_incremental_offset_only_parses_new_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let wire = tmp.path().join("wire.jsonl");
        let first = r#"{"type":"usage.record","model":"kimi-code/kimi-for-coding","usage":{"inputOther":10,"output":1,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"turn","time":1781485995000}
"#;
        std::fs::write(&wire, first).unwrap();
        let off = std::fs::metadata(&wire).unwrap().len();

        let d1 = parse_wire(&wire, 0);
        assert_eq!(d1.input, 10);
        assert_eq!(d1.new_offset, off);

        // append a second record; incremental parse must only see the new one.
        use std::io::Write;
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&wire)
                .unwrap();
            f.write_all(
                br#"{"type":"usage.record","model":"kimi-code/kimi-for-coding","usage":{"inputOther":20,"output":2,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"turn","time":1781485996000}
"#,
            )
            .unwrap();
        }

        let d2 = parse_wire(&wire, off);
        assert_eq!(d2.input, 20); // only the appended line
        assert_eq!(d2.context_tokens, 20);
    }

    #[test]
    fn wire_context_append_message_parses_chat_and_initial_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let wire = tmp.path().join("wire.jsonl");
        std::fs::write(
            &wire,
            r#"{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"organize a git commit"}]}}
{"type":"context.append_message","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}
"#,
        )
        .unwrap();

        let d = parse_wire(&wire, 0);
        assert_eq!(d.turn_count, 1);
        assert_eq!(d.initial_prompt, "organize a git commit");
        assert!(!d.first_assistant_text.is_empty());
        // user + assistant → 2 chat messages
        assert_eq!(d.chat_messages.len(), 2);
    }

    #[test]
    fn wire_loop_event_tool_call_parses_tools_and_file_access() {
        let tmp = tempfile::tempdir().unwrap();
        let wire = tmp.path().join("wire.jsonl");
        std::fs::write(
            &wire,
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","name":"Shell","args":{"command":"cd /x && git status"},"toolCallId":"t1"},"time":1781485995000}
{"type":"context.append_loop_event","event":{"type":"tool.call","name":"WriteFile","args":{"path":"/repo/src/main.rs"},"toolCallId":"t2"},"time":1781485996000}
"#,
        )
        .unwrap();

        let d = parse_wire(&wire, 0);
        assert_eq!(d.tool_calls.len(), 2);
        assert_eq!(d.tool_calls[0].name, "Shell");
        assert_eq!(d.current_task, "WriteFile src/main.rs");
        assert_eq!(d.file_accesses.len(), 1);
        assert_eq!(d.file_accesses[0].path, "/repo/src/main.rs");
        assert_eq!(d.file_accesses[0].operation, FileOp::Write);
    }

    #[test]
    fn state_title_parsed_from_new_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state.json");
        std::fs::write(
            &state,
            r#"{"title":"组织一波 git 提交","isCustomTitle":false,"createdAt":"2026-06-24T01:59:44.115Z","updatedAt":"2026-06-24T02:00:38.951Z"}"#,
        )
        .unwrap();
        assert_eq!(
            read_state_title(&state).as_deref(),
            Some("组织一波 git 提交")
        );
    }

    #[test]
    fn config_root_prefers_kimi_code_over_legacy_kimi() {
        // Both roots exist with sessions/; the one with session_index.jsonl wins.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let new_root = home.join(".kimi-code");
        let legacy = home.join(".kimi");
        std::fs::create_dir_all(new_root.join("sessions")).unwrap();
        std::fs::create_dir_all(legacy.join("sessions")).unwrap();
        std::fs::write(new_root.join("session_index.jsonl"), "").unwrap();

        std::env::set_var("HOME", home);
        std::env::remove_var("KIMI_CONFIG_DIR");
        let root = kimi_config_root();
        std::env::remove_var("HOME");
        assert_eq!(root, new_root);
    }

    #[test]
    fn current_session_ids_keeps_only_the_newest_per_live_workdir() {
        let tmp = tempfile::tempdir().unwrap();

        // Two historical sessions plus a fresh one, all under one workdir.
        let wd = "project_a";
        let dir = tmp.path().join("sessions").join("wd_a").join("ses");
        let entries: Vec<IndexEntry> = ["s1", "s2", "s3"]
            .iter()
            .map(|sid| {
                let sd = dir.join(sid);
                std::fs::create_dir_all(sd.join("agents/main")).unwrap();
                IndexEntry {
                    session_id: sid.to_string(),
                    session_dir: dir.join(sid),
                    work_dir: wd.to_string(),
                }
            })
            .collect();

        // s1 oldest, s2 middle, s3 newest (by wire mtime).
        for (i, sid) in ["s1", "s2", "s3"].iter().enumerate() {
            let wire = dir.join(sid).join("agents/main/wire.jsonl");
            std::fs::write(&wire, format!("run {}\n", i)).unwrap();
            let file = std::fs::File::open(&wire).unwrap();
            let mtime = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + i as u64);
            let _ = file.set_modified(mtime);
        }

        // With the workdir live, only the newest (s3) is current.
        let mut live = HashSet::new();
        live.insert(wd.to_string());
        let current = current_session_ids(&entries, &live);
        assert_eq!(current.len(), 1);
        assert!(current.contains("s3"), "expected only s3, got {:?}", current);

        // An unrelated live workdir must not surface these sessions.
        let mut other_live = HashSet::new();
        other_live.insert("project_other".to_string());
        assert!(current_session_ids(&entries, &other_live).is_empty());
    }

    #[test]
    fn session_index_parsed_into_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        std::fs::write(
            root.join("session_index.jsonl"),
            "{\"sessionId\":\"session_abc\",\"sessionDir\":\"/home/u/.kimi-code/sessions/wd_abtop_a3e08d75b3cc/session_abc\",\"workDir\":\"/home/u/abtop\"}\n\
             {\"sessionId\":\"session_def\",\"sessionDir\":\"/home/u/.kimi-code/sessions/wd_.kimi_9610029f55da/session_def\",\"workDir\":\"/home/u/.kimi\"}\n",
        )
        .unwrap();
        let entries = read_session_index(root);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].session_id, "session_abc");
        assert_eq!(entries[0].work_dir, "/home/u/abtop");
        assert_eq!(entries[1].work_dir, "/home/u/.kimi");
    }

    #[test]
    fn server_lock_pid_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("server")).unwrap();
        std::fs::write(
            root.join("server").join("lock"),
            r#"{"pid":10452,"started_at":"2026-06-24T02:04:19.321Z","host":"127.0.0.1","port":58627}"#,
        )
        .unwrap();
        assert_eq!(read_server_lock_pid(root), Some(10452));
    }

    #[test]
    fn time_to_systemtime_handles_ms_number_and_string() {
        let n = serde_json::json!(1781485995669u64);
        let s = serde_json::json!("1781485995669");
        assert!(time_to_systemtime(Some(&n)).is_some());
        assert_eq!(time_to_systemtime(Some(&n)), time_to_systemtime(Some(&s)));
        assert!(time_to_systemtime(Some(&serde_json::json!("not-a-number"))).is_none());
    }
}
