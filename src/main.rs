//! bandit-X — terminal chat with Tokio + Ratatui
//!
//! ```bash
//! # Server (default port 8080)
//! cargo run -- --server
//! cargo run -- --server --port 9000
//!
//! # Client
//! cargo run -- --client --addr 127.0.0.1:8080 --username alice
//! cargo run -- --client -a 127.0.0.1:8080 -u bob
//! ```

mod client;
mod message;
mod server;
mod ui;

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
    group = ArgGroup::new("mode").required(true).args(&["server", "client"])
)]
struct Cli {
    /// Run as chat server.
    #[arg(long, group = "mode")]
    server: bool,

    /// Run as chat client.
    #[arg(long, group = "mode")]
    client: bool,

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
        let addr = if cli.addr.contains(':') {
            cli.addr
        } else {
            format!("{}:{}", cli.addr, cli.port)
        };
        info!("connecting to {addr} as {username}");
        client::run(&addr, username).await?;
    } else {
        bail!("specify --server or --client");
    }

    Ok(())
}