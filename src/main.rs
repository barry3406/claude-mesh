//! claude-mesh — ask across your Claude Code windows, local and remote.
//!
//! One binary, several roles (selected by subcommand): the `init` wiring, the
//! `broker` rendezvous, the per-machine `daemon`, the `mcp` stdio server Claude
//! Code calls, and the `hook` entrypoints. `peers`/`ask` are CLI shims for testing.

mod broker;
mod client;
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
    /// Ask a peer from the CLI; use the name "all" to broadcast.
    Ask { name: String, question: Vec<String> },
}

#[derive(Subcommand)]
enum HookCmd {
    SessionStart,
    SessionEnd,
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
        },
        Cmd::Peers => cli_peers().await,
        Cmd::Ask { name, question } => cli_ask(name, question.join(" ")).await,
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
    let kind = if name.eq_ignore_ascii_case("all") {
        QueryKind::AskAll {
            question,
            exclude_cwd: None,
        }
    } else {
        QueryKind::Ask {
            target: name,
            question,
        }
    };
    match client::query(kind).await {
        Ok(ServerMsg::Answers { answers, .. }) => println!("{}", mcp::format_answers(&answers)),
        Ok(_) => println!("unexpected response"),
        Err(e) => println!("error: {e}"),
    }
}
