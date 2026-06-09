//! A `Channel` that accepts inbound messages over a unix domain socket using a
//! newline-delimited JSON protocol.
//!
//! Wire format — one JSON object per line, in both directions:
//!   request:  {"input": "...", "session": "optional-id"}
//!   response: {"reply": "..."}   or   {"error": "..."}
//!
//! A connection may send many request lines; each gets one response line, so a
//! single connection can carry a multi-turn conversation (pass the same
//! `session` to continue it; omit it and the gateway mints a fresh session).
//!
//! The socket file doubles as a single-instance guard: if the path is already
//! connectable, another gateway owns it and `bind` fails; a stale socket from a
//! crashed run is replaced. This mirrors hermes's PID-file instance guard with
//! no extra dependency. Try it with:
//!   echo '{"input":"hi"}' | nc -U ~/.shion/gateway.sock

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::watch,
};
use tracing::{info, warn};

use crate::{agent::gateway::Channel, domain::gateway::MessageHandler};

#[derive(Deserialize)]
struct Request {
    input: String,
    #[serde(default)]
    session: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Response {
    Ok { reply: String },
    Err { error: String },
}

pub struct UnixSocketChannel {
    path: PathBuf,
    listener: UnixListener,
}

impl UnixSocketChannel {
    /// Bind the socket at `path`, failing if another gateway is already serving
    /// there. A leftover socket file from a crashed run is removed and rebound.
    pub async fn bind(path: PathBuf) -> anyhow::Result<Self> {
        if path.exists() {
            if UnixStream::connect(&path).await.is_ok() {
                anyhow::bail!(
                    "a gateway is already running at {} (socket is live)",
                    path.display()
                );
            }
            // Connect failed → stale socket from a previous run; safe to replace.
            std::fs::remove_file(&path)?;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(&path)?;
        Ok(Self { path, listener })
    }
}

impl Drop for UnixSocketChannel {
    fn drop(&mut self) {
        // Don't leave a dead socket file behind for the next run to treat as a
        // stale-but-present path.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[async_trait]
impl Channel for UnixSocketChannel {
    fn name(&self) -> &str {
        "unix-socket"
    }

    async fn serve(
        &self,
        handler: Arc<dyn MessageHandler>,
        mut shutdown: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        info!(path = %self.path.display(), "unix socket channel listening");
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    info!("unix socket channel stopping");
                    return Ok(());
                }
                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((stream, _addr)) => {
                            let handler = handler.clone();
                            // Each connection is served concurrently so one slow
                            // turn doesn't block other clients.
                            tokio::spawn(async move {
                                if let Err(error) = serve_connection(stream, handler).await {
                                    warn!(%error, "unix socket connection error");
                                }
                            });
                        }
                        Err(error) => warn!(%error, "unix socket accept failed"),
                    }
                }
            }
        }
    }
}

async fn serve_connection(
    stream: UnixStream,
    handler: Arc<dyn MessageHandler>,
) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Request>(line) {
            Ok(request) => {
                // A fresh session per request unless the caller threads one
                // through, matching the chat REPL's program-managed uuid v7.
                let session = request
                    .session
                    .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
                match handler.handle(&session, request.input).await {
                    Ok(reply) => Response::Ok { reply },
                    Err(error) => Response::Err {
                        error: error.to_string(),
                    },
                }
            }
            Err(error) => Response::Err {
                error: format!("invalid request: {error}"),
            },
        };

        let mut bytes = serde_json::to_vec(&response)?;
        bytes.push(b'\n');
        write_half.write_all(&bytes).await?;
        write_half.flush().await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    struct EchoHandler;

    #[async_trait]
    impl MessageHandler for EchoHandler {
        async fn handle(&self, session_id: &str, input: String) -> anyhow::Result<String> {
            Ok(format!("{session_id}: {input}"))
        }
    }

    fn temp_socket(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("shion-test-{}-{tag}.sock", std::process::id()))
    }

    #[tokio::test]
    async fn round_trips_a_request_with_explicit_session() {
        let path = temp_socket("roundtrip");
        let _ = std::fs::remove_file(&path);
        let channel = UnixSocketChannel::bind(path.clone()).await.unwrap();

        let (tx, rx) = watch::channel(false);
        let handler: Arc<dyn MessageHandler> = Arc::new(EchoHandler);
        let server = tokio::spawn(async move { channel.serve(handler, rx).await });

        let stream = UnixStream::connect(&path).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        write_half
            .write_all(b"{\"input\":\"hello\",\"session\":\"s1\"}\n")
            .await
            .unwrap();

        let mut reply = String::new();
        BufReader::new(read_half)
            .read_line(&mut reply)
            .await
            .unwrap();
        assert_eq!(reply.trim(), "{\"reply\":\"s1: hello\"}");

        tx.send(true).unwrap();
        let _ = server.await;
    }

    #[tokio::test]
    async fn malformed_request_returns_an_error_response() {
        let path = temp_socket("malformed");
        let _ = std::fs::remove_file(&path);
        let channel = UnixSocketChannel::bind(path.clone()).await.unwrap();

        let (tx, rx) = watch::channel(false);
        let handler: Arc<dyn MessageHandler> = Arc::new(EchoHandler);
        let server = tokio::spawn(async move { channel.serve(handler, rx).await });

        let stream = UnixStream::connect(&path).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        write_half.write_all(b"not json\n").await.unwrap();

        let mut reply = String::new();
        BufReader::new(read_half)
            .read_line(&mut reply)
            .await
            .unwrap();
        assert!(reply.contains("\"error\""), "got: {reply}");

        tx.send(true).unwrap();
        let _ = server.await;
    }

    #[tokio::test]
    async fn rejects_a_second_bind_on_a_live_socket() {
        let path = temp_socket("instance-guard");
        let _ = std::fs::remove_file(&path);
        let _first = UnixSocketChannel::bind(path.clone()).await.unwrap();

        // The socket is bound and connectable, so a second gateway must refuse.
        let second = UnixSocketChannel::bind(path.clone()).await;
        assert!(second.is_err());
    }
}
