use std::{
    io::ErrorKind,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::Notify,
    task::JoinHandle,
};
use tracing::warn;

use crate::{
    model::Side,
    state::{client_socket_path, client_status_path, write_file_atomic},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClientLifecycleState {
    Starting,
    WaitingForHost,
    Authenticating,
    Connected,
    Stopping,
    Disconnected,
    WrongSecret,
    SideAlreadyInUse,
    NeedsPermission,
    CleanupUnconfirmed,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClientStatusPayload {
    pub(crate) host_id: String,
    pub(crate) side: Side,
    #[serde(default)]
    pub(crate) instance_id: String,
    pub(crate) state: ClientLifecycleState,
    pub(crate) message: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum ClientIpcCommand {
    Status,
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
struct ClientIpcResponse {
    ok: bool,
    message: String,
    status: Option<ClientStatusPayload>,
}

const IPC_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_IPC_MESSAGE_SIZE: usize = 64 * 1024;

pub(crate) struct ClientControl {
    path: PathBuf,
    socket_identity: Option<SocketIdentity>,
    status_path: PathBuf,
    status: Arc<Mutex<ClientStatusPayload>>,
    stop_notify: Arc<Notify>,
    shutdown_notify: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

impl ClientControl {
    pub(crate) async fn start(host_id: String, side: Side) -> Result<Self> {
        let path = client_socket_path(&host_id, side)?;
        let status_path = client_status_path(&host_id, side)?;
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("missing client socket parent"))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;

        if path.exists() {
            match tokio::time::timeout(IPC_TIMEOUT, UnixStream::connect(&path)).await {
                Ok(Ok(_)) => bail!("client attachment is already running"),
                Ok(Err(err)) if stale_socket_error(&err) => {
                    std::fs::remove_file(&path).with_context(|| {
                        format!("failed to remove stale client socket {}", path.display())
                    })?;
                }
                Ok(Err(err)) => {
                    bail!(
                        "client socket is unavailable and cannot be proven stale ({}): {err}",
                        path.display()
                    );
                }
                Err(_) => {
                    bail!(
                        "client socket did not respond and cannot be proven stale ({})",
                        path.display()
                    );
                }
            }
        }

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("failed to bind client socket {}", path.display()))?;
        let socket_identity = socket_identity(&path);
        let instance_id = uuid::Uuid::new_v4().simple().to_string();
        let initial_status = ClientStatusPayload {
            host_id,
            side,
            instance_id,
            state: ClientLifecycleState::Starting,
            message: "client attach is starting".to_string(),
        };
        write_status(&status_path, &initial_status)?;
        let status = Arc::new(Mutex::new(initial_status));
        let stop_notify = Arc::new(Notify::new());
        let shutdown_notify = Arc::new(Notify::new());
        let task_status = status.clone();
        let task_stop = stop_notify.clone();
        let task_shutdown = shutdown_notify.clone();
        let task = tokio::spawn(async move {
            run_control_socket(listener, task_status, task_stop, task_shutdown).await;
        });

        Ok(Self {
            path,
            socket_identity,
            status_path,
            status,
            stop_notify,
            shutdown_notify,
            task: Some(task),
        })
    }

    pub(crate) async fn set_state(&self, state: ClientLifecycleState, message: impl Into<String>) {
        let Ok(mut status) = self.status.lock() else {
            warn!("failed to lock client lifecycle state");
            return;
        };
        status.state = state;
        status.message = message.into();
        if let Err(err) = write_status(&self.status_path, &status) {
            warn!("failed to persist client lifecycle state: {err:#}");
        }
    }

    pub(crate) async fn stop_requested(&self) {
        self.stop_notify.notified().await;
    }
}

impl Drop for ClientControl {
    fn drop(&mut self) {
        self.shutdown_notify.notify_waiters();
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Ok(mut status) = self.status.lock()
            && matches!(
                status.state,
                ClientLifecycleState::Starting
                    | ClientLifecycleState::WaitingForHost
                    | ClientLifecycleState::Authenticating
                    | ClientLifecycleState::Stopping
            )
        {
            status.state = ClientLifecycleState::Stopped;
            status.message = "client attach exited before reaching a terminal state".to_string();
            let _ = write_status(&self.status_path, &status);
        }
        if socket_is_owned(&self.path, self.socket_identity) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn run_control_socket(
    listener: UnixListener,
    status: Arc<Mutex<ClientStatusPayload>>,
    stop_notify: Arc<Notify>,
    shutdown_notify: Arc<Notify>,
) {
    loop {
        let accepted = tokio::select! {
            _ = shutdown_notify.notified() => break,
            result = listener.accept() => result,
        };
        let Ok((mut stream, _)) = accepted else {
            break;
        };

        let bytes = match read_ipc_message(&mut stream).await {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let response = match serde_json::from_slice::<ClientIpcCommand>(&bytes) {
            Ok(ClientIpcCommand::Status) => ClientIpcResponse {
                ok: true,
                message: "client status".to_string(),
                status: status.lock().ok().map(|status| status.clone()),
            },
            Ok(ClientIpcCommand::Stop) => {
                if let Ok(mut current) = status.lock() {
                    current.state = ClientLifecycleState::Stopping;
                    current.message = "client stop requested".to_string();
                }
                let current = status.lock().ok().map(|status| status.clone());
                let Some(current) = current else {
                    continue;
                };
                if let Ok(path) = client_status_path(&current.host_id, current.side)
                    && let Err(err) = write_status(&path, &current)
                {
                    warn!("failed to persist client stop state: {err:#}");
                }
                stop_notify.notify_one();
                ClientIpcResponse {
                    ok: true,
                    message: "stopping client attachment".to_string(),
                    status: Some(current),
                }
            }
            Err(err) => ClientIpcResponse {
                ok: false,
                message: format!("invalid client control request: {err}"),
                status: None,
            },
        };

        if let Ok(payload) = serde_json::to_vec(&response) {
            match tokio::time::timeout(IPC_TIMEOUT, stream.write_all(&payload)).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => warn!("failed writing client control response: {err}"),
                Err(err) => warn!("failed writing client control response: {err}"),
            }
        }
    }
}

fn write_status(path: &std::path::Path, status: &ClientStatusPayload) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(status)?;
    write_file_atomic(path, &bytes)
}

pub(crate) async fn request_status(path: PathBuf) -> Result<ClientStatusPayload> {
    let response = request(path, ClientIpcCommand::Status).await?;
    if !response.ok {
        bail!(response.message);
    }
    response
        .status
        .ok_or_else(|| anyhow::anyhow!("client status response was missing status"))
}

pub(crate) async fn request_stop(path: PathBuf) -> Result<()> {
    let response = request(path, ClientIpcCommand::Stop).await?;
    if response.ok {
        Ok(())
    } else {
        bail!(response.message)
    }
}

pub(crate) fn load_persisted_status(
    host_id: &str,
    side: Side,
) -> Result<Option<ClientStatusPayload>> {
    let path = client_status_path(host_id, side)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

async fn request(path: PathBuf, command: ClientIpcCommand) -> Result<ClientIpcResponse> {
    let mut stream = tokio::time::timeout(IPC_TIMEOUT, UnixStream::connect(&path))
        .await
        .with_context(|| {
            format!(
                "timed out connecting to client attachment ({})",
                path.display()
            )
        })?
        .with_context(|| format!("client attachment is not running ({})", path.display()))?;
    let bytes = serde_json::to_vec(&command)?;
    tokio::time::timeout(IPC_TIMEOUT, stream.write_all(&bytes))
        .await
        .context("timed out writing client control request")??;
    tokio::time::timeout(IPC_TIMEOUT, stream.shutdown())
        .await
        .context("timed out closing client control request")??;
    let response_bytes = read_ipc_message(&mut stream).await?;
    Ok(serde_json::from_slice(&response_bytes)?)
}

async fn read_ipc_message(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let result = tokio::time::timeout(IPC_TIMEOUT, async {
        let mut bytes = Vec::new();
        let mut limited = stream.take((MAX_IPC_MESSAGE_SIZE + 1) as u64);
        limited.read_to_end(&mut bytes).await?;
        Ok::<_, std::io::Error>(bytes)
    })
    .await
    .context("timed out reading client control request")??;
    if result.len() > MAX_IPC_MESSAGE_SIZE {
        bail!("client control message is too large");
    }
    Ok(result)
}

fn stale_socket_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::ConnectionRefused | ErrorKind::NotFound
    )
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

#[cfg(not(unix))]
type SocketIdentity = ();

fn socket_identity(path: &std::path::Path) -> Option<SocketIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).ok()?;
        Some(SocketIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn socket_is_owned(path: &std::path::Path, expected: Option<SocketIdentity>) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Some(expected) = expected else {
            return false;
        };
        let Some(metadata) = std::fs::metadata(path).ok() else {
            return false;
        };
        metadata.dev() == expected.device && metadata.ino() == expected.inode
    }
    #[cfg(not(unix))]
    {
        let _ = (path, expected);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_socket_cleanup_only_accepts_proven_connection_failures() {
        assert!(stale_socket_error(&std::io::Error::from(
            ErrorKind::ConnectionRefused,
        )));
        assert!(stale_socket_error(&std::io::Error::from(
            ErrorKind::NotFound
        )));
        assert!(!stale_socket_error(&std::io::Error::from(
            ErrorKind::TimedOut
        )));
        assert!(!stale_socket_error(&std::io::Error::from(
            ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn legacy_client_status_without_instance_id_remains_readable() {
        let status: ClientStatusPayload = serde_json::from_value(serde_json::json!({
            "host_id": "host",
            "side": "right",
            "state": "connected",
            "message": "connected"
        }))
        .expect("legacy client status should deserialize");

        assert!(status.instance_id.is_empty());
    }
}
