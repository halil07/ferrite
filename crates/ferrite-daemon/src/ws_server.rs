#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use ferrite_core::{WsEvent, WsRequest};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use url::Url;

use crate::auth::constant_time_eq;
use crate::session::E2eSession;

/// Localhost-only WebSocket RPC gateway hardened against Cross-Site WebSocket Hijacking (CSWSH)
/// and unauthorized local access via 256-bit bearer tokens.
pub struct WsServer {
    addr: SocketAddr,
    shutdown_tx: watch::Sender<bool>,
}

impl WsServer {
    pub async fn bind(
        port: u16,
        session: Arc<E2eSession>,
        token: String,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let addr = listener.local_addr()?;
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, peer)) => {
                                let sess = session.clone();
                                let tok = token.clone();
                                tokio::spawn(async move {
                                    if let Err(err) = handle_connection(stream, sess, tok).await {
                                        tracing::debug!(%peer, "WebSocket client closed: {err}");
                                    }
                                });
                            }
                            Err(err) => {
                                tracing::warn!("TCP accept error: {err}");
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(Self { addr, shutdown_tx })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.addr.port())
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

impl Drop for WsServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn reject_http(status: StatusCode, body: &str) -> ErrorResponse {
    Response::builder()
        .status(status)
        .body(Some(body.to_string()))
        .unwrap_or_else(|_| ErrorResponse::new(Some(body.to_string())))
}

fn extract_bearer_or_query_token(req: &Request) -> Option<String> {
    if let Some(auth_val) = req.headers().get("Authorization")
        && let Ok(auth_str) = auth_val.to_str()
        && let Some(token) = auth_str.strip_prefix("Bearer ")
    {
        return Some(token.trim().to_string());
    }

    let uri_str = req.uri().to_string();
    let full = format!("http://localhost{uri_str}");
    if let Ok(parsed) = Url::parse(&full) {
        for (k, v) in parsed.query_pairs() {
            if k == "token" {
                return Some(v.into_owned());
            }
        }
    }
    None
}

async fn handle_connection(
    stream: TcpStream,
    session: Arc<E2eSession>,
    expected_token: String,
) -> anyhow::Result<()> {
    let ws_stream = accept_hdr_async(stream, |req: &Request, response: Response| {
        // CSWSH protection: reject any browser-originated WebSocket handshake carrying an `Origin` header.
        if req.headers().contains_key("Origin") {
            return Err(reject_http(
                StatusCode::FORBIDDEN,
                "CSWSH rejected: browser Origin header is forbidden",
            ));
        }

        let provided = extract_bearer_or_query_token(req);
        let authorized = provided
            .as_deref()
            .is_some_and(|tok| constant_time_eq(tok.as_bytes(), expected_token.as_bytes()));

        if !authorized {
            return Err(reject_http(
                StatusCode::UNAUTHORIZED,
                "missing or invalid bearer token",
            ));
        }

        Ok(response)
    })
    .await?;

    let (mut sink, mut source) = ws_stream.split();
    let mut events_rx = session.transport().subscribe_events();

    loop {
        tokio::select! {
            incoming = source.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let req: WsRequest = match serde_json::from_str(&text) {
                            Ok(r) => r,
                            Err(err) => {
                                let err_msg = serde_json::to_string(&ferrite_core::WsResponse {
                                    id: 0,
                                    ok: false,
                                    result: None,
                                    error: Some(format!("malformed RPC request: {err}")),
                                })?;
                                sink.send(Message::Text(err_msg.into())).await?;
                                continue;
                            }
                        };
                        let resp = session.dispatch_rpc(&req).await;
                        sink.send(Message::Text(serde_json::to_string(&resp)?.into())).await?;
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        sink.send(Message::Pong(payload)).await?;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(err)) => return Err(err.into()),
                }
            }
            ev = events_rx.recv() => {
                match ev {
                    Ok(cdp_ev) => {
                        let ws_ev = WsEvent {
                            event: cdp_ev.method,
                            payload: serde_json::json!({
                                "tabId": cdp_ev.tab_id,
                                "sessionId": cdp_ev.session_id,
                                "params": cdp_ev.params,
                            }),
                        };
                        sink.send(Message::Text(serde_json::to_string(&ws_ev)?.into())).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    Ok(())
}
