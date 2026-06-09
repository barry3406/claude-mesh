//! The rendezvous. Holds the live peer registry and routes asks between queriers
//! and the daemons that own each session. Local by default; expose it (bind a
//! reachable address + a shared token) to connect machines across SSH.

use crate::config;
use crate::protocol::*;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

type Tx = mpsc::UnboundedSender<Message>;

fn srv(m: &ServerMsg) -> Message {
    Message::Text(serde_json::to_string(m).expect("serialize ServerMsg"))
}

/// Collects responses for one Ask/AskAll fan-out and forwards them once complete.
struct Collector {
    querier: Tx,
    query_id: u64,
    remaining: Mutex<usize>,
    answers: Mutex<Vec<PeerAnswer>>,
    done: AtomicBool,
    r_ids: Mutex<Vec<u64>>,
}

/// One outstanding AskRequest: which collector it feeds + the target's metadata.
struct Pending {
    collector: Arc<Collector>,
    peer: PeerInfo,
}

#[derive(Default)]
struct State {
    peers: HashMap<String, (PeerInfo, u64)>, // peer id -> (info, conn id)
    conns: HashMap<u64, Tx>,                 // conn id -> writer
    conn_peers: HashMap<u64, Vec<String>>,   // conn id -> peer ids it registered
    collectors: HashMap<u64, Pending>,       // internal request id -> pending ask
}

pub async fn run() -> anyhow::Result<()> {
    let bind = config::broker_bind_addr();
    let listener = match TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            // Port taken almost always means another broker already owns it.
            eprintln!("[broker] bind {bind} failed ({e}); another broker is likely up — exiting");
            return Ok(());
        }
    };
    eprintln!("[broker] listening on {bind}");

    let state = Arc::new(Mutex::new(State::default()));
    let conn_ctr = Arc::new(AtomicU64::new(1));
    let req_ctr = Arc::new(AtomicU64::new(1));

    loop {
        let (stream, _addr) = listener.accept().await?;
        let state = state.clone();
        let req_ctr = req_ctr.clone();
        let conn_id = conn_ctr.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, conn_id, state.clone(), req_ctr).await {
                eprintln!("[broker] conn {conn_id} closed: {e}");
            }
            cleanup_conn(&state, conn_id);
        });
    }
}

async fn handle_conn(
    stream: TcpStream,
    conn_id: u64,
    state: Arc<Mutex<State>>,
    req_ctr: Arc<AtomicU64>,
) -> anyhow::Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut read) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    state.lock().unwrap().conns.insert(conn_id, tx.clone());

    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });

    while let Some(msg) = read.next().await {
        let txt = match msg? {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        match serde_json::from_str::<ClientMsg>(&txt) {
            Ok(cm) => handle_client_msg(cm, conn_id, &state, &tx, &req_ctr),
            Err(e) => {
                let _ = tx.send(srv(&ServerMsg::Error {
                    msg: format!("bad message: {e}"),
                }));
            }
        }
    }

    writer.abort();
    Ok(())
}

fn handle_client_msg(
    cm: ClientMsg,
    conn_id: u64,
    state: &Arc<Mutex<State>>,
    tx: &Tx,
    req_ctr: &Arc<AtomicU64>,
) {
    match cm {
        ClientMsg::Hello { token, .. } => {
            if token != config::token() {
                let _ = tx.send(srv(&ServerMsg::Error {
                    msg: "authentication failed (bad CLAUDE_MESH_TOKEN)".into(),
                }));
            } else {
                let _ = tx.send(srv(&ServerMsg::Welcome));
            }
        }
        ClientMsg::Register { peer } => {
            let mut s = state.lock().unwrap();
            s.peers.insert(peer.id.clone(), (peer.clone(), conn_id));
            s.conn_peers.entry(conn_id).or_default().push(peer.id);
        }
        ClientMsg::Deregister { id } => {
            let mut s = state.lock().unwrap();
            s.peers.remove(&id);
            if let Some(v) = s.conn_peers.get_mut(&conn_id) {
                v.retain(|x| x != &id);
            }
        }
        ClientMsg::Heartbeat { id, task } => {
            let mut s = state.lock().unwrap();
            if let Some((info, _)) = s.peers.get_mut(&id) {
                info.task = task;
            }
        }
        ClientMsg::AskResponse {
            request_id,
            context,
        } => {
            let pending = state.lock().unwrap().collectors.remove(&request_id);
            if let Some(p) = pending {
                p.collector.answers.lock().unwrap().push(PeerAnswer {
                    name: p.peer.name,
                    host: p.peer.host,
                    cwd: p.peer.cwd,
                    context,
                });
                let done_now = {
                    let mut rem = p.collector.remaining.lock().unwrap();
                    *rem = rem.saturating_sub(1);
                    *rem == 0
                };
                if done_now {
                    complete(&p.collector, state);
                }
            }
        }
        ClientMsg::Query { request_id, kind } => handle_query(request_id, kind, state, tx, req_ctr),
    }
}

fn handle_query(
    q: u64,
    kind: QueryKind,
    state: &Arc<Mutex<State>>,
    tx: &Tx,
    req_ctr: &Arc<AtomicU64>,
) {
    match kind {
        QueryKind::Peers => {
            let peers: Vec<PeerInfo> = state
                .lock()
                .unwrap()
                .peers
                .values()
                .map(|(p, _)| p.clone())
                .collect();
            let _ = tx.send(srv(&ServerMsg::Peers {
                request_id: q,
                peers,
            }));
        }
        QueryKind::Ask { target, question } => {
            let want = target.to_lowercase();
            let targets: Vec<PeerInfo> = state
                .lock()
                .unwrap()
                .peers
                .values()
                .filter(|(p, _)| {
                    p.name.to_lowercase().contains(&want) || p.id.to_lowercase().contains(&want)
                })
                .map(|(p, _)| p.clone())
                .collect();
            dispatch(q, question, targets, state, tx, req_ctr);
        }
        QueryKind::AskAll {
            question,
            exclude_cwd,
        } => {
            let targets: Vec<PeerInfo> = state
                .lock()
                .unwrap()
                .peers
                .values()
                .map(|(p, _)| p.clone())
                .filter(|p| exclude_cwd.as_deref() != Some(p.cwd.as_str()))
                .collect();
            dispatch(q, question, targets, state, tx, req_ctr);
        }
    }
}

fn dispatch(
    q: u64,
    question: String,
    targets: Vec<PeerInfo>,
    state: &Arc<Mutex<State>>,
    querier_tx: &Tx,
    req_ctr: &Arc<AtomicU64>,
) {
    if targets.is_empty() {
        let _ = querier_tx.send(srv(&ServerMsg::Answers {
            request_id: q,
            answers: vec![],
        }));
        return;
    }

    let collector = Arc::new(Collector {
        querier: querier_tx.clone(),
        query_id: q,
        remaining: Mutex::new(targets.len()),
        answers: Mutex::new(Vec::new()),
        done: AtomicBool::new(false),
        r_ids: Mutex::new(Vec::new()),
    });

    let mut r_ids = Vec::new();
    {
        let mut s = state.lock().unwrap();
        for peer in &targets {
            let r = req_ctr.fetch_add(1, Ordering::SeqCst);
            r_ids.push(r);
            // Find the peer's connection and forward the ask.
            if let Some((_, cid)) = s.peers.get(&peer.id) {
                if let Some(ptx) = s.conns.get(cid) {
                    let _ = ptx.send(srv(&ServerMsg::AskRequest {
                        request_id: r,
                        from: "another Claude window".into(),
                        question: question.clone(),
                        session_id: peer.session_id.clone(),
                    }));
                }
            }
            s.collectors.insert(
                r,
                Pending {
                    collector: collector.clone(),
                    peer: peer.clone(),
                },
            );
        }
    }
    *collector.r_ids.lock().unwrap() = r_ids;

    // Don't wait forever on a slow or wedged peer.
    let state2 = state.clone();
    let collector2 = collector.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(12)).await;
        complete(&collector2, &state2);
    });
}

/// Send whatever answers we have to the querier, exactly once, and clean up.
fn complete(c: &Arc<Collector>, state: &Arc<Mutex<State>>) {
    if c.done.swap(true, Ordering::SeqCst) {
        return;
    }
    let answers = c.answers.lock().unwrap().clone();
    let _ = c.querier.send(srv(&ServerMsg::Answers {
        request_id: c.query_id,
        answers,
    }));
    let ids = c.r_ids.lock().unwrap().clone();
    let mut s = state.lock().unwrap();
    for r in ids {
        s.collectors.remove(&r);
    }
}

fn cleanup_conn(state: &Arc<Mutex<State>>, conn_id: u64) {
    let mut s = state.lock().unwrap();
    s.conns.remove(&conn_id);
    if let Some(ids) = s.conn_peers.remove(&conn_id) {
        for id in ids {
            s.peers.remove(&id);
        }
    }
}
