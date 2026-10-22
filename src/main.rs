use std::io::IsTerminal as _;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use vigilant_vole::{agent, config::Config, mcp_bridge, server};

#[derive(Parser)]
#[command(
    name = "vigilant-vole",
    version,
    about = "Self-hosted uptime monitoring"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the web server (default)
    Serve,
    /// Run as a remote probe agent reporting to a server
    Agent(agent::AgentArgs),
    /// Bridge an MCP client that only speaks stdio to the server's /mcp endpoint
    Mcp(mcp_bridge::McpArgs),
}

#[tokio::main]
async fn main() {
    // The shared kit needs to know which product it runs in before anything else.
    vigilant_vole::product::install();
    let cli = Cli::parse();
    // Colours only on a terminal (logs go to stdout): `docker logs` and log
    // files get plain text. The `mcp` bridge keeps stdout for protocol
    // messages, so its logs go to stderr.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if matches!(cli.command, Some(Command::Mcp(_))) {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(std::io::stdout().is_terminal())
            .init();
    }

    // Every TLS stack in the binary uses rustls; pin its crypto provider so a
    // dependency enabling a second provider can't make rustls panic at runtime.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => server::run(Config::from_env()).await,
        Command::Mcp(args) => {
            if let Err(err) = mcp_bridge::run(args).await {
                eprintln!("vigilant-vole mcp: {err}");
                std::process::exit(1);
            }
        }
        Command::Agent(args) => {
            if let Err(err) = agent::run(args).await {
                tracing::error!(%err, "agent could not run; exiting");
                std::process::exit(1);
            }
        }
    }
}
