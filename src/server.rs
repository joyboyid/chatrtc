//! Chat server — accepts clients, broadcasts messages, handles commands.
//!
//! One listener serves two transports: raw TCP (the original protocol) and
//! WebSocket. Both carry the identical length-prefixed bincode frames, so the
//! chat logic below is shared. Multiplexing them means a single port can be
//! published through a reverse proxy or CDN that only speaks HTTP, while
//! existing TCP clients keep working unchanged.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use tokio::io::{split, AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::message::{read_frame, write_frame, ClientMessage, ServerMessage};
use crate::ws::{looks_like_websocket_handshake, ServerWs};

/// Per-connection state tracked by the server.
struct ConnectedClient {
    username: String,
    /// Channel to push outbound server messages to this client's write task.
    outbound: mpsc::Sender<ServerMessage>,
}

/// Shared server state — `Arc<RwLock<...>>` allows many tasks to read/write safely.
type ClientMap = Arc<RwLock<HashMap<Uuid, ConnectedClient>>>;

/// How long a new connection has to reveal whether it is a WebSocket upgrade.
const HANDSHAKE_PEEK_TIMEOUT: Duration = Duration::from_secs(10);

/// Run the chat server on `bind_addr` until the process is killed.
pub async fn run(bind_addr: &str) -> Result<()> {
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("bind {bind_addr}"))?;
    info!("server listening on {bind_addr} (raw TCP + WebSocket)");
    serve(listener, Arc::new(RwLock::new(HashMap::new()))).await
}

/// Accept loop shared by [`run`] and the tests (which bind their own port).
async fn serve(listener: TcpListener, clients: ClientMap) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await.context("accept connection")?;
        info!("new connection from {peer}");
        let clients = Arc::clone(&clients);
        tokio::spawn(async move {
            if let Err(e) = serve_transport(stream, clients).await {
                warn!("client {peer} ended with error: {e:#}");
            }
        });
    }
}

/// Detect the transport from the first bytes and hand the connection to the
/// matching handler.
async fn serve_transport(mut stream: TcpStream, clients: ClientMap) -> Result<()> {
    let is_ws = tokio::time::timeout(
        HANDSHAKE_PEEK_TIMEOUT,
        looks_like_websocket_handshake(&mut stream),
    )
    .await
    .map_err(|_| anyhow::anyhow!("no data within handshake timeout"))?
    .context("detect transport")?;

    if is_ws {
        info!("websocket upgrade from client");
        let ws = tokio_tungstenite::accept_async(stream)
            .await
            .context("websocket handshake")?;
        let (reader, writer) = split(ServerWs::new(ws));
        handle_client(reader, writer, clients).await
    } else {
        let (reader, writer) = split(stream);
        handle_client(reader, writer, clients).await
    }
}

async fn handle_client<R, W>(mut reader: R, mut writer: W, clients: ClientMap) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
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

    // Writer task: drain outbound channel → transport.
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

async fn read_loop<R>(
    client_id: Uuid,
    username: &str,
    reader: &mut R,
    clients: &ClientMap,
) -> Result<()>
where
    R: AsyncRead + Unpin,
{
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{read_frame, write_frame, ClientMessage, ServerMessage};
    use crate::ws::ClientWs;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    /// Start a server on an OS-assigned port and return its address.
    async fn start_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, Arc::new(RwLock::new(HashMap::new()))));
        addr
    }

    /// Read frames until the expected chat text shows up, skipping the
    /// system/user-list noise that a join produces.
    async fn wait_for_chat<R>(reader: &mut R, expected: &str) -> bool
    where
        R: AsyncRead + Unpin,
    {
        for _ in 0..16 {
            match read_frame::<_, ServerMessage>(reader).await {
                Ok(ServerMessage::Chat { content, .. }) if content == expected => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
        false
    }

    #[tokio::test]
    async fn raw_tcp_and_websocket_clients_share_one_port() {
        let addr = start_server().await;

        // Fail fast instead of hanging if the server misbehaves.
        let ok = tokio::time::timeout(Duration::from_secs(10), async {
            let mut tcp = TcpStream::connect(addr).await.unwrap();
            write_frame(
                &mut tcp,
                &ClientMessage::Join {
                    username: "tcpuser".into(),
                },
            )
            .await
            .unwrap();

            let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
                .await
                .unwrap();
            let (mut ws_reader, mut ws_writer) = split(ClientWs::new(ws));
            write_frame(
                &mut ws_writer,
                &ClientMessage::Join {
                    username: "wsuser".into(),
                },
            )
            .await
            .unwrap();

            // Raw TCP → WebSocket.
            write_frame(
                &mut tcp,
                &ClientMessage::Chat {
                    content: "halo dari tcp".into(),
                },
            )
            .await
            .unwrap();
            assert!(
                wait_for_chat(&mut ws_reader, "halo dari tcp").await,
                "websocket client never saw the raw TCP message"
            );

            // WebSocket → raw TCP.
            write_frame(
                &mut ws_writer,
                &ClientMessage::Chat {
                    content: "halo dari ws".into(),
                },
            )
            .await
            .unwrap();
            assert!(
                wait_for_chat(&mut tcp, "halo dari ws").await,
                "raw TCP client never saw the websocket message"
            );

            true
        })
        .await;

        assert!(ok.is_ok(), "test timed out");
    }

    #[tokio::test]
    async fn websocket_client_joins_the_same_room_as_tcp_clients() {
        let addr = start_server().await;

        let ok = tokio::time::timeout(Duration::from_secs(10), async {
            let mut tcp = TcpStream::connect(addr).await.unwrap();
            write_frame(
                &mut tcp,
                &ClientMessage::Join {
                    username: "tcpuser".into(),
                },
            )
            .await
            .unwrap();

            let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
                .await
                .unwrap();
            let (_ws_reader, mut ws_writer) = split(ClientWs::new(ws));
            write_frame(
                &mut ws_writer,
                &ClientMessage::Join {
                    username: "wsuser".into(),
                },
            )
            .await
            .unwrap();

            // The raw client must be told about the newcomer.
            for _ in 0..16 {
                match read_frame::<_, ServerMessage>(&mut tcp).await {
                    Ok(ServerMessage::UserList { users }) => {
                        if users.iter().any(|u| u == "wsuser") {
                            return true;
                        }
                    }
                    Ok(_) => continue,
                    Err(_) => return false,
                }
            }
            false
        })
        .await;

        assert!(ok.is_ok_and(|joined| joined), "websocket join not announced");
    }

    #[tokio::test]
    async fn websocket_client_gets_rejected_on_bad_join() {
        let addr = start_server().await;

        let ok = tokio::time::timeout(Duration::from_secs(10), async {
            let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
                .await
                .unwrap();
            let (mut ws_reader, mut ws_writer) = split(ClientWs::new(ws));

            // Chat before Join must be refused.
            write_frame(
                &mut ws_writer,
                &ClientMessage::Chat {
                    content: "sneaky".into(),
                },
            )
            .await
            .unwrap();

            matches!(
                read_frame::<_, ServerMessage>(&mut ws_reader).await,
                Ok(ServerMessage::Error { .. })
            )
        })
        .await;

        assert!(ok.is_ok_and(|rejected| rejected), "bad join was not rejected");
    }

    #[tokio::test]
    async fn closing_a_websocket_removes_the_user() {
        let addr = start_server().await;

        let ok = tokio::time::timeout(Duration::from_secs(10), async {
            let mut tcp = TcpStream::connect(addr).await.unwrap();
            write_frame(
                &mut tcp,
                &ClientMessage::Join {
                    username: "tcpuser".into(),
                },
            )
            .await
            .unwrap();

            let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
                .await
                .unwrap();
            let (_ws_reader, mut ws_writer) = split(ClientWs::new(ws));
            write_frame(
                &mut ws_writer,
                &ClientMessage::Join {
                    username: "wsuser".into(),
                },
            )
            .await
            .unwrap();

            // Clean WebSocket close frame, then the TCP client is told.
            ws_writer.shutdown().await.unwrap();
            drop(ws_writer);

            for _ in 0..16 {
                match read_frame::<_, ServerMessage>(&mut tcp).await {
                    Ok(ServerMessage::System { content, .. }) => {
                        if content.contains("wsuser left") {
                            return true;
                        }
                    }
                    Ok(_) => continue,
                    Err(_) => return false,
                }
            }
            false
        })
        .await;

        assert!(ok.is_ok_and(|left| left), "websocket disconnect not announced");
    }
}
