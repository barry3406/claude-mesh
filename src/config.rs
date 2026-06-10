//! Paths, identity, and broker-address resolution. All knobs are env vars so the
//! single-machine default needs zero configuration and the cross-machine case is
//! one `CLAUDE_MESH_BROKER=...` line.

use std::path::{Path, PathBuf};

pub const DEFAULT_BROKER_URL: &str = "ws://127.0.0.1:47800";
pub const DAEMON_LOCK_PORT: u16 = 47801;

pub fn base_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude-mesh")
}

pub fn sessions_dir() -> PathBuf {
    base_dir().join("sessions")
}

/// Liveness beacons — one file per live MCP server (`<pid>.beacon` → its cwd) —
/// used by the daemon to reap session files whose window has died.
pub fn alive_dir() -> PathBuf {
    base_dir().join("alive")
}

pub fn log_file() -> PathBuf {
    base_dir().join("mesh.log")
}

/// Where queriers and daemons connect.
pub fn broker_url() -> String {
    std::env::var("CLAUDE_MESH_BROKER").unwrap_or_else(|_| DEFAULT_BROKER_URL.to_string())
}

/// True when the broker is on this machine, so we may auto-spawn it.
pub fn broker_is_local() -> bool {
    let u = broker_url();
    u.contains("127.0.0.1") || u.contains("localhost")
}

/// host:port the local broker binds (and that liveness checks dial).
pub fn broker_tcp_addr() -> String {
    let u = broker_url();
    let u = u.trim_start_matches("wss://").trim_start_matches("ws://");
    u.split('/').next().unwrap_or(u).to_string()
}

/// Bind address for `broker` (defaults to the local tcp addr; override to expose).
pub fn broker_bind_addr() -> String {
    std::env::var("CLAUDE_MESH_BIND").unwrap_or_else(|_| broker_tcp_addr())
}

pub fn token() -> String {
    std::env::var("CLAUDE_MESH_TOKEN").unwrap_or_default()
}

/// Answer mode for this window: "pull" (default) or "live". The PTY wrapper sets
/// CLAUDE_MESH_MODE=live in the child env; a plain `claude` launch leaves it pull.
pub fn mode() -> String {
    std::env::var("CLAUDE_MESH_MODE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "pull".to_string())
}

/// Max characters of recent-context returned per peer answer (the token-budget
/// knob, especially for broadcasts). Default 5000.
pub fn max_chars() -> usize {
    std::env::var("CLAUDE_MESH_MAX_CHARS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(5000)
}

/// Live-mode control socket path for this window (set by the `cmesh` wrapper).
pub fn ctl() -> String {
    std::env::var("CLAUDE_MESH_CTL").unwrap_or_default()
}

pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Command run on an attention event (a window needs you). Receives the message
/// via $MESH_MSG / $MESH_WINDOW / $MESH_STATE. Empty = no push.
pub fn notify_cmd() -> String {
    std::env::var("CLAUDE_MESH_NOTIFY_CMD").unwrap_or_default()
}

/// Also push when a window goes idle (finished a turn), not just when it's blocked.
pub fn push_idle() -> bool {
    matches!(
        std::env::var("CLAUDE_MESH_PUSH_IDLE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

// ---- feature toggles -------------------------------------------------------
// Every optional capability is on by default and can be turned off via the
// `feature` command (config file) or CLAUDE_MESH_DISABLE=a,b for one shell.

pub const FEATURES: &[&str] = &["ask", "fleet", "push", "live", "collision"];

pub fn config_file() -> PathBuf {
    base_dir().join("config.json")
}

fn disabled_set() -> std::collections::HashSet<String> {
    let mut s = std::collections::HashSet::new();
    if let Ok(v) = std::env::var("CLAUDE_MESH_DISABLE") {
        s.extend(
            v.split(',')
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty()),
        );
    }
    if let Ok(raw) = std::fs::read_to_string(config_file()) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(arr) = v.get("disabled").and_then(|x| x.as_array()) {
                s.extend(arr.iter().filter_map(|f| f.as_str().map(String::from)));
            }
        }
    }
    s
}

/// Is a feature on? On by default; off if listed in config "disabled" or env.
pub fn enabled(feature: &str) -> bool {
    !disabled_set().contains(feature)
}

/// Persist a feature on/off into the config file.
pub fn set_feature(feature: &str, on: bool) -> std::io::Result<()> {
    let mut disabled: Vec<String> = std::fs::read_to_string(config_file())
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("disabled").and_then(|x| x.as_array()).cloned())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| f.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    disabled.retain(|f| f != feature);
    if !on {
        disabled.push(feature.to_string());
    }
    let _ = std::fs::create_dir_all(base_dir());
    std::fs::write(
        config_file(),
        serde_json::to_string_pretty(&serde_json::json!({ "disabled": disabled }))
            .unwrap_or_default(),
    )
}

pub fn hostname() -> String {
    if let Ok(h) = std::env::var("CLAUDE_MESH_HOST") {
        if !h.is_empty() {
            return h;
        }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().split('.').next().unwrap_or("host").to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "host".to_string())
}

/// Human label for a session: the repo/dir basename, or CLAUDE_MESH_NAME override.
pub fn derive_name(cwd: &str) -> String {
    if let Ok(n) = std::env::var("CLAUDE_MESH_NAME") {
        if !n.is_empty() {
            return n;
        }
    }
    Path::new(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "session".to_string())
}

/// Reconstruct the transcript path Claude Code uses when a hook doesn't hand us one:
/// ~/.claude/projects/<cwd-with-slashes-as-dashes>/<session_id>.jsonl
pub fn default_transcript(cwd: &str, session_id: &str) -> String {
    let enc: String = cwd
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect();
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("projects")
        .join(enc)
        .join(format!("{}.jsonl", session_id))
        .to_string_lossy()
        .to_string()
}
