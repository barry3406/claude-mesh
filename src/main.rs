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

    let needs = peers.iter().filter(|p| p.state == "waiting").count();
    let mut out = format!("FLEET — {} window(s), {needs} need you\n\n", peers.len());
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
}
