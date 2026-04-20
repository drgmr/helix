//! WebSocket accept loop + per-client MCP task.

use std::sync::Arc;

use arc_swap::ArcSwap;
use futures_util::{SinkExt, StreamExt};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc},
};
use tokio_tungstenite::tungstenite::{
    handshake::server::{ErrorResponse, Request, Response},
    http::StatusCode,
    Message,
};

use crate::{
    mcp,
    notification::Notification,
    protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse},
    snapshot::Snapshot,
};

pub struct State {
    pub snapshot: Arc<ArcSwap<Snapshot>>,
    pub notifications: broadcast::Sender<Notification>,
    pub commands: mpsc::Sender<super::command::Command>,
    pub auth_token: String,
}

pub async fn run(
    listener: TcpListener,
    state: Arc<State>,
    mut shutdown: mpsc::Receiver<()>,
) {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, addr)) => {
                        log::debug!("helix-claude-ide: incoming connection from {addr}");
                        let state = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(stream, state).await {
                                log::warn!("helix-claude-ide: client error: {e:#}");
                            }
                        });
                    }
                    Err(e) => {
                        log::warn!("helix-claude-ide: accept failed: {e}");
                    }
                }
            }
            _ = shutdown.recv() => {
                log::info!("helix-claude-ide: shutting down");
                break;
            }
        }
    }
}

async fn handle_client(stream: TcpStream, state: Arc<State>) -> anyhow::Result<()> {
    let expected = state.auth_token.clone();

    let ws = tokio_tungstenite::accept_hdr_async(
        stream,
        |req: &Request, resp: Response| match req.headers().get("x-claude-code-ide-authorization") {
            Some(v) if v.as_bytes() == expected.as_bytes() => Ok(resp),
            _ => {
                let mut r = ErrorResponse::new(Some("unauthorized".into()));
                *r.status_mut() = StatusCode::UNAUTHORIZED;
                Err(r)
            }
        },
    )
    .await?;

    let (mut sink, mut stream) = ws.split();
    let mut notif_rx = state.notifications.subscribe();

    loop {
        tokio::select! {
            msg = stream.next() => {
                let Some(msg) = msg else { break };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        log::debug!("helix-claude-ide: ws read error: {e}");
                        break;
                    }
                };
                match msg {
                    Message::Text(text) => {
                        if let Some(resp) = dispatch_text(&state, &text).await {
                            let s = serde_json::to_string(&resp)?;
                            sink.send(Message::Text(s.into())).await?;
                        }
                    }
                    Message::Binary(_) => {}
                    Message::Ping(p) => { sink.send(Message::Pong(p)).await?; }
                    Message::Pong(_) => {}
                    Message::Close(_) => break,
                    Message::Frame(_) => {}
                }
            }
            notif = notif_rx.recv() => {
                match notif {
                    Ok(n) => {
                        let wire = JsonRpcNotification {
                            jsonrpc: "2.0",
                            method: n.method(),
                            params: n.params(),
                        };
                        let s = serde_json::to_string(&wire)?;
                        sink.send(Message::Text(s.into())).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    Ok(())
}

async fn dispatch_text(state: &Arc<State>, text: &str) -> Option<JsonRpcResponse> {
    let req: JsonRpcRequest = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(e) => {
            log::debug!("helix-claude-ide: bad json-rpc: {e}; body={text}");
            return None;
        }
    };
    mcp::handle(state, req).await
}
