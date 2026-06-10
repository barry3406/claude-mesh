//! claude-mesh — ask across your Claude Code windows, local and remote.
//!
//! One binary, several roles (selected by subcommand): the `init` wiring, the
//! `broker` rendezvous, the per-machine `daemon`, the `mcp` stdio server Claude
//! Code calls, and the `hook` entrypoints. `peers`/`ask` are CLI shims for testing.

mod broker;
mod client;
mod cmesh;
mod config;
mod daemon;
mod hook;
mod init;
mod mcp;
mod protocol;
mod transcript;
mod util;

use clap::{Parser, Subcommand};
use protocol::{QueryKind, ServerMsg};

#[derive(Parser)]
#[command(
    name = "claude-mesh",
    version,
    about = "Ask across your Claude Code windows — local and remote."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Wire up hooks + MCP server (run once per machine).
    Init,
    /// Remove the hooks + MCP registration.
    Uninstall,
    /// Run the rendezvous broker (auto-started locally; run by hand for a shared/remote one).
    Broker,
    /// Run the per-machine presence/answer daemon (auto-started).
    Daemon,
    /// Run the MCP server over stdio (Claude Code launches this).
    Mcp,
    /// Hook entrypoints invoked by Claude Code.
    Hook {
        #[command(subcommand)]
        which: HookCmd,
    },
    /// List online peers (same data the `peers` tool returns).
    Peers,
    /// Attention dashboard: which windows need you, are working, or are idle.
    Fleet {
        /// Refresh continuously.
        #[arg(short, long)]
        watch: bool,
    },
    /// Show windows currently editing the same file.
    Collisions,
    /// Show or toggle features: `feature` lists, `feature collision off` toggles.
    Feature { args: Vec<String> },
    /// Ask a peer from the CLI; use the name "all" to broadcast.
    Ask { name: String, question: Vec<String> },
    /// Launch `claude` wrapped for live cross-window answers (sets mode=live).
    Cmesh {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum HookCmd {
    SessionStart,
    SessionEnd,
    Notification,
    Stop,
    Prompt,
}

#[tokio::main]
async fn main() {
    match Cli::parse().cmd {
        Cmd::Init => {
            if let Err(e) = init::run() {
                eprintln!("init error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Uninstall => {
            if let Err(e) = init::uninstall() {
                eprintln!("uninstall error: {e}");
            }
        }
        Cmd::Broker => {
            if let Err(e) = broker::run().await {
                eprintln!("broker error: {e}");
            }
        }
        Cmd::Daemon => {
            if let Err(e) = daemon::run().await {
                eprintln!("daemon error: {e}");
            }
        }
        Cmd::Mcp => {
            if let Err(e) = mcp::run().await {
                eprintln!("mcp error: {e}");
            }
        }
        Cmd::Hook { which } => match which {
            HookCmd::SessionStart => hook::session_start(),
            HookCmd::SessionEnd => hook::session_end(),
            HookCmd::Notification => hook::notification(),
            HookCmd::Stop => hook::stop(),
            HookCmd::Prompt => hook::prompt(),
        },
        Cmd::Peers => cli_peers().await,
        Cmd::Fleet { watch } => cli_fleet(watch).await,
        Cmd::Collisions => cli_collisions().await,
        Cmd::Feature { args } => cmd_feature(args),
        Cmd::Ask { name, question } => cli_ask(name, question.join(" ")).await,
        Cmd::Cmesh { args } => {
            if let Err(e) = cmesh::run(args) {
                eprintln!("cmesh error: {e}");
                std::process::exit(1);
            }
        }
    }
}

async fn cli_peers() {
    match client::query(QueryKind::Peers).await {
        Ok(ServerMsg::Peers { peers, .. }) => println!("{}", mcp::format_peers(&peers)),
        Ok(_) => println!("unexpected response"),
        Err(e) => println!("error: {e}\n(no broker? start one with `claude-mesh broker`)"),
    }
}

async fn cli_ask(name: String, question: String) {
    let from = format!("cli@{}", config::hostname());
    let kind = if name.eq_ignore_ascii_case("all") {
        QueryKind::AskAll {
            question,
            exclude_cwd: None,
            from,
        }
    } else {
        QueryKind::Ask {
            target: name,
            question,
            from,
        }
    };
    match client::query(kind).await {
        Ok(ServerMsg::Answers { answers, .. }) => println!("{}", mcp::format_answers(&answers)),
        Ok(_) => println!("unexpected response"),
        Err(e) => println!("error: {e}"),
    }
}

async fn cli_fleet(watch: bool) {
    loop {
        match client::query(QueryKind::Peers).await {
            Ok(ServerMsg::Peers { peers, .. }) => {
                if watch {
                    print!("\x1b[2J\x1b[H"); // clear + cursor home
                }
                println!("{}", format_fleet(&peers, config::now_epoch()));
            }
            Ok(_) => println!("unexpected response"),
            Err(e) => println!(
                "error: {e}\n(no broker? open a Claude window or run `claude-mesh broker`)"
            ),
        }
        if !watch {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

fn fmt_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

fn format_fleet(peers: &[protocol::PeerInfo], now: u64) -> String {
    if peers.is_empty() {
        return "No Claude Code windows online.".to_string();
    }
    let rank = |s: &str| match s {
        "waiting" => 0,
        "working" => 1,
        _ => 2,
    };
    let mut ps: Vec<&protocol::PeerInfo> = peers.iter().collect();
    ps.sort_by(|a, b| {
        rank(&a.state)
            .cmp(&rank(&b.state))
            .then(a.name.cmp(&b.name))
    });

    let mut out = String::new();
    let cols = collisions(peers);
    if !cols.is_empty() {
        out.push_str(&format!(
            "\x1b[31m⚠ {} file collision(s)\x1b[0m\n",
            cols.len()
        ));
        for (file, who) in &cols {
            out.push_str(&format!("  {file}  ← {}\n", who.join(", ")));
        }
        out.push('\n');
    }
    let needs = peers.iter().filter(|p| p.state == "waiting").count();
    out.push_str(&format!(
        "FLEET — {} window(s), {needs} need you\n\n",
        peers.len()
    ));
    for p in ps {
        let (color, sym, label) = match p.state.as_str() {
            "waiting" => ("\x1b[31m", "●", "needs you"),
            "working" => ("\x1b[33m", "◐", "working"),
            _ => ("\x1b[2m", "○", "idle"),
        };
        let who = format!("{} @ {}", p.name, p.host);
        let age = if p.state_since > 0 && now >= p.state_since {
            fmt_age(now - p.state_since)
        } else {
            String::new()
        };
        let live = if p.mode == "live" { " ⟨live⟩" } else { "" };
        out.push_str(&format!(
            "{color}{sym} {label:<9}\x1b[0m {who:<26}{live} {age:>4}  {}\n",
            p.task
        ));
    }
    out
}

/// Files edited by 2+ windows on the same host → (file, [window names]).
fn collisions(peers: &[protocol::PeerInfo]) -> Vec<(String, Vec<String>)> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut map: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for p in peers {
        for f in &p.files {
            map.entry((p.host.clone(), f.clone()))
                .or_default()
                .insert(p.name.clone());
        }
    }
    map.into_iter()
        .filter(|(_, names)| names.len() >= 2)
        .map(|((_host, file), names)| (file, names.into_iter().collect()))
        .collect()
}

async fn cli_collisions() {
    match client::query(QueryKind::Peers).await {
        Ok(ServerMsg::Peers { peers, .. }) => {
            let cols = collisions(&peers);
            if cols.is_empty() {
                println!("No file collisions — no two windows are editing the same file.");
            } else {
                println!("{} file collision(s):", cols.len());
                for (file, who) in cols {
                    println!("  {file}  ← {}", who.join(", "));
                }
            }
        }
        Ok(_) => println!("unexpected response"),
        Err(e) => println!("error: {e}"),
    }
}

fn cmd_feature(args: Vec<String>) {
    if args.is_empty() {
        for f in config::FEATURES {
            println!(
                "{:<11} {}",
                f,
                if config::enabled(f) { "on" } else { "off" }
            );
        }
        return;
    }
    let name = args[0].as_str();
    if !config::FEATURES.contains(&name) {
        eprintln!(
            "unknown feature '{name}'. known: {}",
            config::FEATURES.join(", ")
        );
        std::process::exit(1);
    }
    let on = match args.get(1).map(|s| s.as_str()) {
        Some("on") => true,
        Some("off") => false,
        _ => {
            eprintln!("usage: claude-mesh feature <name> on|off");
            std::process::exit(1);
        }
    };
    match config::set_feature(name, on) {
        Ok(()) => println!("{name} = {}", if on { "on" } else { "off" }),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(name: &str, state: &str) -> protocol::PeerInfo {
        protocol::PeerInfo {
            id: name.into(),
            name: name.into(),
            host: "h".into(),
            cwd: "/c".into(),
            task: "t".into(),
            session_id: "s".into(),
            mode: "pull".into(),
            state: state.into(),
            state_since: 0,
            files: vec![],
        }
    }

    #[test]
    fn age_format() {
        assert_eq!(fmt_age(5), "5s");
        assert_eq!(fmt_age(120), "2m");
        assert_eq!(fmt_age(7200), "2h");
    }

    #[test]
    fn fleet_orders_needs_you_first() {
        let peers = vec![
            peer("z-idle", "idle"),
            peer("a-work", "working"),
            peer("m-wait", "waiting"),
        ];
        let out = format_fleet(&peers, 100);
        let w = out.find("m-wait").unwrap();
        let k = out.find("a-work").unwrap();
        let i = out.find("z-idle").unwrap();
        assert!(w < k && k < i, "waiting, then working, then idle");
        assert!(out.contains("1 need you"));
    }

    #[test]
    fn detects_same_file_collision() {
        let mut a = peer("a", "working");
        a.files = vec!["/x/auth.py".into()];
        let mut b = peer("b", "working");
        b.files = vec!["/x/auth.py".into(), "/x/only-b.py".into()];
        let c = collisions(&[a, b]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, "/x/auth.py");
        assert_eq!(c[0].1, vec!["a".to_string(), "b".to_string()]);
    }
}
