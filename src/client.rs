//! TCP client — bridges async networking with the Ratatui event loop via channels.

use anyhow::{Context, Result};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use tokio::io::split;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::message::{
    parse_slash_command, read_frame, write_frame, ClientMessage, ServerMessage,
};
use crate::ui::{draw, map_event, poll_event, InputAction, UiState};

/// Connect to `server_addr`, run TUI until the user quits or disconnects.
pub async fn run(server_addr: &str, username: String) -> Result<()> {
    let stream = TcpStream::connect(server_addr)
        .await
        .with_context(|| format!("connect to {server_addr}"))?;
    info!("connected to {server_addr} as {username}");

    let (mut reader, mut writer) = split(stream);

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

    // Network reader: TCP → channel → UI state.
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

    // Network writer: UI channel → TCP.
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
    state.connection_status = "Connected".into();
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