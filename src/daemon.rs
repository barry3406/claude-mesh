//! The per-machine resident. Holds one outbound WebSocket to the broker (so a
//! remote box only needs to reach *out*), mirrors the local session registry as
//! presence, and answers incoming asks by reading the relevant transcript here.

use crate::config;
use crate::protocol::*;
use crate::transcript;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

type Tx = mpsc::UnboundedSender<Message>;

fn cli(m: &ClientMsg) -> Message {
    Message::Text(serde_json::to_string(m).expect("serialize ClientMsg"))
}

pub async fn run() -> anyhow::Result<()> {
    // Singleton guard: hold a loopback port for our whole lifetime. A second
    // daemon fails to bind and exits, so concurrent hooks can't double-start us.
    let _guard = match std::net::TcpListener::bind(("127.0.0.1", config::DAEMON_LOCK_PORT)) {
        Ok(l) => l,
        Err(_) => {
            eprintln!("[daemon] already running — exiting");
            return Ok(());
        }
    };
    eprintln!("[daemon] started (host {})", config::hostname());

    crate::client::ensure_broker().await;
    loop {
        if let Err(e) = session(&_guard).await {
            eprintln!("[daemon] broker link dropped: {e}; retrying in 3s");
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }
}

async fn session(_guard: &std::net::TcpListener) -> anyhow::Result<()> {
    let url = config::broker_url();
    let (ws, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
    let (mut sink, mut read) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });

    tx.send(cli(&ClientMsg::Hello {
        role: "peer".into(),
        token: config::token(),
    }))?;

    let mut known: HashMap<String, PeerInfo> = HashMap::new();
    sync_sessions(&tx, &mut known);

    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3));
    loop {
        tokio::select! {
            _ = ticker.tick() => sync_sessions(&tx, &mut known),
            msg = read.next() => {
                let Some(msg) = msg else { return Err(anyhow::anyhow!("stream ended")); };
                let txt = match msg? {
                    Message::Text(t) => t,
                    Message::Close(_) => return Err(anyhow::anyhow!("closed")),
                    _ => continue,
                };
                if let Ok(ServerMsg::AskRequest {
                    request_id,
                    question,
                    session_id,
                    from,
                }) = serde_json::from_str::<ServerMsg>(&txt)
                {
                    let tx2 = tx.clone();
                    // Answer off the read loop: reading a transcript is blocking IO.
                    tokio::task::spawn_blocking(move || {
                        let context = answer(&session_id, &question, &from);
                        let _ = tx2.send(cli(&ClientMsg::AskResponse { request_id, context }));
                    });
                }
            }
        }
    }
}

/// Diff the sessions dir against what the broker knows and emit register/heartbeat/deregister.
fn sync_sessions(tx: &Tx, known: &mut HashMap<String, PeerInfo>) {
    let host = config::hostname();
    let alive = alive_cwds();
    let mut current: HashMap<String, PeerInfo> = HashMap::new();

    if let Ok(rd) = std::fs::read_dir(config::sessions_dir()) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(sf) = serde_json::from_str::<SessionFile>(&raw) else {
                continue;
            };
            // Reap a phantom: a window that died without firing SessionEnd leaves a
            // stale session file. If nothing live is in its cwd (no MCP beacon) and
            // its transcript has gone cold, drop it so it stops showing as a peer.
            if !alive.contains(&sf.cwd) && transcript_idle(&sf) {
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(path.with_extension("state"));
                continue;
            }
            let id = format!("{host}:{}", sf.session_id);
            // The task label is the (immutable) first user message: compute it
            // once and freeze it. Only re-read while it's still empty — e.g. the
            // transcript had no user message yet when the session first registered.
            let task = match known.get(&id) {
                Some(prev) if !prev.task.is_empty() => prev.task.clone(),
                _ if sf.transcript_path.is_empty() => String::new(),
                _ => transcript::derive_task(&sf.transcript_path),
            };
            let st: StateFile = std::fs::read_to_string(path.with_extension("state"))
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            let mut state = if st.state.is_empty() {
                "idle".to_string()
            } else {
                st.state
            };
            // A "waiting" whose transcript kept advancing is stale (the window
            // resumed after you handled it) — show it as working again, so the
            // board doesn't keep flagging "needs you" once you've responded.
            if state == "waiting" {
                let advanced = resolve_transcript(&sf)
                    .and_then(|tp| std::fs::metadata(tp).and_then(|m| m.modified()).ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() > st.since + 2)
                    .unwrap_or(false);
                if advanced {
                    state = "working".to_string();
                }
            }
            let files = if config::enabled("collision") {
                resolve_transcript(&sf)
                    .map(|tp| transcript::recent_edits(&tp, 5))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            current.insert(
                id.clone(),
                PeerInfo {
                    id,
                    name: sf.name,
                    host: host.clone(),
                    cwd: sf.cwd,
                    task,
                    session_id: sf.session_id,
                    mode: sf.mode,
                    state,
                    state_since: st.since,
                    files,
                },
            );
        }
    }

    // Re-register on any change (task or attention state); the broker upserts.
    for (id, info) in &current {
        if known.get(id) != Some(info) {
            let _ = tx.send(cli(&ClientMsg::Register { peer: info.clone() }));
        }
    }
    for id in known.keys().filter(|k| !current.contains_key(*k)) {
        let _ = tx.send(cli(&ClientMsg::Deregister { id: id.clone() }));
    }

    *known = current;
}

/// Answer a forwarded ask. A live (cmesh-wrapped) session is injected with the
/// question and we read back its real reply; on any hiccup — busy, no control
/// socket, timeout — we fall back to the pull path (relevant earlier messages +
/// recent context). Pull is always available, so live is strictly an upgrade.
fn answer(session_id: &str, question: &str, from: &str) -> String {
    let Some(sf) = find_session(session_id) else {
        return "(this session is no longer live on its host)".to_string();
    };
    if sf.mode == "live" && !sf.ctl.is_empty() && config::enabled("live") {
        if let Some(ans) = try_live(&sf.ctl, &frame(from, question)) {
            return ans;
        }
    }
    pull_answer(&sf, question)
}

/// Identity-aware framing for a live injection: the answering Claude must know it
/// is a peer relaying its user's question (not its own user) and stay read-only.
fn frame(from: &str, question: &str) -> String {
    format!(
        "[via claude-mesh] You're being asked by another Claude Code session (\"{from}\"), \
         relaying on behalf of its user — not by your own user. It thinks your current work is \
         related. Please answer briefly and stay read-only (don't edit files or run mutating \
         commands), then carry on with what you were doing.\n\nIts question:\n{question}"
    )
}

fn pull_answer(sf: &SessionFile, question: &str) -> String {
    let Some(tp) = resolve_transcript(sf) else {
        return "(transcript not found on this host)".to_string();
    };
    let mut out = String::new();
    let rel = transcript::relevant_lines(&tp, question, 4);
    if !rel.is_empty() {
        out.push_str("Possibly relevant earlier in this session:\n");
        for r in rel {
            out.push_str(&format!("• {r}\n"));
        }
        out.push('\n');
    }
    out.push_str("Most recent conversation:\n");
    out.push_str(&transcript::read_context(&tp, 24, config::max_chars()));
    out
}

/// Ask a live window over its control socket and wait for the captured reply.
/// Returns None to signal "fall back to pull".
fn try_live(ctl: &str, question: &str) -> Option<String> {
    use std::io::{Read, Write};
    let mut s = std::os::unix::net::UnixStream::connect(ctl).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(58))).ok()?;
    let req = serde_json::json!({ "question": question }).to_string();
    s.write_all(req.as_bytes()).ok()?;
    s.write_all(b"\n").ok()?;
    s.flush().ok()?;
    let mut resp = String::new();
    s.read_to_string(&mut resp).ok()?;
    let v: serde_json::Value = serde_json::from_str(resp.trim()).ok()?;
    v.get("answer")
        .and_then(|a| a.as_str())
        .map(|a| format!("(answered live)\n{a}"))
}

fn find_session(session_id: &str) -> Option<SessionFile> {
    for entry in std::fs::read_dir(config::sessions_dir()).ok()?.flatten() {
        let Ok(raw) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(sf) = serde_json::from_str::<SessionFile>(&raw) else {
            continue;
        };
        if sf.session_id == session_id {
            return Some(sf);
        }
    }
    None
}

/// The session's transcript path, falling back to Claude Code's default layout.
fn resolve_transcript(sf: &SessionFile) -> Option<String> {
    if !sf.transcript_path.is_empty() && std::path::Path::new(&sf.transcript_path).exists() {
        return Some(sf.transcript_path.clone());
    }
    let fallback = config::default_transcript(&sf.cwd, &sf.session_id);
    std::path::Path::new(&fallback).exists().then_some(fallback)
}

/// cwds that currently host a live MCP server (its per-session beacon). A live
/// session always has one in its own cwd, so this never reaps a live session.
fn alive_cwds() -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let Ok(rd) = std::fs::read_dir(config::alive_dir()) else {
        return set;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let pid: Option<i32> = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse().ok());
        match pid {
            Some(pid) if pid_alive(pid) => {
                if let Ok(cwd) = std::fs::read_to_string(&path) {
                    set.insert(cwd.trim().to_string());
                }
            }
            _ => {
                let _ = std::fs::remove_file(&path); // reap a dead/garbage beacon
            }
        }
    }
    set
}

#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    // EPERM means the process exists but we may not signal it.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(_pid: i32) -> bool {
    true
}

/// True when a session's transcript is gone or hasn't changed in a while — paired
/// with a dead cwd to confirm a session file is a phantom (not just idle-but-live,
/// which the cwd beacon already protects).
fn transcript_idle(sf: &SessionFile) -> bool {
    let Some(tp) = resolve_transcript(sf) else {
        return true;
    };
    match std::fs::metadata(&tp).and_then(|m| m.modified()) {
        Ok(mtime) => mtime
            .elapsed()
            .map(|e| e > Duration::from_secs(600))
            .unwrap_or(false),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sf(cwd: &str, transcript: &str) -> SessionFile {
        SessionFile {
            session_id: "s".into(),
            name: "n".into(),
            cwd: cwd.into(),
            transcript_path: transcript.into(),
            mode: "pull".into(),
            ctl: String::new(),
        }
    }

    #[test]
    fn pid_liveness() {
        assert!(pid_alive(std::process::id() as i32)); // we're alive
        assert!(!pid_alive(2_000_000_000)); // not a real pid
        assert!(!pid_alive(-1));
    }

    #[test]
    fn idle_only_when_transcript_cold_or_gone() {
        let dir = std::env::temp_dir().join("claude-mesh-daemon-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let tp = dir.join("fresh.jsonl");
        std::fs::write(&tp, "{}\n").unwrap();
        // just-written transcript: a live (even idle) session must NOT look idle
        assert!(!transcript_idle(&sf("/x", tp.to_str().unwrap())));
        // no transcript at all: treated as dead
        assert!(transcript_idle(&sf("/x", "/no/such/transcript.jsonl")));
    }
}
