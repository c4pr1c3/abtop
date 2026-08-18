# Pi Session Sidecar Contract

This is the cross-repo contract that unlocks abtop monitoring + session-jump for
**Pi** and any Pi-derivative coding agent (collectively "Pi"). The sidecar is the
single point of truth for *process attribution* — the one thing abtop cannot
derive from Pi's transcripts.

## Why this exists

Pi persists sessions as append-only JSONL under
`~/.pi/agent/sessions/--<encoded-cwd>--/<ts>_<sessionId>.jsonl`. The transcripts
contain rich data (usage, model, chat, tools) but **no PID and no exit marker**.
abtop keys liveness/status AND the `Enter` jump-to-terminal feature off a live
PID. The sidecar closes that gap.

The Pi-side **monitor extension** is the writer; **abtop** is the reader. They
communicate only through this contract.

## Location

```
~/.pi/agent/sessions/active/{pid}.json
```

- `~/.pi/agent/sessions/` is the **shared Pi sessions root** — every Pi distro on
  a host writes here (verified: my-pi-agent and pi-course sessions already coexist).
- `active/` is a subdirectory abtop scans for live sessions.
- One file **per PID**, named by PID. This is structurally collision-free across
  concurrent Pi processes in the same cwd (the Claude "multi-PID same cwd" gotcha).
  It mirrors Claude Code's `sessions/{PID}.json` semantics, which abtop already
  consumes with mature machinery.

> The `ENV_AGENT_DIR` env override (`PI_CODING_AGENT_DIR`, or
> `MY_PI_AGENT_CODING_AGENT_DIR` for a distro) moves the whole root. abtop scans
> the **default** root; sidecars under an overridden root are a documented
> limitation (see abtop PiCollector).

## File format (JSON)

```json
{
  "pid": 12345,
  "procStart": 394363811,
  "agent": "pi",
  "version": "0.84.2",
  "sessionId": "01a0150e-2f27-76c5-8aab-54b21c1a4907",
  "sessionFile": "~/.pi/agent/sessions/--home--...--/2026-08-18T....jsonl",
  "cwd": "/home/user/project",
  "startedAt": 1787059700000,
  "contextWindow": 262144,
  "contextPercent": 42.5,
  "model": "cuc/deepseek"
}
```

| Field | Type | Required | Meaning |
|-------|------|----------|---------|
| `pid` | int | yes | Process ID of the live Pi process owning this session. |
| `procStart` | int | no | Process start time in clock ticks (`/proc/{pid}/stat` field 22) for PID-reuse verification. Linux only; `0`/absent elsewhere. |
| `agent` | string | yes | Distro identity, e.g. `"pi"` or `"my-pi-agent"`. abtop surfaces this via the session `version`/`config_root`, not as a separate collector. |
| `version` | string | no | Distro package version. |
| `sessionId` | string | yes | Pi session id (from the session header). |
| `sessionFile` | string | yes | Absolute path to the session JSONL. Lets abtop tail it without re-encoding the cwd (the encoded dir name is lossy). |
| `cwd` | string | yes | Working directory (must equal session header `cwd`). |
| `startedAt` | int | yes | Epoch ms session started. |
| `contextWindow` | int | no | Model context window, from `ctx.getContextUsage()`. Absent when unavailable. |
| `contextPercent` | float | no | Current context %, from `ctx.getContextUsage()`. Absent when unavailable. |
| `model` | string | no | Current model id. |

The sidecar is intentionally **minimal**. Token totals, chat, tool calls, tasks,
compaction — everything else abtop can tail from the JSONL — are NOT duplicated
here, so there is no second source of truth to drift.

## Lifecycle (writer rules)

- **Upsert** this file on **every** `session_start` event (any reason:
  `startup` `reload` `new` `resume` `fork`).
- **Delete** the file only on `session_shutdown` with `reason === "quit"`.
  Session *replacement* (`reload`/`new`/`resume`/`fork`) MUST keep the file —
  the process is still live, abtop should keep showing it.
- Update `contextPercent`/`model` opportunistically on `turn_end` (cheap, in-memory
  state; no need to re-read the transcript).
- On process exit, flush/deallocate. **Do not rely on being called** — a
  `SIGKILL`/crash fires no event and leaves a stale file.

## Lifecycle (reader rules — abtop)

- Enumerate `~/.pi/agent/sessions/active/*.json`, parse defensively
  (`serde(default)`), skip malformed files.
- Treat each file as a **claim**, not ground truth. Verify:
  1. `pid` is alive (`process_info` lookup), and
  2. when `procStart > 0`, it matches `ProcInfo.start_ticks` (PID-reuse guard).
- Stale files (dead PID) are used for a recency-bounded `Unknown`/`Done` window,
  then GC'd — the Pi monitor extension cannot reliably delete on crash.
- `sessionFile` is the authoritative transcript path for tailing.

## Writer → Reader contract summary

| Concern | Ownership |
|---------|-----------|
| Process identity (`pid`, `procStart`) | sidecar (writer) |
| Session identity (`sessionId`, `sessionFile`, `cwd`, `startedAt`) | sidecar (writer) |
| Context window / % | sidecar (writer, from `ctx.getContextUsage()`) |
| Distro identity (`agent`, `version`) | sidecar (writer) |
| Token totals, chat, tools, tasks, compaction | JSONL transcript tail (reader) |
| Liveness verification, PID-reuse, stale GC | abtop (reader) |

## Deriving the contract

For context: `ctx.getContextUsage()` (Pi ExtensionAPI,
`core/extensions/types.ts`) already exposes `{ tokens, contextWindow, percent }`,
so context% needs no derivation or per-model hardcoding.
