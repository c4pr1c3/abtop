# abtop

AI agent monitor for your terminal. Like btop++, but for AI coding agents.

Supports Claude Code, Codex CLI, OpenCode, kimi-code, Hermes Agent, and Pi sessions.

**Pi is a first-class supported agent.** It is discovered from `~/.pi/agent/sessions` (any Pi or Pi-derivative coding agent built on `@earendil-works/pi-coding-agent`, e.g. my-pi-agent), contributes session / token / context-window / task / project / port data, and supports the `Enter` session-jump, on equal footing with the other agents (see §7).

**kimi-code is a first-class supported agent and an active focus of development.** It is discovered from `~/.kimi-code`, contributes session / token / context-window / task / project / port data, and is on equal footing with Claude Code, Codex CLI, and OpenCode (see §5).

## Language Policy

English is mandatory for all project-facing work and communication.

- Write all source code, comments, tests, fixtures, documentation, examples, configuration text, scripts, and user-facing strings in English.
- Use English for every GitHub artifact: issue titles and bodies, issue comments, pull request titles and descriptions, review comments, commit messages, branch names, release notes, changelogs, discussions, labels, milestones, and workflow or CI messages.
- Do not use non-English text in repository content or GitHub communication unless it is an exact external identifier, a required protocol value, or a direct quote needed for context.
- When quoting or preserving non-English input, add an English explanation and keep the non-English text as short as possible.
- If a contributor opens an issue, comment, or review in another language, respond in English and continue the thread in English.

## Architecture

```
src/
├── main.rs                 # Entry, terminal setup, event loop, --setup flag
├── app.rs                  # App state, tick logic, key handling, summary generation
├── setup.rs                # StatusLine hook installation (abtop --setup)
├── ui/
│   └── mod.rs              # All panels in single file: header, context, quota,
│                           # tokens, projects, ports, sessions, footer
├── collector/
│   ├── mod.rs              # MultiCollector orchestration, orphan port detection
│   ├── claude.rs           # Claude Code: session discovery, transcript parsing
│   ├── codex.rs            # Codex CLI: session discovery via ps+lsof, JSONL parsing
│   ├── opencode.rs         # OpenCode: session discovery via ps + SQLite DB parsing
│   ├── kimi.rs             # kimi-code: session_index.jsonl discovery + wire.jsonl tailing
│   ├── hermes.rs           # Hermes Agent: ~/.hermes/state.db (SQLite) discovery + ps pairing
│   ├── pi.rs               # Pi: ~/.pi/agent/sessions active/{pid}.json sidecar + JSONL tail
│   ├── process.rs          # Child process tree (ps) + open ports (lsof) + git stats
│   └── rate_limit.rs       # Rate limit file reading (~/.claude/abtop-rate-limits.json)
└── model/
    ├── mod.rs              # Re-exports
    └── session.rs          # AgentSession, SessionStatus, RateLimitInfo,
                            # ChildProcess, OrphanPort, SubAgent
```

## Layout

```
┌─ ¹context (token rate sparkline + per-session context bars) ─────────┐
│  ▁▃▅▇█▇▅▃▁▃▅▇██                       S1 abtop       ████████ 82%  │
│  token rate (200pt history)            S2 prediction  █████████91%⚠ │
│                                        S3 api-server  ███      22%  │
└──────────────────────────────────────────────────────────────────────┘
┌─ ²quota ─────┐┌─ ³tokens ───┐┌─ projects ───┐┌─ ⁴ports ──────────┐
│ CLAUDE       ││ Total  1.2M ││ abtop        ││ PORT  SESSION  CMD │
│ 5h ████ 35%  ││ Input  402k ││  main +3 ~18 ││ :3000 api-srv node│
│   resets 2h  ││ Output  89k ││              ││ :8080 predict crgo│
│ 7d ██ 12%    ││ Cache  710k ││ prediction   ││                    │
│              ││ ▁▃▅▇█▇▅▃▁▃▅││  feat/x +1~2 ││ ORPHAN PORTS       │
│ CODEX        ││ Turns: 48   ││              ││ :4000 old-prj node│
│ 5h █ 9%     ││ Avg: 25k/t  ││ api-server   ││                    │
│ 7d ██ 14%    ││             ││  main ✓clean ││                    │
└──────────────┘└─────────────┘└──────────────┘└────────────────────┘
┌─ ⁵sessions ─────────────────────────────────────────────────────────┐
│ ►*CC 7336 abtop  ● Work opus  82% 1.2M  48  Edit src/pay.rs       │
│  >CD 8840 pred   ◌ Wait sonn  91% 340k  12  waiting                │
│ ─────────────────────────────────────────────────────────────────── │
│  SESSION 7336 · /Users/graykode/abtop                               │
│  Stripe payment integration...                                      │
│  └─ Edit src/pay.rs                                                 │
│  CHILDREN: 7401 cargo build                                         │
│  SUBAGENTS: explore-data ✓12k · run-tests ●8k                      │
│  MEM 4f · 12/200 │ v2.1.86 · 47m                                   │
└──────────────────────────────────────────────────────────────────────┘
```

Panel rendering priority (top to bottom):
1. **Sessions** — always visible, gets priority allocation (min 5 rows, ideal = 2/session + 7)
2. **Mid-tier** (quota, tokens, projects, ports) — split equally, shown if space allows
3. **Context** — only renders when sessions have ideal height AND surplus >= 5 rows
4. **Header** (1 row) + **Footer** (1 row) — always present

Panel descriptions:
- **¹context**: Left = token rate braille sparkline (200-point history). Right = per-session context % bars with yellow/red warning.
- **²quota**: Claude + Codex rate limit gauges side-by-side (5h and 7d windows with reset countdown). Quota is intentionally limited to Claude and Codex; do not add an OpenCode row unless OpenCode exposes a reliable account-level provider rate-limit source.
- **³tokens**: Total token breakdown (in/out/cache) + per-turn sparkline for selected session.
- **projects** (always visible): Per-project git branch + added/modified file counts.
- **⁴ports**: Agent-spawned open ports + orphan ports (from dead sessions). Conflict detection.
- **⁵sessions**: Full-width panel below mid row. Session list table (top) + selected session detail (bottom), separated by divider.

## Data Sources

All read-only from local filesystem + `ps` + `lsof`. No API calls, no auth.

### 1. Claude Code session discovery: process + config-root mapping

Discovery strategy:
1. Find running `claude` processes via `ps`
2. Map PID → open files/directories via `lsof`
3. Infer Claude config roots from open paths that contain `sessions/` and `projects/`
4. Read `{config-root}/sessions/{PID}.json`, falling back to scanning session files for the matching embedded PID
5. Parse `{config-root}/projects/{encoded-path}/{sessionId}.jsonl`

Fallback config roots are still scanned: `~/.claude`, direct home profile roots matching `~/.claude-*` when they contain both `sessions/` and `projects/`, `claude_config_dirs` from `~/.config/abtop/config.toml`, abtop's own `CLAUDE_CONFIG_DIR`, and on Linux any `CLAUDE_CONFIG_DIR` read from `/proc/{pid}/environ`.

Session file format:
```json
{ "pid": 7336, "sessionId": "2f029acc-...", "cwd": "/Users/graykode/abtop", "startedAt": 1774715116826, "kind": "interactive", "entrypoint": "cli" }
```
- ~170 bytes. Created on start, deleted on exit.
- Verify PID alive with shared `ps` data containing a `claude` binary.
- Skip sessions whose PID descends from abtop's own `claude --print` summary children without hiding user-spawned non-interactive sessions.

### 2. Claude Code transcript: `{config-root}/projects/{encoded-path}/{sessionId}.jsonl`
Path encoding: `/Users/foo/bar` → `-Users-foo-bar`

Key line types:

**`assistant`** (tokens, model, tools):
```json
{
  "type": "assistant",
  "timestamp": "2026-03-28T15:25:55.123Z",
  "message": {
    "model": "claude-opus-4-6",
    "stop_reason": "end_turn",
    "usage": {
      "input_tokens": 2,
      "output_tokens": 5,
      "cache_read_input_tokens": 11313,
      "cache_creation_input_tokens": 4350
    },
    "content": [
      { "type": "text", "text": "..." },
      { "type": "tool_use", "name": "Edit", "input": { "file_path": "src/main.rs", ... } }
    ]
  }
}
```

**`user`** (prompts, version):
```json
{ "type": "user", "timestamp": "...", "version": "2.1.86", "gitBranch": "main", "message": { "role": "user", "content": "..." } }
```

**`last-prompt`** (session tail marker):
```json
{ "type": "last-prompt", "lastPrompt": "...", "sessionId": "..." }
```

- **Size: 1KB–18MB**. Append-only, new line per message.
- **Reading strategy**: On first discovery, scan full file to build cumulative token totals. Then watch file size — on growth, read only new bytes appended since last read (track file offset). This gives both lifetime totals and real-time updates without re-reading.
- **Partial line handling**: new bytes may end mid-JSON-line. Buffer incomplete lines until next read.
- **File rotation**: if file shrinks (session restart), reset offset to 0 and re-scan.

### 3. Codex CLI sessions: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`

Discovery strategy:
1. Find running `codex` processes via `ps`
2. Map PID → open `rollout-*.jsonl` file via `lsof`
3. Parse JSONL for `session_meta`, `token_count` (includes rate_limits), `agent_message` events
4. Detect finished sessions: scan today's directory for JSONL < 5 min old not owned by running process

Rate limits extracted from `token_count` events:
```json
{
  "rate_limits": {
    "limit_id": "codex",
    "primary": { "used_percent": 9.0, "window_minutes": 300, "resets_at": 1774686045 },
    "secondary": { "used_percent": 14.0, "window_minutes": 10080, "resets_at": 1775186466 },
    "plan_type": "plus"
  }
}
```

### 4. OpenCode sessions: `~/.local/share/opencode/opencode.db`
- Discover running `opencode` processes via shared `ps` data.
- Read recent sessions from OpenCode's SQLite DB through `sqlite3 -readonly -json`.
- Match live PIDs to DB sessions by process cwd. OpenCode does not expose a PID/session mapping, so when multiple DB rows share one cwd, only live PIDs should be assigned and older rows should not be shown as live duplicates.
- OpenCode contributes session/token/project/port data, but not quota data. Quota remains Claude + Codex only.
- **Context % is live, not cumulative, and excludes subagent turns.** OpenCode inlines sub-agent/task-agent turns (tagged with a different `agent` in each message's `data`) into the parent session's message table. The conversation token sums therefore filter to the session's own `agent` (`session.agent`), so subagent window consumption is not double-counted into the parent's context %. Context % is then derived from the most recent real LLM call's full prompt (`cache.read + input` of the last main-agent message that consumed tokens) over the model window — not the lifetime token sum. Sessions whose `agent` is unset fall back to counting all messages.

### 5. kimi-code sessions: `~/.kimi-code/sessions/wd_<base>_<hash>/session_<uuid>/`
kimi migrated its storage from `~/.kimi` to `~/.kimi-code` (an `.migrated-to-kimi-code`
marker is left behind in the legacy root); the collector prefers `~/.kimi-code` and only
falls back to `~/.kimi` if the new root is absent.
- Discover sessions from `~/.kimi-code/session_index.jsonl` — one JSON object per line,
  `{sessionId, sessionDir, workDir}`. This is the authoritative session list (kimi's old
  `<md5(cwd)>` directory encoding was retired). A live `kimi-code` process is detected via
  shared `ps` data (the `kimi-cli` runtime rewrites argv[0] to `kimi-code` via setproctitle;
  match the first token precisely), but it is **not** linked via `/proc/{pid}/cwd` — every
  kimi-code process chdir's to the config root, so its cwd is meaningless.
- Liveness + PID attribution: kimi runs a client+server model with no per-session PID file.
  The shared server daemon's PID lives in `server/lock` (`{"pid":N,...}`) and is the global
  "kimi is running" gate. Each live `kimi-code` PID is attributed to a session by walking its
  ancestor shell chain and matching the shell's cwd (kimi's launch directory) to a session
  `workDir`. The walk is bounded to **shell** ancestors and stops at the first non-shell
  (tmux/screen/init/daemon) — a multiplexer server's cwd is a stale launch-time directory, not
  the directory kimi was run in, so traversing past it would resurface every session that ever
  ran there as a ghost. Because a PID can be tied only to a directory (not a session), only the
  single most-recently-active session under each live workdir is surfaced as current; older
  sessions sharing a workdir stay hidden unless they were just active, so historical runs do
  not reappear as ghosts whenever one process is alive in that directory. Exit detection is
  best-effort: a session with no live PID is also shown while its wire log was touched within
  the last few minutes, then ages out.
- Tail `agents/main/wire.jsonl` (there is no separate `context.jsonl` anymore):
  - `usage.record` — per-turn tokens (`usage.inputOther` / `output` / `inputCacheRead` /
    `inputCacheCreation`, camelCase), `model`, `time` (epoch ms). kimi emits **no**
    context-window field, so context % is derived (last-turn input / hardcoded 262144 window),
    mirroring the Claude collector's accounting.
  - `context.append_message` — user/assistant chat turns (`message.role` / `message.content`).
  - `context.append_loop_event` — `tool.call` (tool name + args → current task, tool timeline,
    file-access audit), `tool.result`, `content.part` (streaming assistant text), `step.*`.
- `state.json` provides `title` (+ `isCustomTitle`) for the session title — kimi generates it
  itself, so no external summarizer is needed.
- kimi contributes session/token/context/project/port data, but not quota data (managed OAuth;
  no local rate-limit telemetry). Quota remains Claude + Codex only.

**Known limitations of kimi support (all heuristic/derived):**
- Context % is **derived** against a hardcoded 262,144-token window (`KIMI_CONTEXT_WINDOW`); kimi emits no context-window field. Same approach as the Claude collector.
- **No git branch** — only added/modified counts are populated by the shared git pass; kimi has no transcript-level branch field, so the projects panel shows counts but no branch name.
- **No Error/Done status** — only Executing / Thinking / Waiting are derived from activity.
- **No rate-limit/quota telemetry** (managed OAuth) — quota stays Claude + Codex only.
- **No subagents / memory** — kimi exposes no subagent tree or memory directory.

### 6. Hermes Agent sessions: `$HERMES_HOME/state.db` (default `~/.hermes`)
Hermes Agent (NousResearch/hermes-agent) stores all session metadata, message history,
and model config in a single SQLite database (WAL mode) at `$HERMES_HOME/state.db`
(default `~/.hermes`; native Windows uses `%LOCALAPPDATA%\hermes`). This replaced an
earlier per-session JSONL trajectory format. v1 monitors the default profile only.
- Discover open CLI sessions from the `sessions` table
  (`WHERE ended_at IS NULL AND source = 'cli'`) via `sqlite3 -readonly -json`
  (WAL-safe concurrent read). Rows are cached and refreshed only on the slow tick
  (~10s). Token counts (`input_tokens` / `output_tokens` / `cache_read_tokens` /
  `cache_write_tokens`), `model`, `title`, `message_count`, and `started_at` are
  first-class columns — no transcript parsing needed. A `last_active` subquery (max
  `messages.timestamp`) drives recency; a `preview` subquery (first user message) is
  the title fallback.
- Liveness + PID attribution: the `sessions` table has **no `cwd`/`directory`
  column**, so PIDs are paired to sessions by **recency**, not cwd. Live `hermes`
  agent processes are found via shared `ps` data (the installer ships a `hermes`
  shim; uv/python wrappers may show `python -m hermes`, so match either a first-token
  basename hit or any command referencing `hermes`). The long-lived messaging gateway
  daemon (`hermes gateway …`) is excluded. PIDs are ordered by RSS so the primary
  process pairs first; subagents then surface as children. A paired PID contributes
  cwd, memory, status, children, and ports.
- Sessions with no paired PID are shown as `Unknown` only while recent
  (`NO_PID_RECENCY_WINDOW_SECS`, 30 min) — hedges against PID-matcher gaps while
  bounding stale/crashed sessions whose `ended_at` never got set.
- Hermes contributes session/token/status/task/git/port data, but not quota data
  (BYO provider; no local account-level rate-limit source). Quota remains Claude +
  Codex only.

**Known limitations of Hermes support (v1):**
- **Default profile only** — `$HERMES_HOME` / `~/.hermes`. Multi-profile discovery
  from live PIDs' `HERMES_HOME` environ is a future enhancement (mirror Claude's
  `refresh_config_dirs`).
- **No context-window %** — Hermes is multi-provider with no single window.
- **No rate-limit/quota telemetry** (BYO provider) — quota stays Claude + Codex only.
- **No subagents / memory / chat-tool-call enrichment** — data lives in the
  `messages` table but is not parsed yet.
- PID↔session pairing is by recency (no cwd column), so with multiple concurrent
  sessions in one profile the exact pairing is best-effort.

### 7. Pi sessions: `~/.pi/agent/sessions` + per-PID sidecar

Pi (and Pi-derivatives built on the `@earendil-works/pi-coding-agent` SDK, e.g.
my-pi-agent) stores sessions as append-only JSONL under
`~/.pi/agent/sessions/--<encoded-cwd>--/<ts>_<sessionId>.jsonl`. The transcripts
carry rich telemetry but **no PID and no exit marker**, so liveness + process
attribution come from a per-PID sidecar written by a Pi-side monitor extension
(see `docs/pi-sidecar-contract.md`):

- **Sidecar** (authoritative): `~/.pi/agent/sessions/active/{pid}.json` holds
  `pid`, `procStart` (PID-reuse guard), `sessionId`/`sessionFile`/`cwd`/
  `startedAt`, distro `agent`/`version`, and `contextWindow`/`contextPercent`
  from the extension's live context. Enumerate these files, verify liveness
  (pid alive + procStart match), and use `contextWindow`/`contextPercent`
  directly.
- **Transcript-only fallback** (no sidecar installed): scan `*.jsonl` for the
  `session` header `cwd` (authoritative; the encoded dir name is lossy), match
  a live Pi process's `/proc/{pid}/cwd` to that cwd. PID=0, status `Unknown`,
  recency-bounded so stale sessions age out. Context % is not derivable.

Transcript parsing (mirrors the Claude collector): `message` entries —
assistant `usage` (camelCase `input`/`output`/`cacheRead`/`cacheWrite`),
`model`, chat roles (user/assistant/toolResult), `toolCall` content blocks
(name + arguments) → tasks + file-access audit; `model_change`;
`thinking_level_change` → effort; `custom` (`customType:"modes"`) → mode;
`compaction` → compaction count; `session_info` → title.

Pi exposes no rate-limit/quota telemetry (managed OAuth), so it contributes
nothing to the quota panel — quota stays Claude + Codex only.

**Known limitations of Pi support (v1):** context % requires the monitor
sidecar (no reliable window via transcript fallback); no git branch from the
transcript; no subagents/memory. The default config root is
`~/.pi/agent/sessions` (`PI_CODING_AGENT_DIR`/`MY_PI_AGENT_CODING_AGENT_DIR`
overrides are honored).

### 8. Subagents: `~/.claude/projects/{path}/{sessionId}/subagents/`
- `agent-{hash}.jsonl` — same JSONL format as main transcript
- `agent-{hash}.meta.json` — `{ "agentType": "general-purpose", "description": "..." }`

### 9. Process tree: `ps` + `lsof`
```bash
ps -eo pid,ppid,rss,%cpu,command    # All processes
lsof -i -P -n -sTCP:LISTEN         # Open ports
```
- Build parent→children map from ppid
- Map listening PID → parent agent PID → session

### 10. Git status per project
```bash
git -C {cwd} status --porcelain     # added/modified file counts
```

### 11. Memory status
- Path: `~/.claude/projects/{encoded-path}/memory/`
- Count files in directory + lines in `MEMORY.md`

### 12. Rate limit (Claude Code)

NOT in transcript JSONL. Collected via StatusLine mechanism.

`abtop --setup` automates this: creates a script at `~/.claude/abtop-statusline.sh` that writes rate limit JSON to `~/.claude/abtop-rate-limits.json`, and registers it in `~/.claude/settings.json`.

File format read by abtop:
```json
{
  "source": "claude",
  "five_hour": { "used_percentage": 35.0, "resets_at": 1774715000 },
  "seven_day": { "used_percentage": 12.0, "resets_at": 1775320000 },
  "updated_at": 1774714400
}
```
- Rejects stale data (> 10 minutes old).
- `rate_limits` only present for Pro/Max subscribers.
- Account-level metric, shared across all sessions.
- Show "—" when not configured or data unavailable.

### 13. Other files
- `~/.claude/stats-cache.json` — daily aggregates. Only updated on `/stats`, NOT real-time.
- `~/.claude/history.jsonl` — prompt history with sessionId.

## Session Status Detection

```
● Working  = PID alive + transcript mtime < 30s ago
◌ Waiting  = PID alive + transcript mtime > 30s ago
✗ Error    = PID alive + last assistant has error content
✓ Done     = PID dead (detected via kill(pid, 0) failure)
```

**Done detection**: session files are deleted on normal exit, but may linger briefly or survive crashes. When PID is dead but file exists, show as Done and clean up on next tick.

**PID reuse risk**: verify PID is still the expected agent process (Claude, Codex, or OpenCode) by checking `ps -p {pid} -o command=`. Don't trust PID alone.

Current task (2nd line under each session):
- Working → last `tool_use` name + first arg (e.g. `Edit src/main.rs`)
- Waiting → "waiting for user input"
- Error → last error message (truncated)
- Done → "finished {duration} ago"

**Known limitations** (all heuristic):
- Cannot distinguish model-thinking vs tool-executing vs rate-limit-waiting vs permission-prompt
- "Waiting" may be wrong if a long-running tool (cargo build, npm test) is running
- Status is best-effort, not authoritative

## Session Summary Generation

Each session gets a one-line summary title generated via `claude --print`:
- Spawned as background process with 10s timeout
- Rejects generic/empty output; falls back to sanitized first prompt (28 chars)
- Cached to `~/.cache/abtop/summaries.json` (persists across runs)
- Max 3 concurrent summary jobs, max 2 retries per session

## Context Window Calculation

Not provided in data files. Derive:
- **Window size**: hardcode by model name
  - `claude-opus-4-6` → 200,000 (default)
  - `claude-opus-4-6[1m]` → 1,000,000
  - `claude-sonnet-4-6` → 200,000
  - `claude-haiku-4-5` → 200,000
  - kimi-code → 262,144 (hardcoded `KIMI_CONTEXT_WINDOW`; kimi emits no window field, so the same derivation applies — see §5)
  - OpenCode → 1,000,000 for GLM-5.2+ (Z.ai / Zhipu; detected by `glm-5.<minor>=2+`), else 200,000
  - Hermes → **not derived** (multi-provider; no single context window — see §6)
- **Current usage**: last `assistant` line's `input_tokens + cache_read_input_tokens`. `cache_creation_input_tokens` is intentionally excluded — on compaction turns the same tokens can be reported as both `cache_creation` *and* `cache_read`, and summing all three double-counts (#54). Matches Claude Code's own statusline and the Codex collector.
  - OpenCode → the most recent real LLM call's full prompt (`cache.read + input` of the last main-agent message that consumed tokens); zero-token stub messages are skipped.
- **Percentage**: current_usage / window_size * 100
- **Warning**: yellow at 80%, red at 90%, ⚠ icon at 90%+

## Orphan Port Detection

Tracks child processes that have open ports. When a parent session dies but the child process remains alive and listening:
- Added to `orphan_ports` list automatically
- Displayed in ports panel under "ORPHAN PORTS" section
- Can be killed via `X` (Shift+X) with safety checks (fresh port scan + PID command verification before SIGKILL)

## Key Bindings

| Key | Action |
|-----|--------|
| `↑`/`↓` or `k`/`j` | Select session in list |
| `Enter` | Jump to session terminal (cmux / tmux / iTerm2) |
| `x` | Kill selected session (SIGKILL) |
| `X` | Kill all orphan ports |
| `q` | Quit |
| `r` | Force refresh |

## Tech Stack

- **Rust** (2021 edition)
- **ratatui** + **crossterm** for TUI
- **serde** + **serde_json** for JSON/JSONL parsing
- **chrono** for timestamp formatting
- **dirs** for home directory resolution
- **Polling intervals** (staggered to avoid freezes):
  - Session scan + transcript tail: every 2s
  - Process tree (ps): every 2s
  - Port scan (lsof) + git status + rate limits: every 10s (5 ticks)

## Commit Convention

```
<type>: <description>
```
Types: `feat`, `fix`, `refactor`, `docs`, `chore`

## Commands

```bash
cargo build                    # Build
cargo run                      # Run TUI
cargo run -- --once            # Print snapshot and exit
cargo run -- --setup           # Install StatusLine hook for rate limit collection
cargo run -- --exit-on-jump    # Quit after Enter-jumping to a session terminal (for popup overlays)
cargo test                     # Tests
cargo clippy                   # Lint
```

## Release Process

1. Pick the target semver version and update both `Cargo.toml` and `Cargo.lock`.
2. Verify the package locally:
   ```bash
   cargo test
   cargo clippy -- -D warnings
   cargo build --release
   cargo publish --dry-run
   ```
3. Commit and merge or push the version bump to `main`:
   ```bash
   git add Cargo.toml Cargo.lock
   git commit -m "chore: bump version to X.Y.Z"
   git push origin main
   ```
4. From a clean, up-to-date `main`, create and push an annotated release tag:
   ```bash
   git tag -a vX.Y.Z -m "vX.Y.Z"
   git push origin vX.Y.Z
   ```
5. Watch the tag-triggered workflows:
   ```bash
   gh run list --workflow Release --limit 5
   gh run list --workflow "Publish to crates.io" --limit 5
   ```
6. `release.yml` builds platform binaries, creates the GitHub Release, and updates the Homebrew formula.
7. `publish.yml` runs `cargo publish` to crates.io automatically.

**Do NOT run `cargo publish` or `gh release create` manually** — the CI workflows handle both.
**Do NOT push the tag before the version bump is on `main`.**
**Do NOT reuse a release tag after a failed publish; bump to a new patch version instead.**

## Non-Goals (v0.1)

- Gemini/Cursor support
- Cost estimation
- Remote/SSH monitoring
- Notifications/alerts
- Rate-limit/quota telemetry for OpenCode, kimi-code, and Hermes Agent (all use managed OAuth / BYO providers with no local account-level rate-limit source; the quota panel stays Claude + Codex only). This is a data limitation only — kimi-code, OpenCode, and Hermes are otherwise first-class supported agents.

## Terminal Jump (`Enter`)

`Enter` focuses the terminal running the selected session's agent process.
The logic lives in `src/jump/` as a registry of `TerminalJumper` adapters
(one file per backend). `jumpers()` is the single ordered source of truth;
`resolve()` walks it and the first applicable adapter wins.

Each adapter returns a three-way `JumpAttempt`:
- `NotApplicable` — not this backend's terminal; try the next adapter.
- `Jumped` — focused successfully; stop.
- `Failed(msg)` — this backend owns the process but the focus command errored;
  stop and surface `"<backend>: <msg>"` in the status line.

Order (most specific first), mutually exclusive by controlling tty:

1. **cmux** (`jump/cmux.rs`) — reads `CMUX_WORKSPACE_ID` (a UUID cmux exports
   into every surface, inherited by the agent) from the process environment via
   `ps eww`, then `cmux select-workspace --workspace <uuid>`.
2. **tmux** (`jump/tmux.rs`) — only when abtop itself runs inside tmux (`$TMUX`).
   Maps PID → pane via `tmux list-panes -a -F '#{pane_pid} #{session_name}:#{window_index}.#{pane_index}'`
   + process-tree descent, then `switch-client` / `select-window` / `select-pane`.
   PID in no pane → `NotApplicable` (lets another backend try).
3. **iTerm2** (`jump/iterm2.rs`) — resolves the PID's controlling tty (`ps -o tty=`),
   then AppleScript selects the session whose `tty` matches and brings its
   window/app to the front. First call triggers a one-time macOS Automation
   permission prompt; until granted, `osascript` exits non-zero → `Failed`.

Parsing/registry logic is unit-tested in `jump/mod.rs`; the thin `ps`/`osascript`/
`tmux` I/O wrappers are verified manually.

## Privacy

abtop reads transcripts, prompts, tool inputs, and memory files. These may contain secrets.
- **`--once` output**: redact file contents from tool_use inputs. Show tool name + file path only, not content.
- **TUI mode**: show tool name + first arg (file path), never show file contents or prompt text in session list.
- **No network**: abtop never sends data anywhere. All local reads.
- **Exception**: summary generation calls `claude --print` locally (no network by abtop itself, but claude may use its API).

## Gotchas

- **Transcript size**: 1KB–18MB. On first load, full scan for totals. After that, track file offset and read only new bytes. Buffer partial lines.
- **Session file deletion**: files disappear when Claude exits. Handle `NotFound` between scan and read.
- **stats-cache.json is stale**: only updated on `/stats` command. Don't use for live data.
- **Context window not in data**: must hardcode per model. Will break if Anthropic/OpenAI add new models.
- **Rate limit is account-level**: shared across all sessions. Don't show per-session.
- **Path encoding**: `/Users/foo/bar` → `-Users-foo-bar`. Used for transcript directory names.
- **Path encoding collision**: `-Users-foo-bar-baz` could be `/Users/foo/bar-baz` or `/Users/foo-bar/baz`. Use session JSON's `cwd` as source of truth.
- **lsof can be slow**: on macOS with many open files. Cache results, poll every 10s.
- **Child process tree**: `pgrep -P` only gets direct children. Build full tree from `ps -eo ppid`.
- **Port detection race**: a port can close between lsof and display. Show stale data gracefully.
- **Subagent directory may not exist**: only created when Agent tool is used. Check existence before scanning.
- **Undocumented internals**: all data sources are Claude Code/Codex implementation details, not stable APIs. Schema may change without notice. Defensive parsing with `serde(default)` everywhere.
- **Terminal size**: minimum 80x24. Panels degrade gracefully when small (context panel hidden first).
- **PID reuse in port cache**: invalidate cached ports when the set of tracked PIDs changes.
- **Rate limit staleness**: reject rate limit data older than 10 minutes.
- **`/clear` + multi-PID same cwd**: after `/clear`, Claude Code mints a new `sessionId` + `.jsonl` without rewriting `sessions/{PID}.json`. abtop overrides the stale sid by picking the newest transcript in the project dir, but this heuristic can't disambiguate ownership when two live `claude` PIDs share a cwd — so the override is disabled in that case and both sessions keep their original sid until exit. Use separate worktrees if live tracking is needed on both simultaneously.
