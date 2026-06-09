//! Querier side: a one-shot connection that asks the broker something and waits
//! for the single matching result. Used by the MCP server and the test CLI.

use crate::config;
use crate::protocol::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

fn cli(m: &ClientMsg) -> Message {
    Message::Text(serde_json::to_string(m).expect("serialize ClientMsg"))
}

/// If the broker is local and not up yet, start it and wait until it accepts.
pub async fn ensure_broker() {
    if !config::broker_is_local() {
        return;
    }
    let addr = config::broker_tcp_addr();
    if tokio::net::TcpStream::connect(&addr).await.is_ok() {
        return;
    }
    crate::util::spawn_detached(&["broker"]);
    for _ in 0..40 {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Send one query, return the broker's matching Peers/Answers response.
pub async fn query(kind: QueryKind) -> anyhow::Result<ServerMsg> {
    ensure_broker().await;
    let url = config::broker_url();
    let (ws, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
    let (mut sink, mut read) = ws.split();

    sink.send(cli(&ClientMsg::Hello {
        role: "querier".into(),
        token: config::token(),
    }))
    .await?;
    sink.send(cli(&ClientMsg::Query {
        request_id: 1,
        kind,
    }))
    .await?;

    while let Some(msg) = read.next().await {
        let txt = match msg? {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        match serde_json::from_str::<ServerMsg>(&txt)? {
            ServerMsg::Welcome => continue,
            ServerMsg::Error { msg } => return Err(anyhow::anyhow!(msg)),
            other => return Ok(other),
        }
    }
    Err(anyhow::anyhow!("broker closed the connection before answering"))
}
