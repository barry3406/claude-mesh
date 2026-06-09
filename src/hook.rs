//! Entrypoints Claude Code invokes on session lifecycle. These must be fast,
//! print NOTHING to stdout (it would pollute the session), and never fail the
//! session — every error is swallowed.
//!
//! Hook stdin is a JSON object with at least { session_id, transcript_path, cwd }.

use crate::config;
use crate::util;
use serde_json::Value;
use std::io::Read;

fn read_stdin() -> Value {
    let mut s = String::new();
    let _ = std::io::stdin().read_to_string(&mut s);
    serde_json::from_str(&s).unwrap_or(Value::Null)
}

fn field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn session_start() {
    let v = read_stdin();
    let session_id = field(&v, "session_id");
    if session_id.is_empty() {
        return;
    }
    let cwd = {
        let c = field(&v, "cwd");
        if c.is_empty() {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            c
        }
    };
    let transcript = {
        let t = field(&v, "transcript_path");
        if t.is_empty() {
            config::default_transcript(&cwd, &session_id)
        } else {
            t
        }
    };

    ensure_running();

    let record = serde_json::json!({
        "session_id": session_id,
        "name": config::derive_name(&cwd),
        "cwd": cwd,
        "transcript_path": transcript,
        "mode": config::mode(),
    });
    let dir = config::sessions_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        dir.join(format!("{}.json", sanitize(&session_id))),
        serde_json::to_string_pretty(&record).unwrap_or_default(),
    );
}

pub fn session_end() {
    let v = read_stdin();
    let session_id = field(&v, "session_id");
    if session_id.is_empty() {
        return;
    }
    let _ = std::fs::remove_file(
        config::sessions_dir().join(format!("{}.json", sanitize(&session_id))),
    );
}

/// Make sure a local broker (if applicable) and the daemon are up. Liveness is a
/// plain TCP connect against each one's port; if it refuses, we spawn it detached.
fn ensure_running() {
    if config::broker_is_local()
        && std::net::TcpStream::connect(config::broker_tcp_addr()).is_err()
    {
        util::spawn_detached(&["broker"]);
    }
    if std::net::TcpStream::connect(("127.0.0.1", config::DAEMON_LOCK_PORT)).is_err() {
        util::spawn_detached(&["daemon"]);
    }
}
