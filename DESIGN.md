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
- **`live`** (opt-in, milestone 2): the window is launched through a thin PTY wrapper
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

### Milestone 2 sketch (live)

1. `cmesh` subcommand: allocate a pty, `exec` `claude` as the child, pump user↔child IO
   transparently, and expose a control channel for injection.
2. Daemon, on an `AskRequest` for a `live` session, sends the framed question to that
   window's control channel instead of reading the transcript; waits (bounded) for the
   reply delimiter; returns the captured text.
3. Inject with a clear, non-executable framing ("a peer asks: …; answer, don't act") and
   keep answering read-only.
4. Timeout / no-pty → fall back to the `pull` path.

## Security model

- **Read-only answering.** No mutating tools on the answer path; a peer's question cannot
  change your files.
- **Prompt-injection aware.** Incoming questions are framed to Claude as peer-reported data,
  not instructions (and the tool descriptions say so).
- **Auth.** `CLAUDE_MESH_TOKEN` gates joining; the default broker binds `127.0.0.1` only.
- **Privacy.** Only presence leaves a machine by default; full context travels only when a
  peer is actually asked, and only from that one session, read on its own host.
