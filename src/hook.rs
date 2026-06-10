//! Entrypoints Claude Code invokes on session lifecycle + attention events. These
//! must be fast, print NOTHING to stdout (it would pollute the session), and never
//! fail the session — every error is swallowed.
//!
//! Hook stdin is a JSON object with at least { session_id, transcript_path, cwd };
//! the Notification hook also carries { message }.

use crate::config;
use crate::protocol::StateFile;
use crate::util;
use serde_json::Value;
use std::io::Read;
use std::process::Stdio;

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

fn cwd_of(v: &Value) -> String {
    let c = field(v, "cwd");
    if c.is_empty() {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        c
    }
}

// ---- session lifecycle -----------------------------------------------------

pub fn session_start() {
    let v = read_stdin();
    let session_id = field(&v, "session_id");
    if session_id.is_empty() {
        return;
    }
    let cwd = cwd_of(&v);
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
        "ctl": config::ctl(),
    });
    let dir = config::sessions_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        dir.join(format!("{}.json", sanitize(&session_id))),
        serde_json::to_string_pretty(&record).unwrap_or_default(),
    );
    set_state(&session_id, "idle", "");
}

pub fn session_end() {
    let v = read_stdin();
    let session_id = field(&v, "session_id");
    if session_id.is_empty() {
        return;
    }
    let stem = sanitize(&session_id);
    let dir = config::sessions_dir();
    let _ = std::fs::remove_file(dir.join(format!("{stem}.json")));
    let _ = std::fs::remove_file(dir.join(format!("{stem}.state")));
}

// ---- attention events ------------------------------------------------------

/// UserPromptSubmit: the user kicked off a turn → working.
pub fn prompt() {
    let v = read_stdin();
    let id = field(&v, "session_id");
    if id.is_empty() || !config::enabled("fleet") {
        return;
    }
    set_state(&id, "working", "");
}

/// Stop: the agent finished a turn → idle (ball back in the user's court).
pub fn stop() {
    let v = read_stdin();
    let id = field(&v, "session_id");
    if id.is_empty() {
        return;
    }
    if config::enabled("fleet") {
        set_state(&id, "idle", "");
    }
    if config::push_idle() {
        push(&config::derive_name(&cwd_of(&v)), "idle", "finished a turn");
    }
}

/// Notification: Claude needs the user (permission / waiting) → waiting + push.
pub fn notification() {
    let v = read_stdin();
    let id = field(&v, "session_id");
    if id.is_empty() {
        return;
    }
    let msg = field(&v, "message");
    if config::enabled("fleet") {
        set_state(&id, "waiting", &msg);
    }
    let window = config::derive_name(&cwd_of(&v));
    let text = if msg.is_empty() { "needs you" } else { &msg };
    push(&window, "waiting", text);
}

// ---- helpers ---------------------------------------------------------------

fn set_state(session_id: &str, state: &str, msg: &str) {
    let dir = config::sessions_dir();
    let _ = std::fs::create_dir_all(&dir);
    let sf = StateFile {
        state: state.to_string(),
        since: config::now_epoch(),
        msg: msg.to_string(),
    };
    let _ = std::fs::write(
        dir.join(format!("{}.state", sanitize(session_id))),
        serde_json::to_string(&sf).unwrap_or_default(),
    );
}

/// Fire the user's notify command (if any), detached. $MESH_WINDOW / $MESH_STATE /
/// $MESH_MSG are exported for the command to use.
fn push(window: &str, state: &str, msg: &str) {
    if !config::enabled("push") {
        return;
    }
    let cmd = config::notify_cmd();
    if cmd.is_empty() {
        return;
    }
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .env("MESH_WINDOW", window)
        .env("MESH_STATE", state)
        .env("MESH_MSG", format!("[{window}] {msg}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Make sure a local broker (if applicable) and the daemon are up. Liveness is a
/// plain TCP connect against each one's port; if it refuses, we spawn it detached.
fn ensure_running() {
    if config::broker_is_local() && std::net::TcpStream::connect(config::broker_tcp_addr()).is_err()
    {
        util::spawn_detached(&["broker"]);
    }
    if std::net::TcpStream::connect(("127.0.0.1", config::DAEMON_LOCK_PORT)).is_err() {
        util::spawn_detached(&["daemon"]);
    }
}
