//! Chat client — bridges async networking with the Ratatui event loop via channels.
//!
//! Speaks the same wire protocol as the server over either raw TCP (`--client`)
//! or WebSocket (`--client --ws`). The WebSocket mode exists for servers that
//! are published through a reverse proxy or CDN which only speaks HTTP.

use anyhow::{bail, Context, Result};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use tokio::io::{split, AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::message::{
    parse_slash_command, read_frame, write_frame, ClientMessage, ServerMessage,
};
use crate::ui::{draw, map_event, poll_event, InputAction, UiState};
use crate::ws::ClientWs;

/// Connect to `server_addr` and run the TUI until the user quits.
pub async fn run(
    server_addr: &str,
    username: String,
    use_ws: bool,
    default_port: u16,
) -> Result<()> {
    if use_ws {
        let url = build_ws_url(server_addr, default_port)?;
        info!("connecting to {url} as {username} (websocket)");
        let (stream, response) = tokio_tungstenite::connect_async(&url)
            .await
            .with_context(|| format!("websocket connect to {url}"))?;
        info!("websocket handshake complete ({})", response.status());
        run_session(split(ClientWs::new(stream)), username, "Connected (WebSocket)").await
    } else {
        let addr = build_tcp_addr(server_addr, default_port)?;
        info!("connecting to {addr} as {username}");
        let stream = TcpStream::connect(&addr)
            .await
            .with_context(|| format!("connect to {addr}"))?;
        run_session(split(stream), username, "Connected").await
    }
}

/// Shared session body for both transports: send `Join`, then bridge the
/// socket and the TUI with channels.
async fn run_session<R, W>(
    (mut reader, mut writer): (R, W),
    username: String,
    status: &str,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // Register username with the server.
    write_frame(
        &mut writer,
        &ClientMessage::Join {
            username: username.clone(),
        },
    )
    .await?;

    // Channels bridge the network tasks and the TUI loop:
    //   net → ui: incoming ServerMessages
    //   ui → net: outgoing ClientMessages
    let (net_to_ui_tx, mut net_to_ui_rx) = mpsc::channel::<ServerMessage>(256);
    let (ui_to_net_tx, mut ui_to_net_rx) = mpsc::channel::<ClientMessage>(64);

    // Network reader: transport → channel → UI state.
    let reader_handle = tokio::spawn(async move {
        loop {
            match read_frame::<_, ServerMessage>(&mut reader).await {
                Ok(msg) => {
                    if net_to_ui_tx.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Network writer: UI channel → transport.
    let writer_handle = tokio::spawn(async move {
        while let Some(msg) = ui_to_net_rx.recv().await {
            if write_frame(&mut writer, &msg).await.is_err() {
                break;
            }
            if matches!(msg, ClientMessage::Quit) {
                break;
            }
        }
    });

    // --- TUI setup ---
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = UiState::new(username);
    state.connection_status = status.into();
    state.push_server_message(ServerMessage::System {
        content: "Welcome! Commands: /name <user>, /msg @user <text>, /quit".into(),
        timestamp: chrono::Utc::now(),
    });

    let ui_result = run_ui_loop(
        &mut terminal,
        &mut state,
        &ui_to_net_tx,
        &mut net_to_ui_rx,
    )
    .await;

    // Graceful shutdown.
    let _ = ui_to_net_tx.send(ClientMessage::Quit).await;
    reader_handle.abort();
    writer_handle.abort();

    disable_raw_mode().context("disable raw mode")?;
    crossterm::execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    ui_result
}

/// Split `host[:port]`, tolerating bracketed IPv6 literals.
fn split_host_port(input: &str) -> (&str, Option<u16>) {
    let input = input.trim();
    if let Some(rest) = input.strip_prefix('[') {
        if let Some((host, tail)) = rest.split_once(']') {
            return (host, tail.strip_prefix(':').and_then(|p| p.parse().ok()));
        }
        return (input, None);
    }
    match input.rsplit_once(':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(p) => (host, Some(p)),
            // Trailing junk after a colon — treat the whole thing as a host.
            Err(_) => (input, None),
        },
        None => (input, None),
    }
}

/// Re-add brackets around IPv6 literals so they can carry a port.
fn format_host(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// Normalize `--addr` into a dialable `host:port` for raw TCP mode.
fn build_tcp_addr(addr: &str, default_port: u16) -> Result<String> {
    let (scheme, rest) = match addr.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => (String::new(), addr),
    };
    if scheme == "ws" || scheme == "wss" {
        bail!("'{addr}' is a websocket address — add --ws, or drop the ws:// prefix");
    }
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (host, port) = split_host_port(host_port);
    if host.is_empty() {
        bail!("server address '{addr}' has no host");
    }
    if !path.is_empty() && path != "/" {
        bail!("path '{path}' is only supported in --ws mode");
    }
    Ok(format!("{}:{}", format_host(host), port.unwrap_or(default_port)))
}

/// Normalize `--addr` into a full `ws://`/`wss://` URL for WebSocket mode.
///
/// Accepts a bare host, `host:port`, or a full URL with an optional path
/// (nginx routes on the path), and defaults the port from `--port` or 443 for
/// `wss://`. A path is always present because the handshake request requires it.
fn build_ws_url(addr: &str, default_port: u16) -> Result<String> {
    let (scheme, rest) = match addr.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => ("ws".to_string(), addr),
    };
    if scheme != "ws" && scheme != "wss" {
        bail!("unsupported scheme '{scheme}' — use ws:// or wss://");
    }
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (host, port) = split_host_port(host_port);
    if host.is_empty() {
        bail!("websocket address '{addr}' has no host");
    }
    // `wss://` implies the standard HTTPS port; otherwise fall back to --port.
    let port = port.unwrap_or(if scheme == "wss" { 443 } else { default_port });
    let path = if path.is_empty() { "/" } else { path };
    Ok(format!("{scheme}://{}:{port}{path}", format_host(host)))
}

async fn run_ui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut UiState,
    ui_to_net: &mpsc::Sender<ClientMessage>,
    net_to_ui: &mut mpsc::Receiver<ServerMessage>,
) -> Result<()> {
    loop {
        // Drain all pending network messages before rendering.
        while let Ok(msg) = net_to_ui.try_recv() {
            state.push_server_message(msg);
        }

        terminal.draw(|f| draw(f, state))?;

        tokio::select! {
            maybe_msg = net_to_ui.recv() => {
                match maybe_msg {
                    Some(msg) => state.push_server_message(msg),
                    None => {
                        state.connection_status = "Disconnected".into();
                        state.push_server_message(ServerMessage::System {
                            content: "Disconnected from server".into(),
                            timestamp: chrono::Utc::now(),
                        });
                        break;
                    }
                }
            }
            maybe_event = poll_event() => {
                match maybe_event {
                    Ok(Some(event)) => {
                        let action = map_event(event, state);
                        match handle_input_action(action, state, ui_to_net).await? {
                            LoopControl::Continue => {}
                            LoopControl::Quit => break,
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        error!("terminal event error: {e}");
                        break;
                    }
                }
            }
        }

        if state.should_quit {
            break;
        }
    }
    Ok(())
}

enum LoopControl {
    Continue,
    Quit,
}

async fn handle_input_action(
    action: InputAction,
    state: &mut UiState,
    ui_to_net: &mpsc::Sender<ClientMessage>,
) -> Result<LoopControl> {
    match action {
        InputAction::None => Ok(LoopControl::Continue),
        InputAction::Quit => {
            state.should_quit = true;
            Ok(LoopControl::Quit)
        }
        InputAction::Submit(text) => {
            if let Some(cmd) = parse_slash_command(&text) {
                match &cmd {
                    ClientMessage::ChangeName { new_name } => {
                        state.my_username = new_name.clone();
                    }
                    ClientMessage::Quit => {
                        state.should_quit = true;
                    }
                    _ => {}
                }
                ui_to_net.send(cmd).await.ok();
                if state.should_quit {
                    return Ok(LoopControl::Quit);
                }
            } else {
                ui_to_net
                    .send(ClientMessage::Chat { content: text })
                    .await
                    .ok();
            }
            Ok(LoopControl::Continue)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::{build_tcp_addr, build_ws_url, split_host_port};

    #[test]
    fn host_and_port_are_split() {
        assert_eq!(split_host_port("example.com:8080"), ("example.com", Some(8080)));
        assert_eq!(split_host_port("example.com"), ("example.com", None));
        assert_eq!(split_host_port("127.0.0.1:9000"), ("127.0.0.1", Some(9000)));
        assert_eq!(split_host_port("[::1]:8080"), ("::1", Some(8080)));
        assert_eq!(split_host_port("[2001:db8::1]"), ("2001:db8::1", None));
        // Trailing junk is not a port — keep it as part of the host.
        assert_eq!(split_host_port("example.com:abc"), ("example.com:abc", None));
    }

    #[test]
    fn tcp_addresses_default_the_port() {
        assert_eq!(build_tcp_addr("127.0.0.1", 8080).unwrap(), "127.0.0.1:8080");
        assert_eq!(build_tcp_addr("127.0.0.1:9000", 8080).unwrap(), "127.0.0.1:9000");
        // Trailing slash is harmless, a real path is not.
        assert_eq!(build_tcp_addr("example.com/", 8080).unwrap(), "example.com:8080");
        assert!(build_tcp_addr("example.com/chat", 8080).is_err());
        // IPv6 keeps its brackets once a port is appended.
        assert_eq!(build_tcp_addr("[::1]", 8080).unwrap(), "[::1]:8080");
        assert_eq!(build_tcp_addr("[::1]:7000", 8080).unwrap(), "[::1]:7000");
    }

    #[test]
    fn tcp_mode_rejects_a_websocket_address() {
        let err = build_tcp_addr("wss://example.com/chat", 8080).unwrap_err();
        assert!(err.to_string().contains("--ws"), "unhelpful error: {err}");
    }

    #[test]
    fn websocket_urls_always_carry_a_scheme_port_and_path() {
        // Bare host falls back to the --port default.
        assert_eq!(
            build_ws_url("example.com", 8080).unwrap(),
            "ws://example.com:8080/"
        );
        assert_eq!(
            build_ws_url("example.com:9000/chat", 8080).unwrap(),
            "ws://example.com:9000/chat"
        );
        // wss implies 443 unless a port is given — this is the Cloudflare case.
        assert_eq!(
            build_ws_url("wss://orojackson.xcodehoster.id/chat", 8080).unwrap(),
            "wss://orojackson.xcodehoster.id:443/chat"
        );
        assert_eq!(
            build_ws_url("wss://example.com", 8080).unwrap(),
            "wss://example.com:443/"
        );
        // Explicit ports win over the default.
        assert_eq!(
            build_ws_url("ws://example.com:8081", 8080).unwrap(),
            "ws://example.com:8081/"
        );
        // IPv6 literals must stay bracketed.
        assert_eq!(build_ws_url("[::1]:8080", 8080).unwrap(), "ws://[::1]:8080/");
    }

    #[test]
    fn websocket_urls_reject_bad_input() {
        assert!(build_ws_url("http://example.com", 8080).is_err());
        assert!(build_ws_url("ws://", 8080).is_err());
        assert!(build_ws_url("wss:///chat", 8080).is_err());
    }
}
