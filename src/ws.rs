//! WebSocket transport.
//!
//! Presents a [`WebSocketStream`] as a plain `AsyncRead`/`AsyncWrite` byte
//! stream so the length-prefixed framing in [`crate::message`] keeps working
//! unchanged. That lets the exact same chat logic run over raw TCP *or*
//! WebSocket, which is what makes the app reachable through reverse proxies
//! and CDNs (nginx, Caddy, Cloudflare) that only speak HTTP.
//!
//! Each `write_frame` is buffered until `flush` and then sent as exactly one
//! WebSocket *binary* message, so message boundaries on the wire stay intact.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// First bytes of an HTTP/1.1 request — used to detect a WebSocket handshake.
const HTTP_REQUEST_PREFIX: &[u8] = b"GET ";

/// WebSocket connection presented as a byte stream.
pub struct WsStream<S = TcpStream> {
    inner: WebSocketStream<S>,
    /// Bytes written but not yet flushed; sent as ONE binary message.
    write_buf: Vec<u8>,
    /// Unread remainder of the current binary message.
    pending: Vec<u8>,
    /// Read cursor into `pending`.
    pending_pos: usize,
}

/// Server side: a handshake accepted straight from a TCP listener.
pub type ServerWs = WsStream<TcpStream>;

/// Client side: TCP, optionally wrapped in TLS for `wss://`.
pub type ClientWs = WsStream<MaybeTlsStream<TcpStream>>;

impl<S> WsStream<S> {
    pub fn new(inner: WebSocketStream<S>) -> Self {
        Self {
            inner,
            write_buf: Vec::new(),
            pending: Vec::new(),
            pending_pos: 0,
        }
    }

    /// Copy as much of the current message into `buf` as fits.
    fn drain_pending(&mut self, buf: &mut ReadBuf<'_>) {
        let available = self.pending.len().saturating_sub(self.pending_pos);
        let n = available.min(buf.remaining());
        if n == 0 {
            return;
        }
        buf.put_slice(&self.pending[self.pending_pos..self.pending_pos + n]);
        self.pending_pos += n;
        if self.pending_pos == self.pending.len() {
            self.pending.clear();
            self.pending_pos = 0;
        }
    }
}

impl<S> AsyncWrite for WsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let me = self.get_mut();
        me.write_buf.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        loop {
            if me.write_buf.is_empty() {
                return match Pin::new(&mut me.inner).poll_flush(cx) {
                    Poll::Ready(result) => Poll::Ready(result.map_err(io::Error::other)),
                    Poll::Pending => Poll::Pending,
                };
            }
            match Pin::new(&mut me.inner).poll_ready(cx) {
                Poll::Ready(Ok(())) => {
                    let payload = std::mem::take(&mut me.write_buf);
                    let msg = Message::Binary(payload.into());
                    if let Err(e) = Pin::new(&mut me.inner).start_send(msg) {
                        return Poll::Ready(Err(io::Error::other(e)));
                    }
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(io::Error::other(e))),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        // Never drop buffered chat data: flush it before the close frame.
        if !me.write_buf.is_empty() {
            return Pin::new(me).poll_flush(cx);
        }
        match Pin::new(&mut me.inner).poll_ready(cx) {
            Poll::Ready(Ok(())) => {
                if let Err(e) = Pin::new(&mut me.inner).start_send(Message::Close(None)) {
                    return Poll::Ready(Err(io::Error::other(e)));
                }
                match Pin::new(&mut me.inner).poll_flush(cx) {
                    Poll::Ready(result) => Poll::Ready(result.map_err(io::Error::other)),
                    Poll::Pending => Poll::Pending,
                }
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> AsyncRead for WsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();

        if me.pending_pos < me.pending.len() {
            me.drain_pending(buf);
            return Poll::Ready(Ok(()));
        }

        loop {
            match Pin::new(&mut me.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(Message::Binary(data)))) => {
                    if data.is_empty() {
                        continue;
                    }
                    me.pending = data.to_vec();
                    me.pending_pos = 0;
                    me.drain_pending(buf);
                    return Poll::Ready(Ok(()));
                }
                // Text, ping/pong and close are not part of our protocol.
                // tungstenite answers pings with a pong on the next write,
                // and a close frame is reported as end-of-stream below.
                Poll::Ready(Some(Ok(Message::Close(_)))) | Poll::Ready(None) => {
                    return Poll::Ready(Ok(()))
                }
                // Text, pong and ping frames carry no protocol payload.
                // Looping back into poll_next is what lets tungstenite flush
                // its automatic pong reply: an idle session must still answer
                // keepalive pings or a proxy/CDN in between may drop us.
                Poll::Ready(Some(Ok(_))) => continue,
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(io::Error::other(e))),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// True when the connection starts with an HTTP request, i.e. the client is
/// trying a WebSocket upgrade rather than speaking the raw TCP protocol.
///
/// `TcpStream::peek` never consumes, so the handshake bytes stay intact for
/// whichever handler picks the connection up. It parks until the peer sends
/// something, so callers should bound it with a timeout.
pub async fn looks_like_websocket_handshake(stream: &mut TcpStream) -> io::Result<bool> {
    let mut head = [0u8; HTTP_REQUEST_PREFIX.len()];
    let n = stream.peek(&mut head).await?;
    if n < HTTP_REQUEST_PREFIX.len() {
        // Peer closed before sending a full request line.
        return Ok(false);
    }
    Ok(head == *HTTP_REQUEST_PREFIX)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{read_frame, write_frame, ClientMessage, ServerMessage};
    use std::future::poll_fn;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Bring up a connected client/server WebSocket pair on a random port.
    async fn ws_pair() -> (
        WsStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
        WsStream<TcpStream>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            WsStream::new(ws)
        });

        let (client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();

        (WsStream::new(client), server.await.unwrap())
    }

    #[tokio::test]
    async fn client_frame_reaches_server_intact() {
        let (mut client, mut server) = ws_pair().await;

        let sent = ClientMessage::Join {
            username: "andi".into(),
        };
        write_frame(&mut client, &sent).await.unwrap();

        let got: ClientMessage = read_frame(&mut server).await.unwrap();
        match (sent, got) {
            (ClientMessage::Join { username: a }, ClientMessage::Join { username: b }) => {
                assert_eq!(a, b);
            }
            other => panic!("message changed in transit: {other:?}"),
        }
    }

    #[tokio::test]
    async fn several_frames_keep_their_boundaries() {
        let (mut client, mut server) = ws_pair().await;

        let batch = ["satu", "dua", "tiga"];
        for text in batch {
            write_frame(
                &mut client,
                &ServerMessage::System {
                    content: text.into(),
                    timestamp: chrono::Utc::now(),
                },
            )
            .await
            .unwrap();
        }

        for expected in batch {
            let got: ServerMessage = read_frame(&mut server).await.unwrap();
            match got {
                ServerMessage::System { content, .. } => assert_eq!(content, expected),
                other => panic!("unexpected message: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn large_payload_survives_split_tcp_reads() {
        let (mut client, mut server) = ws_pair().await;

        // Large enough that the WebSocket message arrives over several TCP
        // segments, exercising the partial-read path.
        let content = "x".repeat(200_000);
        write_frame(
            &mut client,
            &ServerMessage::Chat {
                username: "budi".into(),
                content: content.clone(),
                timestamp: chrono::Utc::now(),
            },
        )
        .await
        .unwrap();

        match read_frame::<_, ServerMessage>(&mut server).await.unwrap() {
            ServerMessage::Chat { content: got, .. } => assert_eq!(got.len(), content.len()),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[tokio::test]
    async fn bidirectional_traffic_works() {
        let (mut client, mut server) = ws_pair().await;

        write_frame(&mut server, &ClientMessage::Quit)
            .await
            .unwrap();
        write_frame(&mut client, &ClientMessage::Quit)
            .await
            .unwrap();

        let _: ClientMessage = read_frame(&mut client).await.unwrap();
        let _: ClientMessage = read_frame(&mut server).await.unwrap();
    }

    #[tokio::test]
    async fn keepalive_ping_is_answered_while_the_chat_is_idle() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut ws = WsStream::new(ws);
            // Drive the reader with no application traffic at all: the only
            // thing that can come back is a protocol-level pong.
            let mut sink = [0u8; 64];
            let mut read_buf = ReadBuf::new(&mut sink);
            loop {
                let result = poll_fn(|cx| Pin::new(&mut ws).poll_read(cx, &mut read_buf)).await;
                if result.is_err() {
                    break;
                }
            }
        });

        // Hand-rolled client so the pong is observable on the raw socket.
        let mut sock = TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "GET /chat HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        sock.write_all(request.as_bytes()).await.unwrap();

        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            sock.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        assert!(
            head.starts_with(b"HTTP/1.1 101"),
            "upgrade failed: {:?}",
            String::from_utf8_lossy(&head)
        );

        // Masked ping, no payload.
        sock.write_all(&[0x89, 0x80, 0, 0, 0, 0]).await.unwrap();

        let pong = tokio::time::timeout(Duration::from_secs(5), async {
            let mut frame = [0u8; 2];
            sock.read_exact(&mut frame).await.unwrap();
            frame
        })
        .await;

        server_task.abort();

        let frame = pong.expect("no pong arrived for the keepalive ping");
        assert_eq!(
            frame[0] & 0x0F,
            0xA,
            "expected a pong frame, got opcode {}",
            frame[0] & 0x0F
        );
    }

    #[tokio::test]
    async fn handshake_detection_tells_transports_apart() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // A raw protocol client starts with a 4-byte big-endian length,
        // never an HTTP request line.
        let mut raw = TcpStream::connect(addr).await.unwrap();
        let payload = bincode::serialize(&ClientMessage::Quit).unwrap();
        AsyncWriteExt::write_all(&mut raw, &(payload.len() as u32).to_be_bytes())
            .await
            .unwrap();
        AsyncWriteExt::write_all(&mut raw, &payload).await.unwrap();

        let (mut accepted_raw, _) = listener.accept().await.unwrap();
        assert!(!looks_like_websocket_handshake(&mut accepted_raw)
            .await
            .unwrap());

        // A WebSocket client starts with "GET / HTTP/1.1". Its handshake is
        // driven from a task because it blocks until the server replies 101.
        let client = tokio::spawn(async move {
            let (ws, _) = tokio_tungstenite::client_async(
                format!("ws://{addr}/chat"),
                TcpStream::connect(addr).await.unwrap(),
            )
            .await
            .unwrap();
            WsStream::new(ws)
        });

        let (mut ws_server, _) = listener.accept().await.unwrap();
        assert!(looks_like_websocket_handshake(&mut ws_server)
            .await
            .unwrap());

        // Detection must not consume anything: the handshake still completes.
        let ws = tokio_tungstenite::accept_async(ws_server).await.unwrap();
        let mut ws_server = WsStream::new(ws);
        let mut ws_client = client.await.unwrap();

        write_frame(&mut ws_client, &ClientMessage::Quit)
            .await
            .unwrap();
        let _: ClientMessage = read_frame(&mut ws_server).await.unwrap();
    }
}
