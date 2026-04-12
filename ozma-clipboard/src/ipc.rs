//! IPC server — exposes clipboard operations to the controller.
//!
//! Transport:
//!   Linux / macOS  — Unix domain socket  (`/tmp/ozma-clipboard.sock`)
//!   Windows        — Named pipe          (`\\.\pipe\ozma-clipboard`)
//!
//! Protocol: newline-delimited JSON.
//!
//! Request  → `{ "id": <u64>, "cmd": "<CMD>", ...params }`
//! Response → `{ "id": <u64>, "ok": true, ...fields }`
//!          | `{ "id": <u64>, "ok": false, "error": "<msg>" }`
//!
//! Commands:
//!   GET                          → { content }
//!   SET  { content }             → { ok }
//!   LIST { limit? }              → { entries }
//!   SEARCH { query }             → { entries }
//!   PIN    { id }                → { ok }
//!   UNPIN  { id }                → { ok }
//!   CLEAR                        → { ok }
//!   WATCH                        → stream of change events until disconnect

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error, info, warn};

use crate::{ClipboardContent, ClipboardManager};

// ── Request / Response types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Request {
    id: u64,
    cmd: String,
    #[serde(flatten)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct Response {
    id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(flatten)]
    data: Value,
}

impl Response {
    fn ok(id: u64, data: Value) -> Self {
        Self { id, ok: true, error: None, data }
    }
    fn err(id: u64, msg: impl Into<String>) -> Self {
        Self { id, ok: false, error: Some(msg.into()), data: Value::Null }
    }
}

// ── Platform socket path / name ───────────────────────────────────────────────

#[cfg(unix)]
pub const SOCKET_PATH: &str = "/tmp/ozma-clipboard.sock";

#[cfg(windows)]
pub const PIPE_NAME: &str = r"\\.\pipe\ozma-clipboard";

// ── Server entry point ────────────────────────────────────────────────────────

/// Start the IPC server.  Runs until the process exits.
pub async fn serve(mgr: Arc<ClipboardManager>) {
    #[cfg(unix)]
    serve_unix(mgr).await;

    #[cfg(windows)]
    serve_windows(mgr).await;
}

// ── Unix implementation ───────────────────────────────────────────────────────

#[cfg(unix)]
async fn serve_unix(mgr: Arc<ClipboardManager>) {
    use tokio::net::UnixListener;

    // Remove stale socket
    let _ = std::fs::remove_file(SOCKET_PATH);

    let listener = match UnixListener::bind(SOCKET_PATH) {
        Ok(l) => l,
        Err(e) => {
            error!("IPC: cannot bind {SOCKET_PATH}: {e}");
            return;
        }
    };
    info!("IPC: listening on {SOCKET_PATH}");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let mgr = Arc::clone(&mgr);
                tokio::spawn(async move {
                    let (reader, writer) = stream.into_split();
                    handle_connection(BufReader::new(reader), writer, mgr).await;
                });
            }
            Err(e) => warn!("IPC accept error: {e}"),
        }
    }
}

// ── Windows named-pipe implementation ────────────────────────────────────────

#[cfg(windows)]
async fn serve_windows(mgr: Arc<ClipboardManager>) {
    use tokio::net::windows::named_pipe::ServerOptions;

    loop {
        let server = match ServerOptions::new()
            .first_pipe_instance(false)
            .create(PIPE_NAME)
        {
            Ok(s) => s,
            Err(e) => {
                error!("IPC: cannot create pipe {PIPE_NAME}: {e}");
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                continue;
            }
        };

        if let Err(e) = server.connect().await {
            warn!("IPC pipe connect error: {e}");
            continue;
        }

        info!("IPC: client connected on {PIPE_NAME}");
        let mgr = Arc::clone(&mgr);
        tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(server);
            handle_connection(BufReader::new(reader), writer, mgr).await;
        });
    }
}

// ── Connection handler ────────────────────────────────────────────────────────

async fn handle_connection<R, W>(mut reader: BufReader<R>, mut writer: W, mgr: Arc<ClipboardManager>)
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                debug!("IPC read error: {e}");
                break;
            }
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::err(0, format!("parse error: {e}"));
                send_response(&mut writer, &resp).await;
                continue;
            }
        };

        debug!("IPC cmd={} id={}", req.cmd, req.id);

        match req.cmd.to_uppercase().as_str() {
            "GET" => {
                let resp = match mgr.get().await {
                    Some(content) => Response::ok(
                        req.id,
                        serde_json::json!({ "content": content }),
                    ),
                    None => Response::err(req.id, "clipboard empty or unavailable"),
                };
                send_response(&mut writer, &resp).await;
            }

            "SET" => {
                let content: ClipboardContent = match serde_json::from_value(req.params["content"].clone()) {
                    Ok(c) => c,
                    Err(e) => {
                        send_response(&mut writer, &Response::err(req.id, format!("bad content: {e}"))).await;
                        continue;
                    }
                };
                let resp = match mgr.set(content).await {
                    Ok(()) => Response::ok(req.id, Value::Null),
                    Err(e) => Response::err(req.id, e),
                };
                send_response(&mut writer, &resp).await;
            }

            "LIST" => {
                let limit = req.params["limit"].as_u64().unwrap_or(20) as usize;
                let entries = mgr.ring.lock().await.list(limit);
                send_response(
                    &mut writer,
                    &Response::ok(req.id, serde_json::json!({ "entries": entries })),
                )
                .await;
            }

            "SEARCH" => {
                let query = req.params["query"].as_str().unwrap_or("").to_owned();
                let entries = mgr.ring.lock().await.search(&query);
                send_response(
                    &mut writer,
                    &Response::ok(req.id, serde_json::json!({ "entries": entries })),
                )
                .await;
            }

            "PIN" => {
                let id = req.params["id"].as_u64().unwrap_or(0);
                let ok = mgr.ring.lock().await.pin(id);
                let resp = if ok {
                    Response::ok(req.id, Value::Null)
                } else {
                    Response::err(req.id, "entry not found")
                };
                send_response(&mut writer, &resp).await;
            }

            "UNPIN" => {
                let id = req.params["id"].as_u64().unwrap_or(0);
                mgr.ring.lock().await.unpin(id);
                send_response(&mut writer, &Response::ok(req.id, Value::Null)).await;
            }

            "CLEAR" => {
                mgr.ring.lock().await.clear();
                send_response(&mut writer, &Response::ok(req.id, Value::Null)).await;
            }

            "WATCH" => {
                // Acknowledge subscription
                send_response(&mut writer, &Response::ok(req.id, serde_json::json!({ "watching": true }))).await;

                // Stream change events until the client disconnects
                let mut rx = mgr.tx.subscribe();
                loop {
                    match rx.recv().await {
                        Ok(evt) => {
                            let line = match serde_json::to_string(&evt) {
                                Ok(s) => s,
                                Err(_) => continue,
                            };
                            if writer.write_all(line.as_bytes()).await.is_err() {
                                break;
                            }
                            if writer.write_all(b"\n").await.is_err() {
                                break;
                            }
                            let _ = writer.flush().await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("IPC WATCH: lagged by {n} events");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                break;
            }

            other => {
                send_response(&mut writer, &Response::err(req.id, format!("unknown command: {other}"))).await;
            }
        }
    }
}

async fn send_response<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, resp: &Response) {
    if let Ok(mut s) = serde_json::to_string(resp) {
        s.push('\n');
        if let Err(e) = writer.write_all(s.as_bytes()).await {
            debug!("IPC write error: {e}");
        }
        let _ = writer.flush().await;
    }
}
