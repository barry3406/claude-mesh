//! One-shot setup: wire the SessionStart/SessionEnd hooks into ~/.claude/settings.json
//! and register the MCP server with Claude Code — so every new window auto-joins the
//! mesh with no per-window action. Idempotent; backs up settings.json first.

use serde_json::{json, Value};
use std::path::PathBuf;

fn settings_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("settings.json")
}

fn exe() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "claude-mesh".into())
}

/// Set hooks[event] to our single command, dropping any prior claude-mesh entry
/// while preserving unrelated hooks the user already has.
fn set_hook(hooks: &mut Value, event: &str, command: &str) {
    let entry = hooks
        .as_object_mut()
        .unwrap()
        .entry(event)
        .or_insert_with(|| json!([]));
    if !entry.is_array() {
        *entry = json!([]);
    }
    let arr = entry.as_array_mut().unwrap();
    arr.retain(|grp| !grp.to_string().contains("claude-mesh"));
    arr.push(json!({"hooks": [{"type": "command", "command": command}]}));
}

pub fn run() -> anyhow::Result<()> {
    let exe = exe();
    let path = settings_path();

    let mut settings: Value = if path.exists() {
        let backup = path.with_file_name("settings.json.mesh-bak");
        std::fs::copy(&path, &backup).ok();
        println!("✓ backed up settings.json → {}", backup.display());
        serde_json::from_str(&std::fs::read_to_string(&path)?).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if !settings.is_object() {
        settings = json!({});
    }

    {
        let hooks = settings
            .as_object_mut()
            .unwrap()
            .entry("hooks")
            .or_insert_with(|| json!({}));
        if !hooks.is_object() {
            *hooks = json!({});
        }
        set_hook(hooks, "SessionStart", &format!("{exe} hook session-start"));
        set_hook(hooks, "SessionEnd", &format!("{exe} hook session-end"));
        set_hook(hooks, "UserPromptSubmit", &format!("{exe} hook prompt"));
        set_hook(hooks, "Stop", &format!("{exe} hook stop"));
        set_hook(hooks, "Notification", &format!("{exe} hook notification"));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, serde_json::to_string_pretty(&settings)?)?;
    println!("✓ wired SessionStart/SessionEnd hooks → {}", path.display());

    register_mcp(&exe);

    println!(
        "\nDone. Open a NEW Claude Code window (so the hooks fire) and ask it:\n  \
         \"use the peers tool to see other windows\"\n\n\
         Cross-machine: on each box run `claude-mesh init`, then point them at a shared\n\
         broker with  export CLAUDE_MESH_BROKER=ws://<host>:47800  and a matching\n\
         export CLAUDE_MESH_TOKEN=<secret>  (also set on the machine running `claude-mesh broker`)."
    );
    Ok(())
}

fn register_mcp(exe: &str) {
    let status = std::process::Command::new("claude")
        .args([
            "mcp",
            "add",
            "claude-mesh",
            "--scope",
            "user",
            "--",
            exe,
            "mcp",
        ])
        .status();
    match status {
        Ok(s) if s.success() => println!("✓ registered MCP server 'claude-mesh' (user scope)"),
        _ => println!(
            "! couldn't run `claude mcp add`. Register it manually:\n    \
             claude mcp add claude-mesh --scope user -- {exe} mcp"
        ),
    }
}

pub fn uninstall() -> anyhow::Result<()> {
    let path = settings_path();
    if path.exists() {
        let mut settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&path)?).unwrap_or_else(|_| json!({}));
        if let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
            for event in [
                "SessionStart",
                "SessionEnd",
                "UserPromptSubmit",
                "Stop",
                "Notification",
            ] {
                if let Some(arr) = hooks.get_mut(event).and_then(|e| e.as_array_mut()) {
                    arr.retain(|grp| !grp.to_string().contains("claude-mesh"));
                }
            }
        }
        std::fs::write(&path, serde_json::to_string_pretty(&settings)?)?;
        println!("✓ removed claude-mesh hooks from {}", path.display());
    }
    let _ = std::process::Command::new("claude")
        .args(["mcp", "remove", "claude-mesh", "--scope", "user"])
        .status();
    println!("✓ unregistered MCP server (if it was present)");
    println!(
        "Running broker/daemon, if any, will exit on their own once idle, or kill them manually."
    );
    Ok(())
}
