//! The per-machine resident. Holds one outbound WebSocket to the broker (so a
//! remote box only needs to reach *out*), mirrors the local session registry as
//! presence, and answers incoming asks by reading the relevant transcript here.

use crate::config;
use crate::protocol::*;
use crate::transcript;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

type Tx = mpsc::UnboundedSender<Message>;

/// On-disk record dropped by the SessionStart hook (one file per live session).
#[derive(Deserialize)]
struct SessionFile {
    session_id: String,
    name: String,
    cwd: String,
    #[serde(default)]
    transcript_path: String,
    #[serde(default = "crate::protocol::default_mode")]
    mode: String,
}

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
                if let Ok(ServerMsg::AskRequest { request_id, question, session_id, .. }) =
                    serde_json::from_str::<ServerMsg>(&txt)
                {
                    let tx2 = tx.clone();
                    // Answer off the read loop: reading a transcript is blocking IO.
                    tokio::task::spawn_blocking(move || {
                        let context = answer(&session_id, &question);
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
            let task = if sf.transcript_path.is_empty() {
                String::new()
            } else {
                transcript::derive_task(&sf.transcript_path)
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

/// Build an answer for a forwarded ask: relevant earlier messages + recent context.
fn answer(session_id: &str, question: &str) -> String {
    let Some(tp) = find_transcript(session_id) else {
        return "(this session is no longer live on its host)".to_string();
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
    out.push_str(&transcript::read_context(&tp, 24, 5000));
    out
}

/// Locate the transcript for a session by reading its session file (then falling
/// back to Claude Code's default project path layout).
fn find_transcript(session_id: &str) -> Option<String> {
    if let Ok(rd) = std::fs::read_dir(config::sessions_dir()) {
        for entry in rd.flatten() {
            let Ok(raw) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(sf) = serde_json::from_str::<SessionFile>(&raw) else {
                continue;
            };
            if sf.session_id == session_id {
                if !sf.transcript_path.is_empty() && std::path::Path::new(&sf.transcript_path).exists()
                {
                    return Some(sf.transcript_path);
                }
                let fallback = config::default_transcript(&sf.cwd, session_id);
                if std::path::Path::new(&fallback).exists() {
                    return Some(fallback);
                }
            }
        }
    }
    None
}
