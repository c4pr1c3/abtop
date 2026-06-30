use super::process;
use crate::model::{AgentSession, ChildProcess, SessionStatus};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Maximum sessions to fetch from the DB per query.
const MAX_SESSIONS: u32 = 20;

/// When no live hermes PID can be paired to an `ended_at IS NULL` session, we
/// still surface it as `Unknown` if it was active within this window — hedges
/// against PID-matcher gaps (e.g. an unfamiliar uv/python wrapper) while
/// bounding stale/crashed sessions whose `ended_at` never got set. Sessions
/// older than this without a live process are treated as ghosts and dropped.
const NO_PID_RECENCY_WINDOW_SECS: u64 = 30 * 60;

/// Collector for Hermes Agent sessions (NousResearch/hermes-agent).
///
/// On-disk layout (per profile, default `~/.hermes`, overridable via
/// `$HERMES_HOME`; native Windows uses `%LOCALAPPDATA%\hermes`):
///
/// ```text
/// ~/.hermes/
/// ├── state.db            — SQLite (WAL): sessions + messages + FTS5
/// ├── state.db-wal/-shm   — WAL sidecars (live while hermes runs)
/// ├── config.yaml         — provider/model/toolset config
/// └── logs/agent.log      — activity log (per-session filterable)
/// ```
///
/// Discovery strategy:
/// 1. Read `state.db` via `sqlite3 -readonly -json` (WAL-safe concurrent read).
///    Sessions are rows in the `sessions` table; `ended_at IS NULL` marks an
///    open session. We only show `source = 'cli'` (interactive agent) sessions,
///    matching `hermes sessions list` semantics and excluding gateway/messaging
///    and `--source tool` integration sessions.
/// 2. DB rows are cached and refreshed only on `shared.slow_tick` (~10s) so we
///    don't fork sqlite3 every tick. Token counts, model, title, started_at,
///    message/tool counts come straight from the row — hermes stores these as
///    first-class columns (no transcript parsing needed).
/// 3. `ps` finds live hermes agent processes. Because the `sessions` table has
///    no `cwd`/`directory` column, we pair PIDs to sessions by **recency**
///    (most-recently-active session ↔ the largest hermes process), not by cwd.
///    The paired PID contributes cwd, memory, status, children, and ports.
///    Sessions without a paired PID are shown as `Unknown` only while recent
///    (see `NO_PID_RECENCY_WINDOW_SECS`); older unmatched rows are dropped as
///    likely ghosts.
///
/// Known limitations (v1):
/// - Only the default profile (`$HERMES_HOME` / `~/.hermes`) is monitored.
///   Multi-profile discovery from live PIDs' `HERMES_HOME` environ is a future
///   enhancement (mirror Claude's `refresh_config_dirs`).
/// - No context-window % (hermes is multi-provider; no single window).
/// - No rate-limit telemetry (BYO provider → no managed quota).
/// - No chat/tool-call/file-access enrichment yet (data lives in `messages`).
pub struct HermesCollector {
    home_dir: PathBuf,
    db_path: PathBuf,
    /// Whether the `sqlite3` CLI is available (checked once).
    sqlite3_available: Option<bool>,
    /// Cached DB rows from the last slow-tick query. Reused on fast ticks.
    cached_db_sessions: Vec<DbSession>,
}

impl HermesCollector {
    pub fn new() -> Self {
        let home_dir = hermes_home();
        let db_path = home_dir.join("state.db");
        Self {
            home_dir,
            db_path,
            sqlite3_available: None,
            cached_db_sessions: Vec::new(),
        }
    }

    fn check_sqlite3(&mut self) -> bool {
        if let Some(available) = self.sqlite3_available {
            return available;
        }
        let available = Command::new("sqlite3").arg("--version").output().is_ok();
        self.sqlite3_available = Some(available);
        available
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        // Security: fail-closed on symlink / missing db / no sqlite3.
        if is_symlink(&self.db_path) || !self.db_path.exists() || !self.check_sqlite3() {
            self.cached_db_sessions.clear();
            return vec![];
        }

        let hermes_pids = Self::find_hermes_pids(&shared.process_info);

        // Refresh DB rows on slow ticks only; reuse cache on fast ticks so we
        // don't fork sqlite3 every 2s.
        if shared.slow_tick {
            if let Some(rows) = self.query_sessions() {
                self.cached_db_sessions = rows;
            }
        }

        let now_ms = current_time_ms();

        // Pair PIDs to sessions greedily by recency. Order PIDs by memory
        // descending so the primary agent process (not a small worker/
        // subagent) is paired first; subagents then surface as children of
        // that session via the children walk below.
        let mut pids_by_mem = hermes_pids;
        pids_by_mem.sort_by_key(|&p| {
            std::cmp::Reverse(shared.process_info.get(&p).map(|i| i.rss_kb).unwrap_or(0))
        });
        let mut claimed: HashSet<u32> = HashSet::new();

        let mut sessions = Vec::new();
        for ds in &self.cached_db_sessions {
            let paired_pid = pids_by_mem.iter().find(|p| !claimed.contains(p)).copied();
            let session = match paired_pid {
                Some(pid) => {
                    claimed.insert(pid);
                    self.build_session(ds, pid, shared, now_ms)
                }
                None => {
                    // No live PID. Hedge against matcher gaps by surfacing a
                    // recent open session as Unknown; drop stale ones as ghosts.
                    let age_secs = now_ms.saturating_sub(ds.last_active_ms) / 1000;
                    if age_secs < NO_PID_RECENCY_WINDOW_SECS {
                        self.build_unknown_session(ds)
                    } else {
                        continue;
                    }
                }
            };
            sessions.push(session);
        }

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    /// Live hermes agent processes. The installer ships a `hermes` shim (and
    /// uv/python wrappers may show `python -m hermes`), so accept either a
    /// first-token basename match or any command line referencing `hermes`.
    /// Excludes the long-lived messaging gateway daemon, grep/search noise,
    /// and abtop itself.
    fn find_hermes_pids(process_info: &HashMap<u32, process::ProcInfo>) -> Vec<u32> {
        process_info
            .iter()
            .filter(|(_, info)| {
                let cmd = &info.command;
                let is_hermes = process::cmd_first_token_has_binary(cmd, "hermes")
                    || cmd.split_whitespace().any(|t| t == "hermes")
                    || cmd.contains("/hermes");
                is_hermes
                    && !cmd.contains("gateway")
                    && !cmd.contains("grep")
                    && !cmd.contains("abtop")
            })
            .map(|(pid, _)| *pid)
            .collect()
    }

    /// Build a fully enriched session from a DB row plus a paired live PID.
    fn build_session(
        &self,
        ds: &DbSession,
        pid: u32,
        shared: &super::SharedProcessData,
        now_ms: u64,
    ) -> AgentSession {
        let proc = shared.process_info.get(&pid);
        let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);
        let cwd = get_process_cwd(pid).unwrap_or_default();

        let since_update_secs = now_ms.saturating_sub(ds.last_active_ms) / 1000;
        let status = if since_update_secs < 30 {
            SessionStatus::Thinking
        } else {
            let cpu_active = proc.is_some_and(|p| p.cpu_pct > 1.0);
            let has_active_child =
                process::has_active_descendant(pid, &shared.children_map, &shared.process_info, 5.0);
            if cpu_active || has_active_child {
                SessionStatus::Executing
            } else {
                SessionStatus::Waiting
            }
        };

        let project_name = if !cwd.is_empty() {
            cwd.rsplit('/').next().unwrap_or("?").to_string()
        } else if !ds.title.is_empty() {
            ds.title.clone()
        } else {
            "?".to_string()
        };

        let current_tasks = match status {
            SessionStatus::Waiting => vec!["waiting for input".to_string()],
            SessionStatus::Executing => vec!["running tool…".to_string()],
            _ => vec!["thinking…".to_string()],
        };

        // Collect child processes with a cycle guard (visited set).
        let mut children = Vec::new();
        let mut stack: Vec<u32> = shared
            .children_map
            .get(&pid)
            .cloned()
            .unwrap_or_default();
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

        let model = if ds.model.is_empty() {
            "-".to_string()
        } else {
            ds.model.clone()
        };
        let initial_prompt = if !ds.title.is_empty() {
            ds.title.clone()
        } else {
            ds.preview.clone()
        };

        AgentSession {
            agent_cli: "hermes",
            pid,
            session_id: ds.id.clone(),
            cwd,
            project_name,
            started_at: ds.started_at_ms,
            status,
            model,
            effort: String::new(),
            context_percent: 0.0,
            total_input_tokens: ds.input_tokens,
            total_output_tokens: ds.output_tokens,
            total_cache_read: ds.cache_read,
            total_cache_create: ds.cache_write,
            turn_count: ds.message_count as u32,
            current_tasks,
            mem_mb,
            version: String::new(),
            git_branch: String::new(),
            git_added: 0,
            git_modified: 0,
            token_history: vec![],
            context_history: vec![],
            compaction_count: 0,
            context_window: 0,
            subagents: vec![],
            mem_file_count: 0,
            mem_line_count: 0,
            children,
            initial_prompt,
            first_assistant_text: String::new(),
            chat_messages: vec![],
            tool_calls: vec![],
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: vec![],
            config_root: super::abbrev_path(&self.home_dir),
        }
    }

    /// Build a degraded session for an open DB row with no paired live PID.
    /// Shown as `Unknown` (process ownership not confirmed) — visible so a
    /// matcher gap doesn't hide a real session, but flagged as unconfirmed.
    fn build_unknown_session(&self, ds: &DbSession) -> AgentSession {
        let initial_prompt = if !ds.title.is_empty() {
            ds.title.clone()
        } else {
            ds.preview.clone()
        };
        let model = if ds.model.is_empty() {
            "-".to_string()
        } else {
            ds.model.clone()
        };
        AgentSession {
            agent_cli: "hermes",
            pid: 0,
            session_id: ds.id.clone(),
            cwd: String::new(),
            project_name: if !ds.title.is_empty() {
                ds.title.clone()
            } else {
                "hermes".to_string()
            },
            started_at: ds.started_at_ms,
            status: SessionStatus::Unknown,
            model,
            effort: String::new(),
            context_percent: 0.0,
            total_input_tokens: ds.input_tokens,
            total_output_tokens: ds.output_tokens,
            total_cache_read: ds.cache_read,
            total_cache_create: ds.cache_write,
            turn_count: ds.message_count as u32,
            current_tasks: vec![],
            mem_mb: 0,
            version: String::new(),
            git_branch: String::new(),
            git_added: 0,
            git_modified: 0,
            token_history: vec![],
            context_history: vec![],
            compaction_count: 0,
            context_window: 0,
            subagents: vec![],
            mem_file_count: 0,
            mem_line_count: 0,
            children: vec![],
            initial_prompt,
            first_assistant_text: String::new(),
            chat_messages: vec![],
            tool_calls: vec![],
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: vec![],
            config_root: super::abbrev_path(&self.home_dir),
        }
    }

    /// Run a single sqlite3 query and parse the JSON output.
    fn run_query(&self, sql: &str) -> Option<Vec<Value>> {
        let db = self.db_path.to_str()?;
        let output = Command::new("sqlite3")
            .args(["-readonly", "-json", db])
            .arg(sql)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Some(vec![]);
        }
        serde_json::from_str(stdout.trim()).ok()
    }

    /// Query open CLI sessions with token telemetry, a recency timestamp, and a
    /// first-user-message preview. hermes stores model/tokens/title as
    /// first-class columns on `sessions`, so a single query suffices.
    fn query_sessions(&self) -> Option<Vec<DbSession>> {
        let sql = format!(
            r#"
SELECT
  s.id,
  COALESCE(s.title, '')              AS title,
  COALESCE(s.model, '')             AS model,
  COALESCE(s.source, '')            AS source,
  s.started_at                      AS started_at,
  COALESCE(s.message_count, 0)      AS message_count,
  COALESCE(s.tool_call_count, 0)    AS tool_call_count,
  COALESCE(s.input_tokens, 0)       AS input_tokens,
  COALESCE(s.output_tokens, 0)      AS output_tokens,
  COALESCE(s.cache_read_tokens, 0)  AS cache_read_tokens,
  COALESCE(s.cache_write_tokens, 0) AS cache_write_tokens,
  COALESCE(s.reasoning_tokens, 0)   AS reasoning_tokens,
  COALESCE(
    (SELECT MAX(m.timestamp) FROM messages m WHERE m.session_id = s.id),
    s.started_at
  )                                 AS last_active,
  COALESCE(
    (SELECT SUBSTR(m.content, 1, 200)
       FROM messages m
      WHERE m.session_id = s.id
        AND m.role = 'user'
        AND m.content IS NOT NULL
      ORDER BY m.timestamp, m.id LIMIT 1),
    ''
  )                                 AS preview
FROM sessions s
WHERE s.ended_at IS NULL
  AND s.source = 'cli'
ORDER BY last_active DESC
LIMIT {};"#,
            MAX_SESSIONS
        );

        let rows = self.run_query(&sql)?;

        let mut sessions = Vec::new();
        for row in rows {
            // started_at / last_active are Unix epoch floats (seconds). Convert
            // to epoch milliseconds for AgentSession.started_at.
            let started_secs = row["started_at"].as_f64().unwrap_or(0.0);
            let started_at_ms = (started_secs * 1000.0) as u64;
            let last_active_secs = row["last_active"].as_f64().unwrap_or(started_secs);
            let last_active_ms = (last_active_secs * 1000.0) as u64;

            let mut title = row["title"].as_str().unwrap_or("").to_string();
            let mut preview = row["preview"].as_str().unwrap_or("").to_string();
            let model = row["model"].as_str().unwrap_or("").to_string();
            truncate_field(&mut title, 512);
            truncate_field(&mut preview, 200);
            let title = super::redact_secrets(&title);
            let preview = super::redact_secrets(&preview);

            sessions.push(DbSession {
                id: row["id"].as_str().unwrap_or("").to_string(),
                title,
                model,
                source: row["source"].as_str().unwrap_or("").to_string(),
                started_at_ms,
                last_active_ms,
                message_count: row["message_count"].as_u64().unwrap_or(0),
                tool_call_count: row["tool_call_count"].as_u64().unwrap_or(0),
                input_tokens: row["input_tokens"].as_u64().unwrap_or(0),
                output_tokens: row["output_tokens"].as_u64().unwrap_or(0),
                cache_read: row["cache_read_tokens"].as_u64().unwrap_or(0),
                cache_write: row["cache_write_tokens"].as_u64().unwrap_or(0),
                reasoning_tokens: row["reasoning_tokens"].as_u64().unwrap_or(0),
                preview,
            });
        }

        Some(sessions)
    }
}

impl Default for HermesCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl super::AgentCollector for HermesCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

/// A session row materialized from `~/.hermes/state.db`.
struct DbSession {
    id: String,
    title: String,
    model: String,
    #[allow(dead_code)]
    source: String,
    /// Unix epoch milliseconds (converted from the DB's float seconds).
    started_at_ms: u64,
    /// Epoch ms of the most recent message, or `started_at_ms` if none.
    last_active_ms: u64,
    message_count: u64,
    #[allow(dead_code)]
    tool_call_count: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_write: u64,
    #[allow(dead_code)]
    reasoning_tokens: u64,
    /// First user message (truncated), used as a title fallback.
    preview: String,
}

/// Resolve the hermes home directory: `$HERMES_HOME` if set, else the native
/// per-OS default (`~/.hermes` on Unix, `%LOCALAPPDATA%\hermes` on Windows).
fn hermes_home() -> PathBuf {
    if let Ok(dir) = std::env::var("HERMES_HOME") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return p;
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
            let p = PathBuf::from(local_appdata).join("hermes");
            if p.is_dir() {
                return p;
            }
        }
    }
    dirs::home_dir().unwrap_or_default().join(".hermes")
}

/// Check if a path is a symlink (fail-closed: returns true on error).
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true)
}

/// Truncate a string at a char boundary to avoid panics on multi-byte UTF-8.
fn truncate_field(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// Get the current working directory of a process.
/// Uses /proc on Linux, lsof on macOS/other Unix.
#[cfg(target_os = "linux")]
fn get_process_cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{}/cwd", pid))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "linux"))]
fn get_process_cwd(pid: u32) -> Option<String> {
    // -a ANDs the selection terms; without it, lsof ORs `-p <pid>` with
    // `-d cwd` and returns cwd entries for unrelated processes too.
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

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_hermes_pids_matches_shim_and_wrapper_excludes_gateway() {
        let mk = |pid: u32, cmd: &str| process::ProcInfo {
            pid, ppid: 1, rss_kb: 1000, cpu_pct: 0.0,
            command: cmd.to_string(), start_ticks: 0,
        };
        let mut info = HashMap::new();
        info.insert(100, mk(100, "/home/u/.local/bin/hermes"));      // shim → match
        info.insert(200, mk(200, "python -m hermes"));               // wrapper → match
        info.insert(300, mk(300, "hermes gateway run"));             // gateway → exclude
        info.insert(400, mk(400, "grep hermes"));                    // grep → exclude
        info.insert(500, mk(500, "/usr/bin/opencode"));              // unrelated → exclude

        let pids = HermesCollector::find_hermes_pids(&info);
        assert!(pids.contains(&100));
        assert!(pids.contains(&200));
        assert!(!pids.contains(&300), "gateway daemon must be excluded");
        assert!(!pids.contains(&400), "grep must be excluded");
        assert!(!pids.contains(&500));
        assert_eq!(pids.len(), 2);
    }

    #[test]
    fn hermes_home_defaults_to_dotdir() {
        // No HERMES_HOME set in test env → falls back to ~/.hermes.
        std::env::remove_var("HERMES_HOME");
        let home = hermes_home();
        assert!(home.to_string_lossy().ends_with(".hermes"));
    }

    #[test]
    fn db_path_under_home() {
        std::env::remove_var("HERMES_HOME");
        let c = HermesCollector::new();
        let s = c.db_path.to_string_lossy();
        assert!(s.ends_with(".hermes/state.db"));
    }

    /// Build a fake state.db with the real hermes schema, seed two sessions
    /// (one open cli, one ended cli, one open telegram), and verify the query
    /// returns only the open cli session with correct telemetry.
    fn seed_state_db(dir: &Path) -> PathBuf {
        let db = dir.join("state.db");
        let schema = r#"
CREATE TABLE sessions (
    id TEXT PRIMARY KEY, source TEXT NOT NULL, user_id TEXT, model TEXT,
    model_config TEXT, system_prompt TEXT, parent_session_id TEXT,
    started_at REAL NOT NULL, ended_at REAL, end_reason TEXT,
    message_count INTEGER DEFAULT 0, tool_call_count INTEGER DEFAULT 0,
    input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0,
    cache_read_tokens INTEGER DEFAULT 0, cache_write_tokens INTEGER DEFAULT 0,
    reasoning_tokens INTEGER DEFAULT 0, billing_provider TEXT, billing_base_url TEXT,
    billing_mode TEXT, estimated_cost_usd REAL, actual_cost_usd REAL, cost_status TEXT,
    cost_source TEXT, pricing_version TEXT, title TEXT, api_call_count INTEGER DEFAULT 0
);
CREATE TABLE messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL,
    content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT, timestamp REAL NOT NULL,
    token_count INTEGER, finish_reason TEXT, reasoning TEXT, reasoning_content TEXT,
    reasoning_details TEXT, codex_reasoning_items TEXT, codex_message_items TEXT
);
"#;
        let status = Command::new("sqlite3")
            .arg(&db)
            .arg(schema)
            .status()
            .expect("sqlite3 schema create failed");
        assert!(status.success(), "sqlite3 not available or schema failed");

        // Open CLI session, started 1000s ago, last message 10s ago.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let insert = format!(
            r#"INSERT INTO sessions (id,source,model,started_at,ended_at,message_count,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,title) VALUES
('sess_open','cli','anthropic/claude-sonnet-4.6',{started},NULL,5,1000,200,300,40,'Fix Docker build'),
('sess_done','cli','openai/gpt-5',{started},{ended},2,10,5,0,0,NULL),
('sess_tg','telegram','openai/gpt-5',{started},NULL,1,1,1,0,0,NULL);
INSERT INTO messages (session_id,role,content,timestamp) VALUES
('sess_open','user','please fix the dockerfile',{msg_ts});
"#,
            started = now - 1000.0,
            ended = now - 500.0,
            msg_ts = now - 10.0,
        );
        let status = Command::new("sqlite3")
            .arg(&db)
            .arg(&insert)
            .status()
            .expect("sqlite3 insert failed");
        assert!(status.success());
        db
    }

    #[test]
    fn query_returns_only_open_cli_sessions_with_telemetry() {
        // Skip cleanly if sqlite3 isn't installed in the test environment.
        if Command::new("sqlite3").arg("--version").status().is_err() {
            eprintln!("skipping: sqlite3 not available");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = seed_state_db(tmp.path());

        let mut c = HermesCollector::new();
        c.db_path = db;
        let rows = c.query_sessions().expect("query returned None");
        assert_eq!(rows.len(), 1, "only the open cli session should be returned");
        let r = &rows[0];
        assert_eq!(r.id, "sess_open");
        assert_eq!(r.model, "anthropic/claude-sonnet-4.6");
        assert_eq!(r.source, "cli");
        assert_eq!(r.input_tokens, 1000);
        assert_eq!(r.output_tokens, 200);
        assert_eq!(r.cache_read, 300);
        assert_eq!(r.cache_write, 40);
        assert_eq!(r.message_count, 5);
        assert_eq!(r.title, "Fix Docker build");
        assert!(r.preview.contains("dockerfile"));
        // last_active is within the last minute (the seeded message ts).
        let now_ms = current_time_ms();
        assert!(now_ms.saturating_sub(r.last_active_ms) < 60_000);
    }

    #[test]
    fn collect_gates_on_live_pid_and_emits_unknown_fallback() {
        if Command::new("sqlite3").arg("--version").status().is_err() {
            eprintln!("skipping: sqlite3 not available");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = seed_state_db(tmp.path());

        let mut c = HermesCollector::new();
        c.db_path = db.clone();
        c.sqlite3_available = Some(true); // skip the runtime probe

        // No live hermes PID → the recent open session surfaces as Unknown.
        let shared = super::super::SharedProcessData {
            process_info: HashMap::new(),
            children_map: HashMap::new(),
            ports: HashMap::new(),
            slow_tick: true,
            mcp_server_pids: HashSet::new(),
            mcp_owned_rollouts: std::collections::HashSet::new(),
            mcp_suppress: true,
            desktop_rollout_fd_map: HashMap::new(),
        };
        let sessions = c.collect_sessions(&shared);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].agent_cli, "hermes");
        assert_eq!(sessions[0].status, SessionStatus::Unknown);
        assert_eq!(sessions[0].total_input_tokens, 1000);
    }
}
