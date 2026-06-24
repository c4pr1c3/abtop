//! Collector for **kimi-code** sessions (launched via the `kimi` CLI).
//!
//! kimi-code stores sessions under `~/.kimi/sessions/<md5(cwd)>/<session_uuid>/`,
//! each holding `context.jsonl` (conversation), `wire.jsonl` (raw API wire log)
//! and `state.json` (session state). A running `kimi` process rewrites its
//! argv[0] to `kimi-code` (via setproctitle), so we discover processes by that
//! token and link each to its session through `/proc/{pid}/cwd` → `md5(cwd)`.
//!
//! Telemetry is richer than Claude Code's: `wire.jsonl` emits a `StatusUpdate`
//! per LLM step carrying an authoritative `context_usage` and a full
//! `token_usage` breakdown, and `state.json` carries the session title — so no
//! external summarizer is needed. kimi exposes no rate-limit telemetry, so it
//! contributes nothing to the quota panel (like OpenCode).

use super::process;
use crate::model::{
    AgentSession, ChatMessage, ChatRole, ChildProcess, FileAccess, FileOp, SessionStatus,
    ToolCall, MAX_CHAT_MESSAGES, MAX_FILE_ACCESSES,
};
use md5::{Digest, Md5};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum tool-call timeline / chat entries kept per session to bound memory.
const MAX_TOOL_CALLS: usize = 500;

pub struct KimiCollector {
    /// kimi-code config root (default `~/.kimi`).
    config_root: PathBuf,
    /// Per-session-uuid incremental parse cache.
    cache: HashMap<String, KimiCache>,
}

impl KimiCollector {
    pub fn new() -> Self {
        Self {
            config_root: kimi_config_root(),
            cache: HashMap::new(),
        }
    }

    fn sessions_dir(&self) -> PathBuf {
        self.config_root.join("sessions")
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        if !is_kimi_root(&self.config_root) {
            return vec![];
        }

        let self_pid = std::process::id();
        let mut sessions = Vec::new();
        let model_default = read_kimi_model(&self.config_root);

        for pid in Self::find_kimi_pids(&shared.process_info, self_pid) {
            // Welcome-screen state: process alive but no session dir yet
            // (kimi creates the session on the first user message). Skip
            // gracefully — no row until a session exists.
            let Some(cwd) = process_cwd(pid) else {
                continue;
            };
            let hash = md5_hex(&cwd);
            let Some(session_dir) = newest_session_dir(&self.sessions_dir(), &hash) else {
                continue;
            };

            if let Some(session) = self.load_session(
                pid,
                &cwd,
                &session_dir,
                &model_default,
                shared,
            ) {
                sessions.push(session);
            }
        }

        // Drop cache entries for sessions no longer live.
        let live: std::collections::HashSet<&str> =
            sessions.iter().map(|s| s.session_id.as_str()).collect();
        self.cache.retain(|sid, _| live.contains(sid.as_str()));

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    /// Live `kimi-code` processes that are not descendants of abtop itself.
    /// kimi-cli rewrites argv[0] to `kimi-code`, so we match the first token's
    /// basename precisely (avoids matching `grep kimi-code` etc.).
    fn find_kimi_pids(
        process_info: &HashMap<u32, process::ProcInfo>,
        self_pid: u32,
    ) -> Vec<u32> {
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
        pid: u32,
        cwd: &str,
        session_dir: &Path,
        model_default: &str,
        shared: &super::SharedProcessData,
    ) -> Option<AgentSession> {
        let session_id = session_dir
            .file_name()
            .and_then(|n| n.to_str())?
            .to_string();

        let wire_path = session_dir.join("wire.jsonl");
        let ctx_path = session_dir.join("context.jsonl");
        let state_path = session_dir.join("state.json");

        let cache = self.cache.entry(session_id.clone()).or_default();

        // --- wire.jsonl: authoritative context% + tokens + window + plan_mode ---
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
                if delta.saw_status {
                    cache.context_percent = delta.context_percent;
                    cache.context_tokens = delta.context_tokens;
                    cache.max_context_tokens = delta.max_context_tokens;
                    cache.plan_mode = delta.plan_mode;
                    cache.last_activity = delta.last_activity;
                    cache.token_history.extend(delta.token_history);
                    if cache.token_history.len() > 10_000 {
                        let extra = cache.token_history.len() - 10_000;
                        cache.token_history.drain(0..extra);
                    }
                    cache.context_history.extend(delta.context_history);
                    if cache.context_history.len() > 10_000 {
                        let extra = cache.context_history.len() - 10_000;
                        cache.context_history.drain(0..extra);
                    }
                }
            }
        }

        // --- context.jsonl: tool_calls, chat, current_task, file accesses ---
        if ctx_path.exists() {
            let identity = file_identity(&ctx_path);
            let reset = cache.ctx_identity != identity;
            let from = if reset { 0 } else { cache.ctx_offset };
            let delta = parse_context(&ctx_path, from);
            if reset {
                cache.ctx_offset = 0;
                cache.ctx_identity = identity;
                cache.turn_count = 0;
                cache.current_task.clear();
                cache.tool_calls.clear();
                cache.chat_messages.clear();
                cache.file_accesses.clear();
                cache.initial_prompt.clear();
                cache.first_assistant_text.clear();
            }
            cache.ctx_offset = delta.new_offset;
            cache.ctx_identity = identity;
            cache.turn_count += delta.turn_count;
            if !delta.current_task.is_empty() {
                cache.current_task = delta.current_task;
            } else if delta.turn_count > 0 {
                // Latest assistant turn had no tool_use → clear stale task.
                cache.current_task.clear();
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

        // --- state.json: title + plan_mode override ---
        // state.json `custom_title` is the real session title (kimi generates
        // it itself), so prefer it over the first-user-prompt fallback for the
        // displayed session title — no external summarizer needed.
        let title = read_state_title(&state_path).unwrap_or_default();
        if let Some(pm) = read_state_plan_mode(&state_path) {
            cache.plan_mode = pm;
        }

        let proc = shared.process_info.get(&pid);
        let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

        // Status: mtime freshness of wire.jsonl + pending tool + descendant CPU.
        let pending_tool = !cache.current_task.is_empty();
        let has_active_descendant =
            process::has_active_descendant(pid, &shared.children_map, &shared.process_info, 5.0);
        let now = SystemTime::now();
        let fresh = cache
            .last_activity
            .map(|t| now.duration_since(t).map(|d| d.as_secs() < 30).unwrap_or(false))
            .unwrap_or(false);
        let status = if has_active_descendant || pending_tool {
            SessionStatus::Executing
        } else if fresh {
            SessionStatus::Thinking
        } else {
            SessionStatus::Waiting
        };

        let project_name = process::last_path_segment(cwd)
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
        let mut stack: Vec<u32> = shared
            .children_map
            .get(&pid)
            .cloned()
            .unwrap_or_default();
        let mut visited = std::collections::HashSet::new();
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

        let started_at = wire_mtime_ms(&wire_path).unwrap_or(0);

        Some(AgentSession {
            agent_cli: "kimi",
            pid,
            session_id,
            cwd: cwd.to_string(),
            project_name,
            started_at,
            status,
            model: model_default.to_string(),
            effort: String::new(),
            context_percent: cache.context_percent,
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
            context_window: cache.max_context_tokens,
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
// Incremental parse cache
// ---------------------------------------------------------------------------

#[derive(Default)]
struct KimiCache {
    wire_offset: u64,
    wire_identity: (u64, u64),
    ctx_offset: u64,
    ctx_identity: (u64, u64),
    // telemetry (cumulative / latest from wire.jsonl StatusUpdate)
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_create: u64,
    context_percent: f64,
    context_tokens: u64,
    max_context_tokens: u64,
    plan_mode: bool,
    last_activity: Option<SystemTime>,
    token_history: Vec<u64>,
    context_history: Vec<u64>,
    // structural (from context.jsonl)
    turn_count: u32,
    current_task: String,
    tool_calls: Vec<ToolCall>,
    chat_messages: Vec<ChatMessage>,
    file_accesses: Vec<FileAccess>,
    initial_prompt: String,
    first_assistant_text: String,
}

struct WireDelta {
    new_offset: u64,
    saw_status: bool,
    context_percent: f64,
    context_tokens: u64,
    max_context_tokens: u64,
    plan_mode: bool,
    last_activity: Option<SystemTime>,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
    token_history: Vec<u64>,
    context_history: Vec<u64>,
}

struct ContextDelta {
    new_offset: u64,
    turn_count: u32,
    current_task: String,
    tool_calls: Vec<ToolCall>,
    chat_messages: Vec<ChatMessage>,
    file_accesses: Vec<FileAccess>,
    initial_prompt: String,
    first_assistant_text: String,
}

// ---------------------------------------------------------------------------
// wire.jsonl parsing (StatusUpdate → tokens / context / window / plan_mode)
// ---------------------------------------------------------------------------

fn parse_wire(path: &Path, from_offset: u64) -> WireDelta {
    let mut delta = WireDelta {
        new_offset: from_offset,
        saw_status: false,
        context_percent: 0.0,
        context_tokens: 0,
        max_context_tokens: 0,
        plan_mode: false,
        last_activity: None,
        input: 0,
        output: 0,
        cache_read: 0,
        cache_create: 0,
        token_history: Vec::new(),
        context_history: Vec::new(),
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

    let mtime = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok());
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
                    let msg_type = val
                        .get("message")
                        .and_then(|m| m.get("type"))
                        .and_then(|t| t.as_str());
                    if msg_type == Some("StatusUpdate") {
                        let payload = val.get("message").and_then(|m| m.get("payload"));
                        if let Some(p) = payload {
                            delta.saw_status = true;
                            if let Some(u) = p.get("context_usage").and_then(|v| v.as_f64()) {
                                delta.context_percent = u * 100.0;
                            }
                            delta.context_tokens =
                                p.get("context_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                            delta.max_context_tokens =
                                p.get("max_context_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                            delta.plan_mode =
                                p.get("plan_mode").and_then(|v| v.as_bool()).unwrap_or(false);
                            if let Some(tu) = p.get("token_usage") {
                                let inp = tu
                                    .get("input_other")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let out = tu.get("output").and_then(|v| v.as_u64()).unwrap_or(0);
                                let cr = tu
                                    .get("input_cache_read")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let cc = tu
                                    .get("input_cache_creation")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                delta.input += inp;
                                delta.output += out;
                                delta.cache_read += cr;
                                delta.cache_create += cc;
                                if delta.token_history.len() < 10_000 {
                                    delta.token_history.push(inp + out + cr + cc);
                                }
                            }
                            if delta.context_history.len() < 10_000 {
                                delta.context_history.push(delta.context_tokens);
                            }
                        }
                    }
                    if let Some(ts) = val.get("timestamp").and_then(|v| v.as_f64()) {
                        // timestamp is epoch seconds (float). Convert to SystemTime.
                        let secs = ts.trunc();
                        let nanos = ((ts - secs).abs() * 1_000_000_000.0) as u32;
                        if secs >= 0.0 {
                            delta.last_activity = SystemTime::UNIX_EPOCH
                                .checked_add(std::time::Duration::new(secs as u64, nanos));
                        }
                    }
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
    // A file mtime is a reliable fallback if no StatusUpdate timestamp was seen.
    if delta.last_activity.is_none() {
        delta.last_activity = mtime;
    }
    delta
}

// ---------------------------------------------------------------------------
// context.jsonl parsing (tool_calls / chat / current_task / file accesses)
// ---------------------------------------------------------------------------

fn parse_context(path: &Path, from_offset: u64) -> ContextDelta {
    let mut delta = ContextDelta {
        new_offset: from_offset,
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
                    let role = val.get("role").and_then(|r| r.as_str()).unwrap_or("");
                    match role {
                        "assistant" => {
                            delta.turn_count += 1;
                            delta.current_task.clear();
                            let assistant_text = extract_text(&val);
                            if delta.first_assistant_text.is_empty() && !assistant_text.is_empty() {
                                delta.first_assistant_text = truncate(&assistant_text, 200);
                            }
                            if !assistant_text.is_empty() {
                                push_chat(
                                    &mut delta.chat_messages,
                                    ChatRole::Assistant,
                                    assistant_text,
                                );
                            }
                            if let Some(calls) =
                                val.get("tool_calls").and_then(|c| c.as_array())
                            {
                                for call in calls {
                                    let function =
                                        call.get("function").or_else(|| call.get("function"));
                                    let Some(function) = function else { continue };
                                    let name = function
                                        .get("name")
                                        .and_then(|n| n.as_str())
                                        .unwrap_or("?");
                                    let args_str = function
                                        .get("arguments")
                                        .and_then(|a| a.as_str())
                                        .unwrap_or("");
                                    let (arg, file_path) = parse_tool_args(name, args_str);
                                    delta.current_task =
                                        format!("{} {}", name, truncate(&arg, 40));
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
                            }
                        }
                        "user" => {
                            let user_text = extract_text(&val);
                            if delta.initial_prompt.is_empty() && !user_text.is_empty() {
                                delta.initial_prompt = truncate(&clean_prompt(&user_text), 50);
                            }
                            if !user_text.is_empty() {
                                push_chat(&mut delta.chat_messages, ChatRole::User, user_text);
                            }
                        }
                        _ => {}
                    }
                }
                if !has_newline {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    delta.new_offset = bytes_read;
    delta
}

/// Extract concatenated text content from a context.jsonl message.
/// `content` may be a string or an array of `{type,text}` / `{type,think}` blocks.
fn extract_text(msg: &Value) -> String {
    let raw = match msg.get("content") {
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

/// Parse a tool's arguments JSON string into a short display arg and an
/// optional file path (for the file-access audit).
fn parse_tool_args(name: &str, args_json: &str) -> (String, Option<String>) {
    let val: Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(_) => return (String::new(), None),
    };
    let path = val.get("path").and_then(|p| p.as_str()).map(String::from);
    match name {
        "Shell" => {
            let cmd = val.get("command").and_then(|c| c.as_str()).unwrap_or("");
            let first = cmd.lines().next().unwrap_or(cmd);
            (
                super::redact_secrets(&truncate(first, 40)),
                None,
            )
        }
        "WriteFile" | "StrReplaceFile" | "ReadFile" | "ReadMediaFile" => {
            let p = path.clone().unwrap_or_default();
            (shorten_path(&p), path)
        }
        _ => {
            if let Some(s) = val.get("command").and_then(|c| c.as_str()) {
                (truncate(s.lines().next().unwrap_or(s), 40), None)
            } else if let Some(p) = path.as_deref() {
                (shorten_path(p), path.clone())
            } else {
                // Fall back to the first string value in the object.
                let first = val
                    .as_object()
                    .and_then(|o| {
                        o.values().find_map(|v| v.as_str()).map(|s| s.to_string())
                    })
                    .unwrap_or_default();
                (truncate(&first, 40), None)
            }
        }
    }
}

fn file_op_for(tool: &str) -> Option<FileOp> {
    match tool {
        "ReadFile" | "ReadMediaFile" => Some(FileOp::Read),
        "WriteFile" => Some(FileOp::Write),
        "StrReplaceFile" => Some(FileOp::Edit),
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

// ---------------------------------------------------------------------------
// state.json (title + plan_mode) and config.toml (model)
// ---------------------------------------------------------------------------

fn read_state_title(state_path: &Path) -> Option<String> {
    let val: Value = serde_json::from_str(&fs::read_to_string(state_path).ok()?).ok()?;
    let title = val
        .get("custom_title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim();
    if title.is_empty() {
        None
    } else {
        Some(super::redact_secrets(&truncate(title, 80)))
    }
}

fn read_state_plan_mode(state_path: &Path) -> Option<bool> {
    let val: Value = serde_json::from_str(&fs::read_to_string(state_path).ok()?).ok()?;
    val.get("plan_mode").and_then(|p| p.as_bool())
}

/// Read the configured model's display name from `~/.kimi/config.toml`.
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

/// Default kimi-code config root: `$KIMI_CONFIG_DIR` if set, else `~/.kimi`.
fn kimi_config_root() -> PathBuf {
    if let Ok(dir) = std::env::var("KIMI_CONFIG_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return p;
        }
    }
    dirs::home_dir().unwrap_or_default().join(".kimi")
}

fn is_kimi_root(path: &Path) -> bool {
    path.is_dir() && path.join("sessions").is_dir()
}

fn md5_hex(s: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(s.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Newest session-uuid subdirectory under `sessions/<hash>/` by modification time.
fn newest_session_dir(sessions_root: &Path, hash: &str) -> Option<PathBuf> {
    let dir = sessions_root.join(hash);
    let entries = fs::read_dir(&dir).ok()?;
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        if entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(true) {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(UNIX_EPOCH);
        match &best {
            Some((b, _)) if &mtime <= b => {}
            _ => best = Some((mtime, path)),
        }
    }
    best.map(|(_, p)| p)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_hashing_matches_kimi_session_layout() {
        // Verified against this machine: kimi names session dirs md5(cwd).
        assert_eq!(
            md5_hex("/home/huangwei/workspace/playground/Auto-claude-code-research-in-sleep"),
            "7f406012a8bda4d83cbe521fb8598eec"
        );
        assert_eq!(
            md5_hex("/tmp"),
            "d42b9c57d24cf5db3bd8d332dc35437f"
        );
    }

    #[test]
    fn wire_parses_status_update_into_tokens_and_context() {
        let tmp = tempfile::tempdir().unwrap();
        let wire = tmp.path().join("wire.jsonl");
        std::fs::write(
            &wire,
            r#"{"type":"metadata","protocol_version":"1.10"}
{"timestamp":1780132708.65,"message":{"type":"StatusUpdate","payload":{"context_usage":0.5,"context_tokens":131072,"max_context_tokens":262144,"token_usage":{"input_other":100,"output":10,"input_cache_read":200,"input_cache_creation":5},"message_id":"m1","plan_mode":false,"mcp_status":null}}}
{"timestamp":1780132709.0,"message":{"type":"StatusUpdate","payload":{"context_usage":0.25,"context_tokens":65536,"max_context_tokens":262144,"token_usage":{"input_other":50,"output":5,"input_cache_read":0,"input_cache_creation":0},"message_id":"m2","plan_mode":true,"mcp_status":null}}}
"#,
        )
        .unwrap();

        let delta = parse_wire(&wire, 0);
        assert!(delta.saw_status);
        // latest wins for context/window/plan_mode
        assert_eq!(delta.context_percent, 25.0);
        assert_eq!(delta.context_tokens, 65536);
        assert_eq!(delta.max_context_tokens, 262144);
        assert!(delta.plan_mode);
        // cumulative token sums
        assert_eq!(delta.input, 150);
        assert_eq!(delta.output, 15);
        assert_eq!(delta.cache_read, 200);
        assert_eq!(delta.cache_create, 5);
        assert!(delta.last_activity.is_some());
    }

    #[test]
    fn wire_incremental_offset_only_parses_new_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let wire = tmp.path().join("wire.jsonl");
        let first =
            r#"{"timestamp":1780132708.0,"message":{"type":"StatusUpdate","payload":{"context_usage":0.1,"context_tokens":100,"max_context_tokens":262144,"token_usage":{"input_other":10,"output":1,"input_cache_read":0,"input_cache_creation":0}}}}
"#;
        std::fs::write(&wire, first).unwrap();
        let off = std::fs::metadata(&wire).unwrap().len();

        let d1 = parse_wire(&wire, 0);
        assert_eq!(d1.input, 10);
        assert_eq!(d1.new_offset, off);

        // append a second status; incremental parse must only see the new one.
        use std::io::Write;
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&wire)
                .unwrap();
            f.write_all(
                br#"{"timestamp":1780132709.0,"message":{"type":"StatusUpdate","payload":{"context_usage":0.2,"context_tokens":200,"max_context_tokens":262144,"token_usage":{"input_other":20,"output":2,"input_cache_read":0,"input_cache_creation":0}}}}
"#,
            )
            .unwrap();
        }

        let d2 = parse_wire(&wire, off);
        assert_eq!(d2.input, 20); // only the appended line
        assert_eq!(d2.context_tokens, 200);
    }

    #[test]
    fn context_parses_assistant_tool_calls_and_user_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = tmp.path().join("context.jsonl");
        std::fs::write(
            &ctx,
            r#"{"role":"_system_prompt","content":"system"}
{"role":"user","content":"organize a git commit"}
{"role":"assistant","content":[{"type":"text","text":"done"}]}
{"role":"assistant","content":[{"type":"think","think":"planning"}],"tool_calls":[{"type":"function","id":"t1","function":{"name":"Shell","arguments":"{\"command\":\"cd /x && git status\"}"}}]}
"#,
        )
        .unwrap();

        let d = parse_context(&ctx, 0);
        assert_eq!(d.turn_count, 2);
        assert_eq!(d.current_task, "Shell cd /x && git status");
        assert_eq!(d.tool_calls.len(), 1);
        assert_eq!(d.tool_calls[0].name, "Shell");
        assert_eq!(d.initial_prompt, "organize a git commit");
        assert!(!d.first_assistant_text.is_empty());
        // user + 2 assistant text turns → at least 2 chat messages
        assert!(d.chat_messages.len() >= 2);
    }

    #[test]
    fn context_file_accesses_captured_for_file_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = tmp.path().join("context.jsonl");
        std::fs::write(
            &ctx,
            r#"{"role":"assistant","content":[],"tool_calls":[{"type":"function","id":"t1","function":{"name":"WriteFile","arguments":"{\"path\":\"/repo/src/main.rs\"}"}}]}
"#,
        )
        .unwrap();
        let d = parse_context(&ctx, 0);
        assert_eq!(d.file_accesses.len(), 1);
        assert_eq!(d.file_accesses[0].path, "/repo/src/main.rs");
        assert_eq!(d.file_accesses[0].operation, FileOp::Write);
    }

    #[test]
    fn state_title_and_plan_mode_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state.json");
        std::fs::write(
            &state,
            r#"{"custom_title":"组织一波 git 提交","plan_mode":false,"todos":[]}"#,
        )
        .unwrap();
        assert_eq!(read_state_title(&state).as_deref(), Some("组织一波 git 提交"));
        assert_eq!(read_state_plan_mode(&state), Some(false));
    }

    #[test]
    fn config_model_display_name_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"default_model = "kimi-code/kimi-for-coding"
[models."kimi-code/kimi-for-coding"]
display_name = "Kimi-k2.6"
"#,
        )
        .unwrap();
        assert_eq!(read_kimi_model(tmp.path()), "Kimi-k2.6");
    }

    #[test]
    fn config_model_falls_back_to_id_without_display_name() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"default_model = "kimi-code/kimi-for-coding"
[models."kimi-code/kimi-for-coding"]
max_context_size = 262144
"#,
        )
        .unwrap();
        assert_eq!(read_kimi_model(tmp.path()), "kimi-code/kimi-for-coding");
    }

    #[test]
    fn welcome_screen_no_session_dir_is_skipped() {
        // sessions/<hash>/ absent → newest_session_dir returns None → caller skips.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        assert!(newest_session_dir(&root, "deadbeefdeadbeefdeadbeefdeadbeef").is_none());
    }
}
