# Design

## Philosophy

`claude-mesh` is a **presence + context-routing layer, not an inference layer.** It never
runs a model to "answer" a cross-window question. The reasoning always happens in the
session that *asked* — which is already running on the user's subscription — so the mesh
adds zero inference cost and zero API spend. The mesh only moves two cheap things around:
*who is online*, and *a slice of a peer's transcript when it's actually asked for*.

This constraint is load-bearing. An earlier design had the answering side spawn
`claude -p --resume` to reply; that bills off-subscription per ask. Routing context to the
already-running asker instead makes the whole thing free, which is the point.

## Components (one binary, role per subcommand)

| Role | Subcommand | Responsibility |
|------|-----------|----------------|
| Hook | `hook session-start` / `session-end` | Registers/de-registers a window by writing a small file in `~/.claude-mesh/sessions/`; ensures broker + daemon are up. Must be fast, silent on stdout, and never fail a session. |
| Daemon | `daemon` | One per machine. Mirrors session files to the broker as presence (one outbound WebSocket). Answers forwarded asks by reading the relevant transcript **locally**. |
| Broker | `broker` | The rendezvous. Holds the live registry and routes asks out / gathers answers. Local + loopback by default; bind a reachable address + token to connect machines. |
| MCP | `mcp` | stdio JSON-RPC server Claude Code launches. Turns `peers`/`ask_peer`/`ask_peers` tool calls into broker queries; hands raw context back to the calling session to reason over. |
| Init | `init` / `uninstall` | Wires the hooks into `~/.claude/settings.json` and registers the MCP server (user scope). Idempotent; backs up settings first. |

## Wire protocol

JSON over WebSocket text frames. Two client roles connect to the broker:

- **peers** (daemons): `Hello` → `Register`/`Heartbeat`/`Deregister`; receive `AskRequest`,
  reply `AskResponse`.
- **queriers** (MCP server / CLI): `Hello` → `Query { Peers | Ask | AskAll }`; receive
  `Peers` / `Answers`.

Fan-out (`AskAll`, or an `Ask` matching several peers) is tracked by a per-query collector
in the broker: each forwarded ask gets an internal id, responses are gathered, and the
querier gets a single `Answers` once all peers reply **or** a 12s timeout fires (whichever
first). Transcript paths never enter the protocol — only presence and resulting context do.

## Answer modes: `pull` and `live` coexist

`PeerInfo.mode` is a **per-window** property, set at launch, mixed freely within one mesh:

- **`pull`** (default, shipped): the answering daemon reads that session's transcript and
  returns a recent slice + keyword-relevant lines. Always available. Non-intrusive — the
  peer's live window is untouched.
- **`live`** (opt-in, experimental): the window is launched through a thin PTY wrapper
  (`cmesh`, which `exec`s the real `claude` and owns its pty) and advertises `mode=live`.
  An incoming ask is injected into the running session; its actual reply is captured from
  the pty/transcript and returned. The peer answers for real and may run read-only tools —
  still on the subscription, no `claude -p`, no extra billing.

The seam is already in place: the PTY wrapper just sets `CLAUDE_MESH_MODE=live` in the child
env, which the hook records into the session file, which the daemon advertises as the peer's
mode. The asker (or the answering daemon) selects the mechanism per peer, and **`live` falls
back to `pull`** on timeout or a missing pty, so `live` is strictly an upgrade layered on top
— never a separate, incompatible track. Users choose by how they launch each window:
`claude` → pull, `cmesh` → live.

### How `live` works (implemented, experimental)

1. `cmesh` allocates a pty, spawns the real `claude` inside it, and proxies stdin/stdout
   transparently (tracking terminal resize). It sets `CLAUDE_MESH_MODE=live` and
   `CLAUDE_MESH_CTL=<unix socket>` in the child env; the hook records both into the session
   file, so the daemon learns the window is live and where to reach it.
2. On an `AskRequest` for a live session, the daemon connects to that socket and forwards the
   question. `cmesh` injects it into the pty (the text + a carriage return to submit) and then
   **captures the reply from the transcript** — not by scraping the TUI — waiting for a new
   assistant message followed by ~1.5s of quiescence.
3. Guards: if the session looks mid-turn (transcript changed in the last 2s) `cmesh` declines
   rather than barge in; a 50s cap bounds the wait. Any decline/timeout/error makes the daemon
   fall back to `pull`, so `live` is strictly an upgrade.
4. **Caveat (why experimental):** injection takes a real turn in that window — it appears in
   the conversation and consumes its context — and submit relies on a carriage return being
   interpreted as Enter by Claude's TUI. Verify in your setup before relying on it; `pull`
   stays the safe default.

## Security model

- **Read-only answering.** No mutating tools on the answer path; a peer's question cannot
  change your files.
- **Prompt-injection aware.** Incoming questions are framed to Claude as peer-reported data,
  not instructions (and the tool descriptions say so).
- **Auth.** `CLAUDE_MESH_TOKEN` gates joining; the default broker binds `127.0.0.1` only.
- **Privacy.** Only presence leaves a machine by default; full context travels only when a
  peer is actually asked, and only from that one session, read on its own host.
- **Liveness / phantom reaping.** Every live window runs an MCP server that drops a per-cwd
  liveness beacon (`~/.claude-mesh/alive/<pid>.beacon`); Claude keeps it alive for the whole
  session and kills it on exit. The daemon reaps session files whose cwd has no live beacon
  and whose transcript has gone cold, so a window that died without firing `SessionEnd` stops
  showing as a peer. A live session always has a beacon in its own cwd, so this never reaps a
  live one. (Caveat: a phantom that shares a cwd with another live window lingers until that
  window also closes.)
