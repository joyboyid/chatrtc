//! TCP chat server — accepts clients, broadcasts messages, handles commands.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use tokio::io::split;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::message::{read_frame, write_frame, ClientMessage, ServerMessage};

/// Per-connection state tracked by the server.
struct ConnectedClient {
    username: String,
    /// Channel to push outbound server messages to this client's write task.
    outbound: mpsc::Sender<ServerMessage>,
}

/// Shared server state — `Arc<RwLock<...>>` allows many tasks to read/write safely.
type ClientMap = Arc<RwLock<HashMap<Uuid, ConnectedClient>>>;

/// Run the chat server on `bind_addr` until the process is killed.
pub async fn run(bind_addr: &str) -> Result<()> {
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("bind {bind_addr}"))?;
    info!("server listening on {bind_addr}");

    let clients: ClientMap = Arc::new(RwLock::new(HashMap::new()));

    loop {
        let (stream, peer) = listener.accept().await.context("accept connection")?;
        info!("new connection from {peer}");
        let clients = Arc::clone(&clients);
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, clients).await {
                warn!("client {peer} ended with error: {e:#}");
            }
        });
    }
}

async fn handle_client(stream: TcpStream, clients: ClientMap) -> Result<()> {
    let (mut reader, mut writer) = split(stream);

    // --- Handshake: first frame must be Join ---
    let join_msg: ClientMessage = read_frame(&mut reader).await?;
    let username = match join_msg {
        ClientMessage::Join { username } if is_valid_username(&username) => username,
        _ => {
            write_frame(
                &mut writer,
                &ServerMessage::Error {
                    content: "first message must be Join with a valid username".into(),
                    timestamp: Utc::now(),
                },
            )
            .await?;
            anyhow::bail!("invalid join handshake");
        }
    };

    if username_taken(&clients, &username).await {
        write_frame(
            &mut writer,
            &ServerMessage::Error {
                content: format!("username '{username}' is already taken"),
                timestamp: Utc::now(),
            },
        )
        .await?;
        anyhow::bail!("duplicate username");
    }

    let client_id = Uuid::new_v4();
    let (tx, mut rx) = mpsc::channel::<ServerMessage>(64);

    {
        let mut map = clients.write().await;
        map.insert(
            client_id,
            ConnectedClient {
                username: username.clone(),
                outbound: tx,
            },
        );
    }

    // Notify everyone (including the new client via broadcast).
    broadcast(
        &clients,
        ServerMessage::System {
            content: format!("{username} joined the room"),
            timestamp: Utc::now(),
        },
    )
    .await;
    send_user_list(&clients).await;

    // Writer task: drain outbound channel → TCP.
    let write_handle = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_frame(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    // Reader loop: process inbound client messages.
    let reader_result = read_loop(client_id, &username, &mut reader, &clients).await;

    // Cleanup on disconnect.
    remove_client(client_id, &username, &clients).await;
    write_handle.abort();

    reader_result
}

async fn read_loop(
    client_id: Uuid,
    username: &str,
    reader: &mut tokio::io::ReadHalf<TcpStream>,
    clients: &ClientMap,
) -> Result<()> {
    loop {
        let msg: ClientMessage = match read_frame(reader).await {
            Ok(m) => m,
            Err(_) => break, // disconnect
        };

        match msg {
            ClientMessage::Chat { content } => {
                if content.trim().is_empty() {
                    continue;
                }
                let current_name = current_username(clients, client_id)
                    .await
                    .unwrap_or_else(|| username.to_string());
                broadcast(
                    clients,
                    ServerMessage::Chat {
                        username: current_name,
                        content,
                        timestamp: Utc::now(),
                    },
                )
                .await;
            }
            ClientMessage::ChangeName { new_name } => {
                if !is_valid_username(&new_name) {
                    send_to_client(
                        clients,
                        client_id,
                        ServerMessage::Error {
                            content: "invalid username (2-24 chars, alphanumeric + _ -)".into(),
                            timestamp: Utc::now(),
                        },
                    )
                    .await;
                    continue;
                }
                if username_taken(clients, &new_name).await {
                    send_to_client(
                        clients,
                        client_id,
                        ServerMessage::Error {
                            content: format!("username '{new_name}' is already taken"),
                            timestamp: Utc::now(),
                        },
                    )
                    .await;
                    continue;
                }
                let old_name = current_username(clients, client_id)
                    .await
                    .unwrap_or_else(|| username.to_string());
                {
                    let mut map = clients.write().await;
                    if let Some(client) = map.get_mut(&client_id) {
                        client.username = new_name.clone();
                    }
                }
                broadcast(
                    clients,
                    ServerMessage::System {
                        content: format!("{old_name} is now known as {new_name}"),
                        timestamp: Utc::now(),
                    },
                )
                .await;
                send_user_list(clients).await;
            }
            ClientMessage::PrivateMsg { target, content } => {
                let from = current_username(clients, client_id)
                    .await
                    .unwrap_or_else(|| username.to_string());
                match find_client_by_name(clients, &target).await {
                    Some(target_id) => {
                        let ts = Utc::now();
                        let msg = ServerMessage::Private {
                            from: from.clone(),
                            to: target.clone(),
                            content: content.clone(),
                            timestamp: ts,
                        };
                        send_to_client(clients, target_id, msg.clone()).await;
                        // Echo back to sender so they see their own PM.
                        send_to_client(clients, client_id, msg).await;
                    }
                    None => {
                        send_to_client(
                            clients,
                            client_id,
                            ServerMessage::Error {
                                content: format!("user '{target}' not found or offline"),
                                timestamp: Utc::now(),
                            },
                        )
                        .await;
                    }
                }
            }
            ClientMessage::Quit => break,
            ClientMessage::Join { .. } => {
                send_to_client(
                    clients,
                    client_id,
                    ServerMessage::Error {
                        content: "already joined".into(),
                        timestamp: Utc::now(),
                    },
                )
                .await;
            }
        }
    }
    Ok(())
}

async fn remove_client(client_id: Uuid, username: &str, clients: &ClientMap) {
    let removed = {
        let mut map = clients.write().await;
        map.remove(&client_id).is_some()
    };
    if removed {
        broadcast(
            clients,
            ServerMessage::System {
                content: format!("{username} left the room"),
                timestamp: Utc::now(),
            },
        )
        .await;
        send_user_list(clients).await;
        info!("{username} disconnected");
    }
}

async fn broadcast(clients: &ClientMap, msg: ServerMessage) {
    let map = clients.read().await;
    for client in map.values() {
        if let Err(e) = client.outbound.send(msg.clone()).await {
            error!("broadcast send failed for {}: {e}", client.username);
        }
    }
}

async fn send_to_client(clients: &ClientMap, id: Uuid, msg: ServerMessage) {
    let map = clients.read().await;
    if let Some(client) = map.get(&id) {
        let _ = client.outbound.send(msg).await;
    }
}

async fn send_user_list(clients: &ClientMap) {
    let users = {
        let map = clients.read().await;
        let mut names: Vec<String> = map.values().map(|c| c.username.clone()).collect();
        names.sort();
        names
    };
    broadcast(clients, ServerMessage::UserList { users }).await;
}

async fn username_taken(clients: &ClientMap, name: &str) -> bool {
    let map = clients.read().await;
    map.values()
        .any(|c| c.username.eq_ignore_ascii_case(name))
}

async fn current_username(clients: &ClientMap, id: Uuid) -> Option<String> {
    let map = clients.read().await;
    map.get(&id).map(|c| c.username.clone())
}

async fn find_client_by_name(clients: &ClientMap, name: &str) -> Option<Uuid> {
    let map = clients.read().await;
    map.iter()
        .find(|(_, c)| c.username.eq_ignore_ascii_case(name))
        .map(|(id, _)| *id)
}

fn is_valid_username(name: &str) -> bool {
    let len = name.chars().count();
    if !(2..=24).contains(&len) {
        return false;
    }
    name.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}