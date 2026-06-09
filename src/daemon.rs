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
            let id = format!("{host}:{}", sf.session_id);
            // The task label is the (immutable) first user message: compute it
            // once and freeze it. Only re-read while it's still empty — e.g. the
            // transcript had no user message yet when the session first registered.
            let task = match known.get(&id) {
                Some(prev) if !prev.task.is_empty() => prev.task.clone(),
                _ if sf.transcript_path.is_empty() => String::new(),
                _ => transcript::derive_task(&sf.transcript_path),
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
                },
            );
        }
    }

    for (id, info) in &current {
        match known.get(id) {
            None => {
                let _ = tx.send(cli(&ClientMsg::Register { peer: info.clone() }));
            }
            Some(prev) if prev.task != info.task => {
                let _ = tx.send(cli(&ClientMsg::Heartbeat {
                    id: id.clone(),
                    task: info.task.clone(),
                }));
            }
            _ => {}
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
    if sf.mode == "live" && !sf.ctl.is_empty() {
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
