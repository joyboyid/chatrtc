//! bandit-X — terminal chat with Tokio + Ratatui
//!
//! ```bash
//! # Server (default port 8080) — accepts raw TCP and WebSocket
//! cargo run -- --server
//! cargo run -- --server --port 9000
//!
//! # Client
//! cargo run -- --client --addr 127.0.0.1:8080 --username alice
//! cargo run -- --client -a 127.0.0.1:8080 -u bob
//!
//! # Client through a reverse proxy / CDN (HTTP-only), e.g. Cloudflare
//! cargo run -- --client --ws --addr wss://chat.example.com/chat -u bob
//! ```

mod client;
mod message;
mod p2p;
mod server;
mod ui;
mod ws;

use anyhow::{bail, Result};
use clap::{ArgGroup, Parser};
use tracing::info;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Parser, Debug)]
#[command(
    name = "bandit-x",
    version,
    about = "bandit-X — terminal chat (server & client)",
    group = ArgGroup::new("mode").required(true).args(&["server", "client", "peer"])
)]
struct Cli {
    /// Run as chat server.
    #[arg(long, group = "mode")]
    server: bool,

    /// Run as chat client.
    #[arg(long, group = "mode")]
    client: bool,

    /// Run as peer-to-peer mesh node (no central server).
    #[arg(long, group = "mode")]
    peer: bool,

    /// P2P: listen address for inbound peer connections.
    #[arg(long, default_value = "0.0.0.0:9090")]
    listen: String,

    /// P2P: seed peer address(es) to dial (comma-separated, repeatable).
    #[arg(long, value_delimiter = ',')]
    connect: Vec<String>,

    /// TCP port (server bind, or client fallback if --addr has no port).
    #[arg(long, short, default_value_t = 8080)]
    port: u16,

    /// Server bind address (server mode only).
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// Server address to connect to (client mode).
    #[arg(long, short, default_value = "127.0.0.1:8080")]
    addr: String,

    /// Username (client mode).
    #[arg(long, short)]
    username: Option<String>,

    /// Connect over WebSocket instead of raw TCP (client mode).
    /// Required when the server sits behind a proxy/CDN that only speaks HTTP.
    #[arg(long)]
    ws: bool,
}

fn init_logging() -> Result<()> {
    let file_appender = RollingFileAppender::new(Rotation::DAILY, "logs", "bandit-x.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    // Leak guard so the writer lives for the whole process.
    std::mem::forget(_guard);

    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false);

    let stdout_layer = fmt::layer().with_writer(std::io::stderr);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(file_layer)
        .with(stdout_layer)
        .init();

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    std::fs::create_dir_all("logs").ok();
    init_logging()?;

    let cli = Cli::parse();

    if cli.server {
        let bind = format!("{}:{}", cli.host, cli.port);
        info!("starting server on {bind}");
        server::run(&bind).await?;
    } else if cli.client {
        let username = cli.username.unwrap_or_else(|| {
            format!("user_{}", &uuid::Uuid::new_v4().to_string()[..8])
        });
        client::run(&cli.addr, username, cli.ws, cli.port).await?;
    } else if cli.peer {
        let username = cli.username.unwrap_or_else(|| {
            format!("user_{}", &uuid::Uuid::new_v4().to_string()[..8])
        });
        info!(
            "starting p2p node '{username}' listen={}",
            cli.listen
        );
        p2p::run(&username, &cli.listen, &cli.connect).await?;
    } else {
        bail!("specify --server or --client");
    }

    Ok(())
}