# abtop

**Like [btop](https://github.com/aristocratos/btop), but for your AI coding agents.**

See every Claude Code, Codex CLI, OpenCode, kimi-code, Hermes Agent, and Pi session at a glance — token usage, context window %, rate limits, child processes, open ports, and more.
Claude Code, Codex CLI, OpenCode, kimi-code, Hermes Agent, and Pi sessions are discovered from local process/file state, so multiple active profiles are supported across macOS, Linux, and Windows.

![demo](https://raw.githubusercontent.com/graykode/abtop/main/assets/demo.gif)

## Why

- Running 3+ agents across projects? See them all in one screen.
- Hitting rate limits? Watch your quota in real-time.
- Agent spawned a server and forgot to kill it? Orphan port detection.
- Context window filling up? Per-session % bars with warnings.

All read-only. No API keys. No auth.

## Install

### macOS / Linux

> [!IMPORTANT]
> On Linux, ensure `sqlite3` is installed to enable monitoring for OpenCode sessions.

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/graykode/abtop/releases/latest/download/abtop-installer.sh | sh
```

### Cargo

```bash
cargo install abtop
```

### Windows

Native support — no WSL required. Uses `sysinfo` for process info and host CPU/MEM metrics, and `netstat -ano` for listening ports. Windows has no load average, so LOAD is reported as 0. OpenCode session discovery additionally requires the `sqlite3` CLI (`winget install SQLite.SQLite`); without it abtop prints a one-time warning to stderr.

```powershell
powershell -c "irm https://github.com/graykode/abtop/releases/latest/download/abtop-installer.ps1 | iex"
```

Or `cargo install abtop` from any terminal with Git in PATH. Claude Code config is resolved automatically from `%USERPROFILE%\.claude`.

### Other

Pre-built binaries for all platforms are available on the [GitHub Releases](https://github.com/graykode/abtop/releases) page.

## Usage

```bash
abtop                    # Launch TUI
abtop --once             # Print snapshot and exit
abtop --json             # Print one JSON snapshot and exit (for scripts/tools)
abtop --setup            # Install rate limit collection hook
abtop --theme dracula    # Launch with a specific theme
abtop --mouse            # Enable mouse click/scroll navigation
```

Recommended terminal size: **120x40** or larger. Minimum 80x24 — panels hide gracefully when small.
Mouse capture is off by default so terminal drag selection and copy keep working. Launch with `--mouse` if you prefer click targets and wheel navigation.

### Terminal Jump

Press `Enter` to focus the terminal running the selected agent. abtop supports cmux, tmux, and iTerm2 on macOS.

```bash
tmux new -s work
# pane 0: abtop
# pane 1: claude (project A)
# pane 2: claude (project B)
# → Enter on a session in abtop jumps to its pane
```

## Supported Agents

| Feature           | Claude Code | Codex CLI | OpenCode | kimi-code | Hermes | Pi |
| ----------------- | :---------: | :-------: | :------: | :------: | :-----: | :-: |
| Session Discovery |     ✅      |    ✅     |    ✅    |    ✅    |   ✅    | ✅  |
| Token Tracking    |     ✅      |    ✅     |    ✅    |    ✅    |   ✅    | ✅  |
| Context Window %  |     ✅      |    ✅     |    ❌    |    ✅    |   ❌    | ✅¹ |
| Status Detection  |     ✅      |    ✅     |    ✅    |    ✅    |   ✅    | ✅² |
| Current Task      |     ✅      |    ✅     |    ❌    |    ✅    |   ✅    | ✅  |
| Rate Limit        |     ✅      |    ✅     |    ❌    |    ❌    |   ❌    | ❌  |
| Git Status        |     ✅      |    ✅     |    ✅    |    ✅    |   ✅    | ✅³ |
| Children / Ports  |     ✅      |    ✅     |    ✅    |    ✅    |   ✅    | ✅  |
| Subagents         |     ✅      |    ❌     |    ❌    |    ❌    |   ❌    | ❌  |
| Memory Status     |     ✅      |    ❌     |    ❌    |    ❌    |   ❌    | ❌  |

¹ **Pi context window %** requires the Pi monitor sidecar (see below). Without the
sidecar installed, Pi falls back to transcript-only discovery and shows no
context %.

² **Pi status** is derived as Executing / Thinking / Waiting / Unknown; there is
no Error/Done state.

³ **Pi git status** shows added/modified file counts (via the shared git pass)
but no branch name.

OpenCode support reads the local SQLite database at `~/.local/share/opencode/opencode.db` (also the default location on Windows; `%LOCALAPPDATA%\opencode` and `%APPDATA%\opencode` are probed as fallbacks) and requires `sqlite3` in `PATH` (on Windows: `winget install SQLite.SQLite`).

kimi-code support reads `~/.kimi-code/session_index.jsonl` and tails each session's `agents/main/wire.jsonl`. It honors `$KIMI_CONFIG_DIR` and falls back to the legacy `~/.kimi` root. **kimi-code is a first-class supported agent and an active focus of development** — it contributes session, token, context-window, current-task, project, and port data. (Rate-limit/quota remains Claude + Codex only, since kimi uses managed OAuth with no local telemetry.)

Hermes Agent support reads the local SQLite database at `$HERMES_HOME/state.db` (default `~/.hermes`) and requires `sqlite3` in `PATH`. Open CLI sessions are read from the `sessions` table (`ended_at IS NULL`, `source = 'cli'`) and paired with live `hermes` processes for cwd, status, and children/ports. It contributes session, token, status, current-task, git, and port data. (No context-window % or rate-limit column: Hermes is multi-provider with no single context window or managed quota.)

Pi support (any Pi or Pi-derivative coding agent built on the `@earendil-works/pi-coding-agent` SDK, e.g. `my-pi-agent`) reads append-only JSONL transcripts under `~/.pi/agent/sessions` (honoring `$PI_CODING_AGENT_DIR` / `$MY_PI_AGENT_CODING_AGENT_DIR`). Liveness and process attribution come from a per-PID sidecar written by a [Pi monitor extension](docs/pi-sidecar-contract.md) at `~/.pi/agent/sessions/active/{pid}.json`, which also carries the live `contextWindow`/`contextPercent`. When no sidecar is installed, abtop falls back to matching transcripts to live Pi processes by cwd, showing sessions as `Unknown` with no context %. It contributes session, token, context-window (with sidecar), current-task, project, git-count, and port data. (No rate limit, subagents, or memory: Pi uses managed OAuth with no local quota or subagent/memory telemetry.)

## Themes

12 built-in themes, including 4 colorblind-friendly options (`high-contrast`, `protanopia`, `deuteranopia`, `tritanopia`). Press `t` to cycle at runtime, or launch with `--theme <name>`. Your choice is saved to `~/.config/abtop/config.toml`.

| btop (default) | dracula | catppuccin |
|:-:|:-:|:-:|
| ![btop](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/btop.png) | ![dracula](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/dracula.png) | ![catppuccin](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/catppuccin.png) |

| tokyo-night | gruvbox | nord |
|:-:|:-:|:-:|
| ![tokyo-night](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/tokyo-night.png) | ![gruvbox](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/gruvbox.png) | ![nord](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/nord.png) |

Colorblind-friendly themes:

| high-contrast | protanopia |
|:-:|:-:|
| ![high-contrast](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/high-contrast.png) | ![protanopia](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/protanopia.png) |

| deuteranopia | tritanopia |
|:-:|:-:|
| ![deuteranopia](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/deuteranopia.png) | ![tritanopia](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/tritanopia.png) |

Light themes (`light` — Solarized cream, `white` — GitHub-style pure white) for bright terminals:

| light | white |
|:-:|:-:|
| ![light](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/light.png) | ![white](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/white.png) |

## Configuration

`~/.config/abtop/config.toml` supports:

```toml
theme = "btop"
# Hide specific agent CLIs from the TUI (case-insensitive).
# Useful if you only use one agent and want a cleaner view.
hidden_agents = ["codex"]
# Additional Claude Code profile roots to scan.
# abtop also auto-discovers ~/.claude and ~/.claude-* roots that contain
# both sessions/ and projects/.
claude_config_dirs = ["~/.claude-personal", "~/.claude-work-team"]
# UI language. Omit or leave empty to auto-detect from LANG.
language = "zh"
```

### Supported Languages

| Code | Language            |
| ---- | ------------------- |
| `en` | English (default)   |
| `zh` | Simplified Chinese  |

When `language` is unset, abtop auto-detects from `LANG` — any value starting with `zh` switches to Simplified Chinese, otherwise English.

## Key Bindings

| Key                | Action                               |
| ------------------ | ------------------------------------ |
| `↑`/`↓` or `k`/`j` | Select session                       |
| `Enter`            | Jump to session terminal             |
| `x`                | Kill selected session                |
| `X`                | Kill all orphan ports                |
| `t`                | Cycle theme                          |
| `1`–`5`            | Toggle panel visibility              |
| `Esc`              | Open/close config page               |
| `q`                | Quit                                 |
| `r`                | Force refresh                        |

## Library / JSON snapshot

abtop is also a library crate, so local tools can reuse its data-collection
layer in-process — no re-scanning, no subprocesses — and serialize the same
state the TUI renders.

```bash
abtop --json    # one-shot JSON snapshot for scripts
```

For long-running consumers, build an `App`, refresh it with
`App::tick_no_summaries()` (which never spawns `claude --print`, so it doesn't
touch your Claude quota), and call `App::to_snapshot(interval_ms)` to get a
JSON-serializable [`Snapshot`]:

```rust,no_run
use abtop::app::App;
use abtop::{config, theme::Theme};

let cfg = config::load_config();
let mut app = App::new_with_config_and_claude_dirs(
    Theme::default(), &cfg.hidden_agents, cfg.panels, &cfg.claude_config_dirs,
);
app.tick_no_summaries();
let json = serde_json::to_string(&app.to_snapshot(2_000)).unwrap();
```

`App` is not `Send` (it owns the collectors), so keep it on one thread and pass
the serialized JSON elsewhere. [abtop-web-ui](https://github.com/XKHoshizora/abtop-web-ui)
is a reference consumer: a local-first web dashboard built on exactly this API.

## Privacy

abtop reads local files and local process/open-file metadata only. No API keys, no auth. In the TUI and `--once` output, tool names and file paths are shown, but file contents and prompt text are never displayed. Session summaries are generated via `claude --print`, which makes its own API call — this is the only indirect network usage.

The JSON snapshot includes richer local dashboard data, including `summary`, `chat_messages`, working directories, config roots, tool-call previews, child process commands, token counts, and port metadata. Chat text is bounded and redacted by the collectors, but it is still derived from local transcripts and may contain sensitive project context. Treat JSON snapshots as local/private data and avoid writing them to shared logs or exposing them on a network without your own access controls.

## Acknowledgements

Huge thanks to [@tbouquet](https://github.com/tbouquet) for driving much of abtop's recent shape — themes, config overlay and panel toggles, session filtering, subagent tree view, the context window gauge with compaction detection, plus a steady stream of fixes and security hardening along the way.

## License

MIT
