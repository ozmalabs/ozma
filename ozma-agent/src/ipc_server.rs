//! IPC server for privileged communications.
//!
//! This server uses OS-specific mechanisms (Unix sockets on Linux/macOS, Named Pipes on Windows)
//! to provide a secure channel for approval and event traffic.

use crate::approvals::{ActionType, ApprovalConfig, ApprovalQueue, ApprovalRequest, ApprovalState, AgentEvent};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum IpcRequest {
    ListApprovals,
    Approve { id: Uuid },
    Deny { id: Uuid },
    GetConfig,
    SetConfig { modes: HashMap<ActionType, String> },
    Subscribe,
}

#[derive(Serialize)]
#[serde(untagged)]
enum IpcResponse {
    Approvals { approvals: Vec<ApprovalRequest> },
    Ok { ok: bool },
    Config { modes: HashMap<ActionType, String> },
    Error { error: String },
}

/// Starts the privileged IPC socket/pipe server.
pub async fn serve(queue: Arc<ApprovalQueue>) -> Result<()> {
    let path = socket_path();
    #[cfg(unix)]
    {
        let listener = bind_socket(&path).await?;
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let queue = queue.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(stream, queue).await {
                            tracing::error!(error = %e, "IPC client error");
                        }
                    });
                }
                Err(e) => tracing::error!(error = %e, "IPC accept error"),
            }
        }
    }

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

        loop {
            let server = match ServerOptions::new()
                .access_inbound(true)
                .access_outbound(true)
                .first_instance(true)
                .create(&path.to_string_lossy())
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to create named pipe, retrying...");
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    continue;
                }
            };

            let queue = queue.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_windows_client(server, queue).await {
                    tracing::error!(error = %e, "Windows IPC client error");
                }
            });
        }
    }

    Ok(())
}

/// Platform socket path.
pub fn socket_path() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        PathBuf::from("/run/ozma/agent.sock")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/var/run/ozma-agent.sock")
    }
    #[cfg(windows)]
    {
        PathBuf::from(r"\\.\pipe\ozma-agent")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        unimplemented!("Unsupported platform")
    }
}

#[cfg(unix)]
async fn bind_socket(path: &std::path::Path) -> Result<tokio::net::UnixListener> {
    // Remove stale socket if present
    if path.exists() {
        std::fs::remove_file(path).context("Failed to remove stale socket")?;
    }

    // Ensure directory exists
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).context("Failed to create socket directory")?;
        }
    }

    let listener = tokio::net::UnixListener::bind(path).context("Failed to bind Unix socket")?;

    // Set group ownership to "ozma" and mode 0660 so any user in the ozma group
    // can connect, but others cannot.
    // NOTE: In a real deployment, the systemd unit would handle RuntimeDirectory=ozma
    // and permissions would be managed by the service manager.
    // We look up the "ozma" group by name; if it doesn't exist, skip chown.
    if let Ok(ozma_gid) = lookup_ozma_gid() {
        use std::os::unix::fs::PermissionsExt;
        nix::unistd::chown(path, None, Some(ozma_gid)).context("Failed to chown socket")?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
            .context("Failed to set socket permissions")?;
    } else {
        // Fallback: set mode 0660 without group change
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
            .context("Failed to set socket permissions")?;
        tracing::warn!("ozma group not found — socket mode is 0660 but group ownership not set");
    }

    Ok(listener)
}

#[cfg(unix)]
fn lookup_ozma_gid() -> Result<nix::unistd::Gid, std::io::Error> {
    use std::ffi::CString;
    // getgrnam_r is thread-safe; fall back to getgrnam if unavailable
    let cstr = CString::new("ozma").map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid group name")
    })?;
    let mut grp = std::mem::MaybeUninit::<libc::group>::uninit();
    let mut buf: Vec<u8> = vec![0; 16384];
    let result = unsafe {
        libc::getgrnam_r(
            cstr.as_ptr() as *const libc::c_char,
            grp.as_mut_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut *(&mut grp as *mut _ as *mut *mut libc::group),
        )
    };
    if result == 0 {
        let group = unsafe { grp.assume_init() };
        Ok(nix::unistd::Gid::from_raw(group.gr_gid))
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "ozma group not found",
        ))
    }
}

async fn handle_client<S>(mut stream: S, queue: Arc<ApprovalQueue>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; 4096];

    loop {
        // Read 4-byte length prefix
        let mut len_buf = [0u8; 4];
        if stream.read_exact(&mut len_buf).await.is_err() {
            break; // Client disconnected
        }
        let len = u32::from_le_bytes(len_buf) as usize;

        if len > buffer.len() {
            buffer.resize(len, 0);
        }

        stream.read_exact(&mut buffer[..len]).await?;
        let request: IpcRequest = serde_json::from_slice(&buffer[..len])?;

        match request {
            IpcRequest::ListApprovals => {
                let approvals = queue.list_pending().await;
                send_response(&mut stream, IpcResponse::Approvals { approvals }).await?;
            }
            IpcRequest::Approve { id } => {
                if queue.resolve(id, ApprovalState::Approved).await.is_some() {
                    send_response(&mut stream, IpcResponse::Ok { ok: true }).await?;
                } else {
                    send_response(&mut stream, IpcResponse::Error { error: "Not found".to_string() }).await?;
                }
            }
            IpcRequest::Deny { id } => {
                if queue.resolve(id, ApprovalState::Denied).await.is_some() {
                    send_response(&mut stream, IpcResponse::Ok { ok: true }).await?;
                } else {
                    send_response(&mut stream, IpcResponse::Error { error: "Not found".to_string() }).await?;
                }
            }
            IpcRequest::GetConfig => {
                let config = queue.get_config().await;
                send_response(&mut stream, IpcResponse::Config { modes: config.modes }).await?;
            }
            IpcRequest::SetConfig { modes } => {
                queue.set_config(ApprovalConfig { modes }).await;
                send_response(&mut stream, IpcResponse::Ok { ok: true }).await?;
            }
            IpcRequest::Subscribe => {
                let mut rx = queue.tx.subscribe();
                // Send subscription success
                send_response(&mut stream, IpcResponse::Ok { ok: true }).await?;

                loop {
                    match rx.recv().await {
                        Ok(event) => {
                            let json = serde_json::to_vec(&event)?;
                            let len = (json.len() as u32).to_le_bytes();
                            stream.write_all(&len).await?;
                            stream.write_all(&json).await?;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
                return Ok(());
            }
        }
    }

    Ok(())
}

#[cfg(windows)]
async fn handle_windows_client(
    mut server: tokio::net::windows::named_pipe::NamedPipeServer,
    queue: Arc<ApprovalQueue>,
) -> Result<()> {
    server.connect().await.context("Windows named pipe connect")?;
    let mut buffer = vec![0u8; 4096];

    loop {
        // Read 4-byte length prefix
        let mut len_buf = [0u8; 4];
        match server.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_le_bytes(len_buf) as usize;

        if len > buffer.len() {
            buffer.resize(len, 0);
        }

        server.read_exact(&mut buffer[..len]).await?;
        let request: IpcRequest = serde_json::from_slice(&buffer[..len])?;

        match request {
            IpcRequest::ListApprovals => {
                let approvals = queue.list_pending().await;
                send_response_win(&mut server, IpcResponse::Approvals { approvals }).await?;
            }
            IpcRequest::Approve { id } => {
                if queue.resolve(id, ApprovalState::Approved).await.is_some() {
                    send_response_win(&mut server, IpcResponse::Ok { ok: true }).await?;
                } else {
                    send_response_win(&mut server, IpcResponse::Error { error: "Not found".to_string() }).await?;
                }
            }
            IpcRequest::Deny { id } => {
                if queue.resolve(id, ApprovalState::Denied).await.is_some() {
                    send_response_win(&mut server, IpcResponse::Ok { ok: true }).await?;
                } else {
                    send_response_win(&mut server, IpcResponse::Error { error: "Not found".to_string() }).await?;
                }
            }
            IpcRequest::GetConfig => {
                let config = queue.get_config().await;
                send_response_win(&mut server, IpcResponse::Config { modes: config.modes }).await?;
            }
            IpcRequest::SetConfig { modes } => {
                queue.set_config(ApprovalConfig { modes }).await;
                send_response_win(&mut server, IpcResponse::Ok { ok: true }).await?;
            }
            IpcRequest::Subscribe => {
                let mut rx = queue.tx.subscribe();
                send_response_win(&mut server, IpcResponse::Ok { ok: true }).await?;

                loop {
                    match rx.recv().await {
                        Ok(event) => {
                            let json = serde_json::to_vec(&event)?;
                            let len = (json.len() as u32).to_le_bytes();
                            server.write_all(&len).await?;
                            server.write_all(&json).await?;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(windows)]
async fn send_response_win<S>(stream: &mut S, response: IpcResponse) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let json = serde_json::to_vec(&response)?;
    let len = (json.len() as u32).to_le_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&json).await?;
    Ok(())
}

async fn send_response<S>(stream: &mut S, response: IpcResponse) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let json = serde_json::to_vec(&response)?;
    let len = (json.len() as u32).to_le_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&json).await?;
    Ok(())
}
