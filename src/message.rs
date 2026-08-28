//! Protocol messages and length-prefixed bincode framing over TCP.
//!
//! Each frame: `[u32 length (big-endian)][bincode payload]`

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Messages sent from client → server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// First message after TCP connect; registers username.
    Join { username: String },
    /// Public chat message (broadcast by server).
    Chat { content: String },
    /// `/name <new>` — request username change.
    ChangeName { new_name: String },
    /// `/msg @user text` — private message.
    PrivateMsg { target: String, content: String },
    /// `/quit` or `/exit` — graceful disconnect.
    Quit,
}

/// Messages sent from server → client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Public chat line with timestamp.
    Chat {
        username: String,
        content: String,
        timestamp: DateTime<Utc>,
    },
    /// System notice (join/leave/rename/info).
    System {
        content: String,
        timestamp: DateTime<Utc>,
    },
    /// Private message delivered to sender or recipient.
    Private {
        from: String,
        to: String,
        content: String,
        timestamp: DateTime<Utc>,
    },
    /// Full list of online usernames (sent on join + on changes).
    UserList { users: Vec<String> },
    /// Server rejected an action (duplicate name, unknown user, etc.).
    Error {
        content: String,
        timestamp: DateTime<Utc>,
    },
}

/// Write one length-prefixed bincode frame.
pub async fn write_frame<W, T>(writer: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = bincode::serialize(msg).context("serialize message")?;
    let len = payload.len() as u32;
    writer.write_u32(len).await.context("write frame length")?;
    writer.write_all(&payload).await.context("write frame payload")?;
    writer.flush().await.context("flush frame")?;
    Ok(())
}

/// Read one length-prefixed bincode frame.
pub async fn read_frame<R, T>(reader: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let len = reader.read_u32().await.context("read frame length")?;
    // Guard against absurd allocations from malformed streams.
    if len > 1_048_576 {
        anyhow::bail!("frame too large: {len} bytes");
    }
    let mut buf = vec![0u8; len as usize];
    reader
        .read_exact(&mut buf)
        .await
        .context("read frame payload")?;
    let msg = bincode::deserialize(&buf).context("deserialize message")?;
    Ok(msg)
}

/// Parse `/msg @alice hello world` style commands from the input bar.
pub fn parse_slash_command(input: &str) -> Option<ClientMessage> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();
    match parts[0] {
        "/name" => {
            let new_name = parts.get(1)?.trim();
            if new_name.is_empty() {
                return None;
            }
            Some(ClientMessage::ChangeName {
                new_name: new_name.to_string(),
            })
        }
        "/msg" => {
            let target = parts.get(1)?.trim_start_matches('@');
            let content = parts.get(2)?;
            if target.is_empty() || content.is_empty() {
                return None;
            }
            Some(ClientMessage::PrivateMsg {
                target: target.to_string(),
                content: content.to_string(),
            })
        }
        "/quit" | "/exit" => Some(ClientMessage::Quit),
        _ => None,
    }
}