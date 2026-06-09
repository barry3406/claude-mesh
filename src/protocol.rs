//! Wire protocol shared between the broker, the per-machine daemon, and queriers
//! (the MCP server / CLI). Everything is JSON over a WebSocket text frame.

use serde::{Deserialize, Serialize};

/// Presence record for one live Claude Code session. This is the only thing that
/// leaves a machine by default — never the transcript itself. Transcript content
/// is read locally by the owning daemon and only travels as an on-demand answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerInfo {
    /// Globally unique: "<host>:<session_id>".
    pub id: String,
    /// Human label, e.g. "claude-mesh" (the repo dir basename) or a user override.
    pub name: String,
    pub host: String,
    pub cwd: String,
    /// Short, derived from the session's first real user message.
    pub task: String,
    pub session_id: String,
    /// How this window answers asks: "pull" (read its transcript) or "live"
    /// (inject into the running session via the PTY wrapper). Both coexist in one
    /// mesh — `mode` is per-window, chosen at launch, with "pull" as the fallback.
    #[serde(default = "default_mode")]
    pub mode: String,
}

pub fn default_mode() -> String {
    "pull".to_string()
}

/// On-disk record the SessionStart hook drops per live session (in
/// ~/.claude-mesh/sessions/). Read by the daemon (presence + answering) and by
/// cmesh (to find its own transcript). `ctl` is the live-mode control socket path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionFile {
    pub session_id: String,
    pub name: String,
    pub cwd: String,
    #[serde(default)]
    pub transcript_path: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub ctl: String,
}

/// One peer's reply to an ask: a slice of its live context.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerAnswer {
    pub name: String,
    pub host: String,
    pub cwd: String,
    pub context: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum QueryKind {
    /// Who's online?
    Peers,
    /// Ask one peer (matched by name/id substring). `from` identifies the asker.
    Ask {
        target: String,
        question: String,
        from: String,
    },
    /// Broadcast to everyone (optionally skipping the asker's own cwd).
    AskAll {
        question: String,
        exclude_cwd: Option<String>,
        from: String,
    },
}

/// daemon/querier -> broker
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum ClientMsg {
    Hello {
        role: String,
        token: String,
    },
    Register {
        peer: PeerInfo,
    },
    Deregister {
        id: String,
    },
    Heartbeat {
        id: String,
        task: String,
    },
    /// A peer's reply to a forwarded AskRequest. `request_id` echoes the broker's id.
    AskResponse {
        request_id: u64,
        context: String,
    },
    /// A querier asks the broker something. `request_id` is the querier's own id.
    Query {
        request_id: u64,
        kind: QueryKind,
    },
}

/// broker -> daemon/querier
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum ServerMsg {
    Welcome,
    Error {
        msg: String,
    },
    /// Forwarded to a peer's daemon; it should answer with AskResponse.
    AskRequest {
        request_id: u64,
        from: String,
        question: String,
        session_id: String,
    },
    /// Result of a Peers query (request_id echoes the querier's id).
    Peers {
        request_id: u64,
        peers: Vec<PeerInfo>,
    },
    /// Result of an Ask/AskAll query.
    Answers {
        request_id: u64,
        answers: Vec<PeerAnswer>,
    },
}
