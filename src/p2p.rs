//! Peer-to-peer chat mode — each node binds a TCP listener and optionally
//! dials seed peers to form a mesh. Fully independent from the client-server
//! architecture; only reuses the framing helpers and the TUI/state layer.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serde::{Deserialize, Serialize};
use tokio::io::{split, ReadHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::message::{parse_slash_command, read_frame, write_frame, ClientMessage, ServerMessage};
use crate::ui::{draw, map_event, poll_event, InputAction, UiState};

/// Symmetric P2P protocol — one enum used in both directions between nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PeerMessage {
    /// Handshake: name + dialable listen address announcement on connect.
    Hello {
        username: String,
        addr: String,
    },
    Chat {
        username: String,
        content: String,
        timestamp: DateTime<Utc>,
    },
    Private {
        from: String,
        to: String,
        content: String,
        timestamp: DateTime<Utc>,
    },
    NameChange { old_name: String, new_name: String },
    /// System notice (join/leave/etc.) forwarded to every peer.
    Notice { content: String, timestamp: DateTime<Utc> },
    /// Peer address gossip: dialable listen addresses of peers we know.
    /// Used so a fresh node can learn the rest of the mesh from one seed.
    Introduce { addrs: Vec<String> },
    Bye { username: String },
    Error { content: String },
}

/// Per-peer connection state tracked by the local node.
struct PeerEntry {
    username: String,
    /// The peer's advertised dialable listen address (empty if unknown).
    addr: String,
    /// Channel to push outbound messages to this peer's write task.
    outbound: mpsc::Sender<PeerMessage>,
}

/// Shared peer registry — `Arc<RwLock<HashMap<Uuid, PeerEntry>>>`.
type PeerMap = Arc<RwLock<HashMap<Uuid, PeerEntry>>>;

/// Whether a TCP connection was accepted (listener) or dialed (seed connect).
enum Role {
    Inbound,
    Outbound,
}

/// Run a P2P mesh node. The TUI blocks here until the user quits.
pub async fn run(username: &str, listen_addr: &str, connect_addrs: &[String]) -> Result<()> {
    let peers: PeerMap = Arc::new(RwLock::new(HashMap::new()));
    let our_name = Arc::new(RwLock::new(username.to_string()));
    // Addresses we already dial (or have a retry task for) — guards against
    // Introduce storms re-dialing the same peer forever.
    let dialed: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let own_addr = listen_addr.to_string();
    let (net_to_ui_tx, mut net_to_ui_rx) = mpsc::channel::<ServerMessage>(256);
    let (ui_to_net_tx, mut ui_to_net_rx) = mpsc::channel::<ClientMessage>(64);

    // --- Inbound listener ---
    let listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("bind {listen_addr}"))?;
    info!("p2p node '{username}' listening on {listen_addr}");
    let listener_ui = net_to_ui_tx.clone();
    let listener_peers = Arc::clone(&peers);
    let listener_name = Arc::clone(&our_name);
    let listener_dialed = Arc::clone(&dialed);
    let listener_own = own_addr.clone();
    tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    error!("p2p accept error: {e}");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };
            info!("inbound peer from {peer}");
            let peers = Arc::clone(&listener_peers);
            let our_name = Arc::clone(&listener_name);
            let ui = listener_ui.clone();
            let dialed = Arc::clone(&listener_dialed);
            let own = listener_own.clone();
            tokio::spawn(async move {
                let res = handle_connection(
                    Role::Inbound,
                    stream,
                    peers,
                    our_name,
                    ui,
                    &own,
                    dialed,
                )
                .await;
                if let Err(e) = res {
                    warn!("inbound peer ended: {e:#}");
                }
            });
        }
    });

    // --- Outbound dials (seeds); retry indefinitely on failure ---
    for addr in connect_addrs {
        let addr = addr.clone();
        info!("p2p will dial seed {addr}");
        spawn_peer_dial(addr, &peers, &our_name, &net_to_ui_tx, &own_addr, &dialed);
    }

    // --- Outgoing dispatcher: UI → peers ---
    let dispatcher_peers = Arc::clone(&peers);
    let dispatcher_name = Arc::clone(&our_name);
    let dispatcher_ui = net_to_ui_tx.clone();
    tokio::spawn(async move {
        while let Some(msg) = ui_to_net_rx.recv().await {
            if handle_outgoing(msg, &dispatcher_peers, &dispatcher_name, &dispatcher_ui).await {
                break;
            }
        }
    });

    // --- TUI setup (same pattern as client mode) ---
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = UiState::new(username.to_string());
    state.connection_status = "P2P Mesh".into();
    state.push_server_message(ServerMessage::System {
        content: "P2P node up. Commands: /name <user>, /msg @user <text>, /quit".into(),
        timestamp: Utc::now(),
    });

    let ui_result = run_ui_loop(&mut terminal, &mut state, &ui_to_net_tx, &mut net_to_ui_rx).await;

    let _ = ui_to_net_tx.send(ClientMessage::Quit).await;
    disable_raw_mode().context("disable raw mode")?;
    crossterm::execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    ui_result
}

/// Handle one peer connection: handshake, announce join, read loop, cleanup.
#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    role: Role,
    stream: TcpStream,
    peers: PeerMap,
    our_name: Arc<RwLock<String>>,
    ui: mpsc::Sender<ServerMessage>,
    own_addr: &str,
    dialed: Arc<Mutex<HashSet<String>>>,
) -> Result<()> {
    let (mut reader, mut writer) = split(stream);

    // --- Handshake: exchange Hello both ways (full-duplex safe) ---
    let (peer_name, peer_addr) = match role {
        Role::Outbound => {
            let my = our_name.read().await.clone();
            write_frame(
                &mut writer,
                &PeerMessage::Hello {
                    username: my,
                    addr: own_addr.to_string(),
                },
            )
            .await?;
            let msg = read_frame::<_, PeerMessage>(&mut reader).await?;
            match msg {
                PeerMessage::Hello { username, addr } => (username, addr),
                other => {
                    write_peer_error(&mut writer, &other).await?;
                    bail!("expected Hello on outbound handshake");
                }
            }
        }
        Role::Inbound => {
            let msg = read_frame::<_, PeerMessage>(&mut reader).await?;
            let (name, addr) = match msg {
                PeerMessage::Hello { username, addr } => (username, addr),
                other => {
                    write_peer_error(&mut writer, &other).await?;
                    bail!("expected Hello on inbound handshake");
                }
            };
            let my = our_name.read().await.clone();
            write_frame(
                &mut writer,
                &PeerMessage::Hello {
                    username: my,
                    addr: own_addr.to_string(),
                },
            )
            .await?;
            (name, addr)
        }
    };

    if !is_valid_name(&peer_name) {
        write_frame(
            &mut writer,
            &PeerMessage::Error {
                content: "invalid username (2-24 chars, alphanumeric + _ -)".into(),
            },
        )
        .await?;
        bail!("peer sent invalid username");
    }
    if peer_name.eq_ignore_ascii_case(&our_name.read().await) {
        write_frame(
            &mut writer,
            &PeerMessage::Error {
                content: "that's your own username".into(),
            },
        )
        .await?;
        bail!("refusing self-connection");
    }
    if username_taken(&peers, &peer_name).await {
        write_frame(
            &mut writer,
            &PeerMessage::Error {
                content: format!("username '{peer_name}' is already connected"),
            },
        )
        .await?;
        bail!("duplicate peer username");
    }

    let client_id = Uuid::new_v4();
    let (tx, mut rx) = mpsc::channel::<PeerMessage>(64);
    {
        let mut map = peers.write().await;
        map.insert(
            client_id,
            PeerEntry {
                username: peer_name.clone(),
                addr: peer_addr.clone(),
                outbound: tx,
            },
        );
    }

    // Notify everyone (including this peer, which gets it via our broadcast).
    let ts = Utc::now();
    broadcast(
        &peers,
        PeerMessage::Notice {
            content: format!("{peer_name} joined the mesh"),
            timestamp: ts,
        },
    )
    .await;
    let _ = ui
        .send(ServerMessage::System {
            content: format!("{peer_name} joined the mesh"),
            timestamp: ts,
        })
        .await;
    push_user_list(&peers, &our_name.read().await, &ui).await;

    // Address gossip: teach the newcomer the dialable addrs of our other
    // peers so the mesh can grow from a single seed across the internet.
    introduce_peer(&peers, client_id, own_addr).await;

    // Writer task: outbound channel → TCP.
    let write_handle = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_frame(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    let read_result =
        read_loop(client_id, &peers, &our_name, &ui, own_addr, &dialed, &mut reader).await;

    remove_peer(client_id, &peers, &our_name.read().await, &ui).await;
    write_handle.abort();

    read_result
}

/// Send an `Introduce` listing our other peers' dialable addresses, so the
/// newcomer can dial the rest of the mesh without knowing every seed.
async fn introduce_peer(peers: &PeerMap, new_id: Uuid, own_addr: &str) {
    let tx = {
        let map = peers.read().await;
        map.get(&new_id).map(|p| p.outbound.clone())
    };
    let Some(tx) = tx else { return };

    let addrs: Vec<String> = {
        let map = peers.read().await;
        map.values()
            .filter(|p| is_dialable(&p.addr) && p.addr != own_addr)
            .map(|p| p.addr.clone())
            .collect()
    };
    // Deduplicate while keeping order.
    let mut seen = HashSet::new();
    let addrs: Vec<String> = addrs
        .into_iter()
        .filter(|a| seen.insert(a.clone()))
        .collect();

    if !addrs.is_empty() {
        let _ = tx.send(PeerMessage::Introduce { addrs }).await;
    }
}

async fn read_loop(
    client_id: Uuid,
    peers: &PeerMap,
    our_name: &Arc<RwLock<String>>,
    ui: &mpsc::Sender<ServerMessage>,
    own_addr: &str,
    dialed: &Arc<Mutex<HashSet<String>>>,
    reader: &mut ReadHalf<TcpStream>,
) -> Result<()> {
    loop {
        let msg: PeerMessage = match read_frame(reader).await {
            Ok(m) => m,
            Err(_) => break, // disconnect
        };
        match msg {
            PeerMessage::Chat {
                username,
                content,
                timestamp,
            } => {
                let _ = ui
                    .send(ServerMessage::Chat {
                        username,
                        content,
                        timestamp,
                    })
                    .await;
            }
            PeerMessage::Private {
                from,
                to,
                content,
                timestamp,
            } => {
                let _ = ui
                    .send(ServerMessage::Private {
                        from,
                        to,
                        content,
                        timestamp,
                    })
                    .await;
            }
            PeerMessage::Notice { content, timestamp } => {
                let _ = ui
                    .send(ServerMessage::System { content, timestamp })
                    .await;
            }
            PeerMessage::NameChange { old_name, new_name } => {
                {
                    let mut map = peers.write().await;
                    if let Some(p) = map.get_mut(&client_id) {
                        p.username = new_name.clone();
                    }
                }
                let _ = ui
                    .send(ServerMessage::System {
                        content: format!("{old_name} is now known as {new_name}"),
                        timestamp: Utc::now(),
                    })
                    .await;
                push_user_list(peers, &our_name.read().await, ui).await;
            }
            PeerMessage::Introduce { addrs } => {
                // `spawn_peer_dial` owns the dedup (atomic insert into the
                // dialed set), so duplicate gossips never double-dial.
                for addr in addrs {
                    if !is_dialable(&addr) || addr == own_addr {
                        continue;
                    }
                    info!("mesh gossip: dialing introduced peer {addr}");
                    spawn_peer_dial(addr, peers, our_name, ui, own_addr, dialed);
                }
            }
            PeerMessage::Bye { .. } => break,
            PeerMessage::Error { content } => {
                let _ = ui
                    .send(ServerMessage::System {
                        content: format!("⚠ {content}"),
                        timestamp: Utc::now(),
                    })
                    .await;
            }
            PeerMessage::Hello { .. } => {}
        }
    }
    Ok(())
}

/// Outgoing messages from the UI: render locally and forward to peers.
/// Returns `true` when the node should shut down (quit).
async fn handle_outgoing(
    msg: ClientMessage,
    peers: &PeerMap,
    our_name: &Arc<RwLock<String>>,
    ui: &mpsc::Sender<ServerMessage>,
) -> bool {
    match msg {
        ClientMessage::Chat { content } => {
            if content.trim().is_empty() {
                return false;
            }
            let name = our_name.read().await.clone();
            let ts = Utc::now();
            let _ = ui
                .send(ServerMessage::Chat {
                    username: name.clone(),
                    content: content.clone(),
                    timestamp: ts,
                })
                .await;
            broadcast(
                peers,
                PeerMessage::Chat {
                    username: name,
                    content,
                    timestamp: ts,
                },
            )
            .await;
            false
        }
        ClientMessage::ChangeName { new_name } => {
            if !is_valid_name(&new_name)
                || username_taken(peers, &new_name).await
                || new_name.eq_ignore_ascii_case(&our_name.read().await)
            {
                let _ = ui
                    .send(ServerMessage::Error {
                        content: format!("invalid or taken username: '{new_name}'"),
                        timestamp: Utc::now(),
                    })
                    .await;
                return false;
            }
            let old = our_name.read().await.clone();
            *our_name.write().await = new_name.clone();
            let ts = Utc::now();
            let _ = ui
                .send(ServerMessage::System {
                    content: format!("you are now known as {new_name}"),
                    timestamp: ts,
                })
                .await;
            broadcast(peers, PeerMessage::NameChange { old_name: old, new_name }).await;
            push_user_list(peers, &our_name.read().await, ui).await;
            false
        }
        ClientMessage::PrivateMsg { target, content } => {
            let from = our_name.read().await.clone();
            match find_peer(peers, &target).await {
                Some(id) => {
                    let ts = Utc::now();
                    let pmsg = PeerMessage::Private {
                        from: from.clone(),
                        to: target.clone(),
                        content: content.clone(),
                        timestamp: ts,
                    };
                    let tx = {
                        let map = peers.read().await;
                        map.get(&id).map(|p| p.outbound.clone())
                    };
                    if let Some(tx) = tx {
                        let _ = tx.send(pmsg).await;
                    }
                    let _ = ui
                        .send(ServerMessage::Private {
                            from,
                            to: target,
                            content,
                            timestamp: ts,
                        })
                        .await;
                }
                None => {
                    let _ = ui
                        .send(ServerMessage::Error {
                            content: format!("user '{target}' is not a direct peer"),
                            timestamp: Utc::now(),
                        })
                        .await;
                }
            }
            false
        }
        ClientMessage::Quit => {
            let name = our_name.read().await.clone();
            let ts = Utc::now();
            broadcast(
                peers,
                PeerMessage::Notice {
                    content: format!("{name} left the mesh"),
                    timestamp: ts,
                },
            )
            .await;
            broadcast(peers, PeerMessage::Bye { username: name }).await;
            true
        }
        ClientMessage::Join { .. } => false,
    }
}

async fn remove_peer(
    client_id: Uuid,
    peers: &PeerMap,
    our_name: &str,
    ui: &mpsc::Sender<ServerMessage>,
) {
    let removed = {
        let mut map = peers.write().await;
        map.remove(&client_id).map(|p| p.username)
    };
    if let Some(name) = removed {
        let ts = Utc::now();
        let _ = ui
            .send(ServerMessage::System {
                content: format!("{name} left the mesh"),
                timestamp: ts,
            })
            .await;
        broadcast(
            peers,
            PeerMessage::Notice {
                content: format!("{name} left the mesh"),
                timestamp: ts,
            },
        )
        .await;
        push_user_list(peers, our_name, ui).await;
        info!("{name} disconnected");
    }
}

async fn broadcast(peers: &PeerMap, msg: PeerMessage) {
    let map = peers.read().await;
    for peer in map.values() {
        if let Err(e) = peer.outbound.send(msg.clone()).await {
            error!("p2p broadcast failed for {}: {e}", peer.username);
        }
    }
}

/// Build the local user list (direct peers + self) and show it in the UI.
async fn push_user_list(peers: &PeerMap, our_name: &str, ui: &mpsc::Sender<ServerMessage>) {
    let mut users: Vec<String> = {
        let map = peers.read().await;
        map.values().map(|p| p.username.clone()).collect()
    };
    users.push(our_name.to_string());
    users.sort();
    users.dedup();
    let _ = ui.send(ServerMessage::UserList { users }).await;
}

async fn username_taken(peers: &PeerMap, name: &str) -> bool {
    let map = peers.read().await;
    map.values()
        .any(|p| p.username.eq_ignore_ascii_case(name))
}

async fn find_peer(peers: &PeerMap, name: &str) -> Option<Uuid> {
    let map = peers.read().await;
    map.iter()
        .find(|(_, p)| p.username.eq_ignore_ascii_case(name))
        .map(|(id, _)| *id)
}

/// Send an `Error` frame describing what an unexpected message was.
async fn write_peer_error<W>(writer: &mut W, unexpected: &PeerMessage) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    write_frame(
        writer,
        &PeerMessage::Error {
            content: format!("unexpected first message: {}", variant_name(unexpected)),
        },
    )
    .await
}

fn variant_name(msg: &PeerMessage) -> &'static str {
    match msg {
        PeerMessage::Hello { .. } => "Hello",
        PeerMessage::Chat { .. } => "Chat",
        PeerMessage::Private { .. } => "Private",
        PeerMessage::NameChange { .. } => "NameChange",
        PeerMessage::Notice { .. } => "Notice",
        PeerMessage::Introduce { .. } => "Introduce",
        PeerMessage::Bye { .. } => "Bye",
        PeerMessage::Error { .. } => "Error",
    }
}

fn is_valid_name(name: &str) -> bool {
    let len = name.chars().count();
    if !(2..=24).contains(&len) {
        return false;
    }
    name.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// Dial a peer address with a permanent retry loop — one task per address.
/// The shared `dialed` set guarantees an address is only ever dialed once,
/// which prevents `Introduce` gossip from spawning connection storms.
#[allow(clippy::too_many_arguments)]
fn spawn_peer_dial(
    addr: String,
    peers: &PeerMap,
    our_name: &Arc<RwLock<String>>,
    ui: &mpsc::Sender<ServerMessage>,
    own_addr: &str,
    dialed: &Arc<Mutex<HashSet<String>>>,
) {
    {
        let mut seen = dialed.lock().unwrap();
        if !seen.insert(addr.clone()) {
            return;
        }
    }
    let peers = Arc::clone(peers);
    let our_name = Arc::clone(our_name);
    let ui = ui.clone();
    let dialed = Arc::clone(dialed);
    let own_addr = own_addr.to_string();
    tokio::spawn(async move {
        loop {
            match TcpStream::connect(&addr).await {
                Ok(stream) => {
                    info!("connected to peer {addr}");
                    let res = handle_connection(
                        Role::Outbound,
                        stream,
                        peers.clone(),
                        our_name.clone(),
                        ui.clone(),
                        &own_addr,
                        dialed.clone(),
                    )
                    .await;
                    match res {
                        Ok(()) => warn!("peer {addr} closed"),
                        Err(e) => warn!("peer {addr} ended: {e:#}"),
                    }
                }
                Err(e) => warn!("dial {addr} failed: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
}

/// An address others can actually dial: non-empty and not a wildcard bind.
fn is_dialable(addr: &str) -> bool {
    let a = addr.trim();
    !a.is_empty()
        && !a.starts_with("0.0.0.0")
        && !a.starts_with("::")
        && !a.starts_with("0:0")
}

/// TUI loop shared with node wiring (mirrors client mode; channels are typed
/// differently, so it is duplicated here to keep client.rs untouched).
async fn run_ui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut UiState,
    ui_to_net: &mpsc::Sender<ClientMessage>,
    net_to_ui: &mut mpsc::Receiver<ServerMessage>,
) -> Result<()> {
    loop {
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
                            content: "Node shut down".into(),
                            timestamp: Utc::now(),
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