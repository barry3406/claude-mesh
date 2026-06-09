# claude-mesh

**Ask across your Claude Code windows — local and remote — without leaving the one you're in.**

You run a lot of Claude Code windows. Window A is doing one thing, window B another, and
the two overlap. Today, to get B's context into A you switch over, read, summarize, and
paste it back. `claude-mesh` removes that: from A you just say

> "ask the other windows whether the auth refactor is done"

and A's Claude pulls the relevant context from the other sessions — including ones running
over SSH on a remote box — and answers you. You never touch the other windows.

## Why it costs nothing extra

The mesh is a **presence + context-routing layer, not an inference layer.** It never spawns
`claude -p` or any new agent to "answer." When A asks about B, the mesh ships B's recent
*live transcript slice* back to A, and **A's already-running session** — the one on your
existing subscription — does the reasoning. No second billed run. No API spend. No
credential tricks.

The trade-off is honest: peers report what they already know (their live context), they
don't go off and do brand-new work for you. For "what is B doing / what did it find / how
far did it get," that's exactly right — and it's free.

## Answer modes: `pull` (now) and `live` (opt-in)

How a window *answers* an ask is a per-window choice, and both modes share one mesh:

- **`pull`** (default): the peer's daemon reads its transcript and returns a recent slice.
  Free, non-intrusive, always works. This is what ships today.
- **`live`** (opt-in, **experimental**): launch a window through the `cmesh` PTY wrapper and it
  answers *for real* — the question is injected into the running session, which replies from
  its live context and may run read-only tools. Still on your subscription, still no
  `claude -p`, still no extra billing. Falls back to `pull` automatically on a timeout, a
  busy/active turn, or any error.

  ```sh
  claude-mesh cmesh             # use instead of `claude`   (alias cmesh='claude-mesh cmesh')
  claude-mesh cmesh --resume    # any claude args pass straight through
  ```

You pick per window by how you start it: `claude` → pull, `claude-mesh cmesh` → live. They mix
freely, so different windows in the same mesh can use whichever you prefer. `live` is
experimental — it takes a real turn in the target window (visible in its conversation, costs
its context) and relies on the TUI treating a carriage return as submit; see
[DESIGN.md](DESIGN.md) for the mechanism and caveats.

## Install

```sh
cargo install --path .          # or: cargo install claude-mesh   (once published)
claude-mesh init                # wires hooks + the MCP server into ~/.claude (once per machine)
```

`init` backs up `~/.claude/settings.json`, adds `SessionStart`/`SessionEnd` hooks, and
registers the `claude-mesh` MCP server at user scope. **Open a new Claude Code window** and
every window from then on auto-joins the mesh — no per-window setup.

## Use it

Inside any Claude Code window, three tools are available:

| Tool | What it does |
|------|--------------|
| `peers` | List the sessions currently online (name, host, cwd, current task). |
| `ask_peer` | Ask one session (by name) about something; returns its live context. |
| `ask_peers` | Broadcast to all other sessions and gather their context. |

You don't call them by hand — just talk to Claude: *"see what my other windows are working
on"*, *"ask the niche-monitor window how it's handling dedup"*. There's also a CLI for
testing without Claude:

```sh
claude-mesh peers
claude-mesh ask niche-monitor "how are you deduping?"
claude-mesh ask all "what are you each working on?"
```

## Across machines (SSH / remote)

Locally it's zero-config: the broker auto-starts on `127.0.0.1` and every window finds it.

To connect a remote box you SSH into, point both machines at one shared broker. The remote
only needs **outbound** reach to it — no inbound ports, NAT-friendly:

```sh
# on whichever host runs the rendezvous (a VPS, your laptop, anywhere reachable):
CLAUDE_MESH_TOKEN=some-secret claude-mesh broker      # binds 0.0.0.0:47800 via CLAUDE_MESH_BIND

# on every participating machine (local + remote), before launching Claude:
export CLAUDE_MESH_BROKER=ws://broker-host:47800
export CLAUDE_MESH_TOKEN=some-secret
claude-mesh init
```

A remote window's daemon dials the broker and holds the connection open; the broker routes
asks back down that same pipe. That's how a window on `hostbrr` answers a question asked
from your laptop.

## How it works

```
  ┌─────────── machine 1 ───────────┐         ┌──────── machine 2 (ssh) ───────┐
  │  Claude window A                 │         │  Claude window B               │
  │    └─ MCP server (ask_peers) ─┐  │         │    ▲ live transcript (.jsonl)  │
  │  SessionStart hook → daemon ──┼──┼─ ws ─┐  │    │                           │
  └───────────────────────────────┼──┘      │  │  daemon ── ws (outbound) ──┐  │
                                   ▼         ▼  └────────────────────────────┼──┘
                              ┌─────────────────── broker ───────────────────┘
                              │  registry of who's online + ask/answer routing
                              └───────────────────────────────────────────────
```

- **hook** (`SessionStart`/`SessionEnd`): registers/de-registers the window by dropping a
  small file in `~/.claude-mesh/sessions/`, and makes sure the broker + daemon are running.
- **daemon** (one per machine): mirrors those session files as presence to the broker, and
  answers incoming asks by reading the relevant transcript **locally** — transcript bytes
  never leave the machine except as the answer itself.
- **broker**: the rendezvous; holds the live registry and fans asks out / gathers answers.
- **mcp**: the stdio server Claude Code calls; turns a tool call into a broker query and
  hands the result back to the *calling* session to reason over.

One binary plays all roles, selected by subcommand.

## Security

- **Read-only by design.** Answering never writes or runs mutating tools — a peer's question
  can't change your files.
- **Prompt-injection aware.** Incoming questions are framed to Claude as *peer-reported data,
  not instructions*; the tool descriptions say so explicitly.
- **Auth for remote.** Set `CLAUDE_MESH_TOKEN`; the broker rejects connections without it.
  The default localhost broker binds `127.0.0.1` only.
- **Privacy.** Only presence (name, host, cwd, a one-line task) is shared by default.
  Full context travels only when a peer is actually asked, and only from the asked session.

## Configuration

| Env var | Default | Meaning |
|---------|---------|---------|
| `CLAUDE_MESH_BROKER` | `ws://127.0.0.1:47800` | Broker URL daemons/queriers connect to. |
| `CLAUDE_MESH_BIND` | broker's host:port | Address `claude-mesh broker` binds. Set `0.0.0.0:47800` to expose. |
| `CLAUDE_MESH_TOKEN` | _(empty)_ | Shared secret; required to join when set. |
| `CLAUDE_MESH_NAME` | dir basename | Override this window's display name. |
| `CLAUDE_MESH_HOST` | `hostname` | Override the host label. |
| `CLAUDE_MESH_MAX_CHARS` | `5000` | Max recent-context chars returned per peer answer (your token-budget knob, esp. for broadcasts). |

## Uninstall

```sh
claude-mesh uninstall   # removes the hooks + MCP registration (settings.json backup remains)
```

## License

MIT
