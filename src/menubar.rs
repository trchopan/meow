use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    attach,
    cli::HostArgs,
    client_ipc::{
        ClientLifecycleState, ClientStatusPayload, load_persisted_status,
        request_status as request_client_status, request_stop as request_client_stop,
    },
    clipboard, host,
    input::{DEFAULT_EDGE_DWELL_MS, DEFAULT_EDGE_ZONE_PX},
    ipc::{AttachedPeerStatus, IpcCommand, StatusPayload, request_ipc},
    macos_permissions,
    model::{ActiveTarget, RemotePointerMode, Side},
    state::{format_attach_command, load_client_profile, load_existing_host_credentials},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostMenuState {
    Stopped,
    Starting,
    Running,
    NeedsPermission,
    Degraded,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostMenuStatus {
    pub(crate) state: HostMenuState,
    pub(crate) endpoint_id: Option<String>,
    pub(crate) active: Option<ActiveTarget>,
    pub(crate) pointer_mode: Option<RemotePointerMode>,
    pub(crate) attached_peers: Vec<AttachedPeerStatus>,
    pub(crate) message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClientMenuState {
    NotConfigured,
    Stopped,
    Starting,
    WaitingForHost,
    Authenticating,
    Connected(Side),
    Disconnected,
    Stopping,
    NeedsPermission,
    WrongSecret,
    SideAlreadyInUse,
    CleanupUnconfirmed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientMenuStatus {
    pub(crate) state: ClientMenuState,
    pub(crate) message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MenuSnapshot {
    pub(crate) host: HostMenuStatus,
    pub(crate) client: ClientMenuStatus,
    pub(crate) credentials_available: bool,
    pub(crate) last_error: Option<String>,
    pub(crate) notice: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum MenuCommand {
    StartHost,
    StopHost,
    StartClient,
    ReconnectClient,
    StopClient,
    CopyInvitation { side: Side },
    Refresh,
    Shutdown,
}

pub(crate) struct MenuSupervisor {
    host_task: Option<JoinHandle<Result<()>>>,
    client_task: Option<JoinHandle<Result<()>>>,
    last_error: Option<String>,
    notice: Option<String>,
}

impl MenuSupervisor {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            host_task: None,
            client_task: None,
            last_error: None,
            notice: None,
        })
    }

    pub(crate) async fn refresh(&mut self) -> MenuSnapshot {
        self.reap_tasks().await;
        let host = self.refresh_host().await;
        let client = self.refresh_client().await;
        let credentials_available = load_existing_host_credentials().ok().flatten().is_some();
        MenuSnapshot {
            host,
            client,
            credentials_available,
            last_error: self.last_error.clone(),
            notice: self.notice.take(),
        }
    }

    pub(crate) async fn execute(&mut self, command: MenuCommand) -> Result<()> {
        self.last_error = None;
        match command {
            MenuCommand::StartHost => self.start_host().await?,
            MenuCommand::StopHost => self.stop_host().await?,
            MenuCommand::StartClient => self.start_client().await?,
            MenuCommand::ReconnectClient => {
                self.ensure_client_stopped().await?;
                self.start_client().await?;
            }
            MenuCommand::StopClient => self.stop_client().await?,
            MenuCommand::CopyInvitation { side } => self.copy_invitation(side)?,
            MenuCommand::Refresh => {}
            MenuCommand::Shutdown => {
                self.shutdown().await;
                return Ok(());
            }
        }
        Ok(())
    }

    pub(crate) async fn run(
        mut self,
        mut commands: mpsc::Receiver<MenuCommand>,
        snapshots: mpsc::Sender<MenuSnapshot>,
    ) {
        let _ = snapshots.send(self.refresh().await).await;
        let mut refresh_tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    if let Err(err) = self.execute(command).await {
                        self.last_error = Some(safe_error(&err));
                    }
                    if matches!(command, MenuCommand::Shutdown) {
                        break;
                    }
                    let _ = snapshots.send(self.refresh().await).await;
                }
                _ = refresh_tick.tick() => {
                    let _ = snapshots.send(self.refresh().await).await;
                }
            }
        }
    }

    async fn shutdown(&mut self) {
        let _ = self.ensure_client_stopped().await;
        let _ = self.stop_host().await;
        self.abort_unfinished_tasks();
    }

    async fn refresh_host(&mut self) -> HostMenuStatus {
        match request_ipc(IpcCommand::Status).await {
            Ok(response) if response.ok => response
                .status
                .map(host_status_from_payload)
                .unwrap_or_else(|| HostMenuStatus {
                    state: HostMenuState::Error,
                    endpoint_id: None,
                    active: None,
                    pointer_mode: None,
                    attached_peers: Vec::new(),
                    message: Some("host status response was missing status".to_string()),
                }),
            Ok(response) => HostMenuStatus {
                state: HostMenuState::Error,
                endpoint_id: None,
                active: None,
                pointer_mode: None,
                attached_peers: Vec::new(),
                message: Some(safe_message(&response.message)),
            },
            Err(_) => {
                if self.host_task.is_some() {
                    return HostMenuStatus {
                        state: HostMenuState::Starting,
                        endpoint_id: None,
                        active: None,
                        pointer_mode: None,
                        attached_peers: Vec::new(),
                        message: Some("waiting for host daemon".to_string()),
                    };
                }
                let permissions = macos_permissions::check_host_permissions();
                if !permissions.accessibility || !permissions.input_monitoring {
                    HostMenuStatus {
                        state: HostMenuState::NeedsPermission,
                        endpoint_id: None,
                        active: None,
                        pointer_mode: None,
                        attached_peers: Vec::new(),
                        message: Some(host_permission_message(&permissions)),
                    }
                } else {
                    HostMenuStatus {
                        state: HostMenuState::Stopped,
                        endpoint_id: None,
                        active: None,
                        pointer_mode: None,
                        attached_peers: Vec::new(),
                        message: None,
                    }
                }
            }
        }
    }

    async fn refresh_client(&mut self) -> ClientMenuStatus {
        let profile = match load_client_profile() {
            Ok(profile) => profile,
            Err(err) => {
                return ClientMenuStatus {
                    state: ClientMenuState::Failed,
                    message: Some(safe_error(&err)),
                };
            }
        };
        let Some(profile) = profile else {
            return ClientMenuStatus {
                state: ClientMenuState::NotConfigured,
                message: None,
            };
        };
        let socket = match crate::state::client_socket_path(&profile.host_id, profile.side) {
            Ok(path) => path,
            Err(err) => {
                return ClientMenuStatus {
                    state: ClientMenuState::Failed,
                    message: Some(safe_error(&err)),
                };
            }
        };
        match request_client_status(socket).await {
            Ok(status) if status.host_id == profile.host_id && status.side == profile.side => {
                client_status_from_payload(status)
            }
            Ok(_) => ClientMenuStatus {
                state: ClientMenuState::Failed,
                message: Some("client status did not match the saved profile".to_string()),
            },
            Err(_) => {
                if let Ok(Some(status)) = load_persisted_status(&profile.host_id, profile.side)
                    && status.host_id == profile.host_id
                    && status.side == profile.side
                    && (!matches!(
                        status.state,
                        ClientLifecycleState::Starting
                            | ClientLifecycleState::WaitingForHost
                            | ClientLifecycleState::Authenticating
                            | ClientLifecycleState::Stopping
                    ) || self.client_task.is_some())
                {
                    return client_status_from_payload(status);
                }
                if self.client_task.is_some() {
                    return ClientMenuStatus {
                        state: ClientMenuState::Starting,
                        message: Some("starting client attachment".to_string()),
                    };
                }
                let permissions = macos_permissions::check_client_permissions();
                if !permissions.accessibility {
                    ClientMenuStatus {
                        state: ClientMenuState::NeedsPermission,
                        message: Some(format!(
                            "client Accessibility permission is missing for {}",
                            macos_permissions::permission_target_for_executable(
                                &std::env::current_exe().unwrap_or_default(),
                            )
                            .display()
                        )),
                    }
                } else {
                    ClientMenuStatus {
                        state: ClientMenuState::Stopped,
                        message: None,
                    }
                }
            }
        }
    }

    async fn start_host(&mut self) -> Result<()> {
        if request_ipc(IpcCommand::Status)
            .await
            .is_ok_and(|response| response.ok)
        {
            return Ok(());
        }
        if self.host_task.is_some() {
            bail!("host is already starting");
        }
        let permissions = macos_permissions::check_host_permissions();
        if !permissions.accessibility || !permissions.input_monitoring {
            bail!(host_permission_message(&permissions));
        }
        self.host_task = Some(tokio::spawn(host::run_host_in_process(HostArgs {
            edge_zone_px: DEFAULT_EDGE_ZONE_PX,
            edge_dwell_ms: DEFAULT_EDGE_DWELL_MS,
        })));
        let ready = wait_until(Duration::from_secs(5), || async {
            request_ipc(IpcCommand::Status)
                .await
                .is_ok_and(|response| response.ok && response.status.is_some())
        })
        .await;
        if !ready {
            self.reap_tasks().await;
            bail!("host daemon did not become ready within 5 seconds");
        }
        Ok(())
    }

    async fn stop_host(&mut self) -> Result<()> {
        let response = request_ipc(IpcCommand::Stop).await?;
        if !response.ok {
            bail!(safe_message(&response.message));
        }
        let socket = crate::state::socket_path()?;
        let stopped = wait_until(Duration::from_secs(5), || {
            let socket = socket.clone();
            async move { !socket.exists() || request_ipc(IpcCommand::Status).await.is_err() }
        })
        .await;
        if !stopped {
            bail!("host shutdown was not confirmed");
        }
        self.await_host_task(Duration::from_secs(5)).await?;
        Ok(())
    }

    async fn start_client(&mut self) -> Result<()> {
        let profile =
            load_client_profile()?.ok_or_else(|| anyhow!("client profile is not configured"))?;
        let socket = crate::state::client_socket_path(&profile.host_id, profile.side)?;
        if request_client_status(socket.clone())
            .await
            .is_ok_and(|status| status.host_id == profile.host_id && status.side == profile.side)
        {
            return Ok(());
        }
        if self.client_task.is_some() {
            bail!("client is already starting");
        }
        if !macos_permissions::check_client_permissions().accessibility {
            bail!("client Accessibility permission is missing");
        }
        let profile_path = crate::state::client_profile_path()?;
        self.client_task = Some(tokio::task::spawn_local(
            attach::run_attach_profile_in_process(profile_path),
        ));
        let socket = crate::state::client_socket_path(&profile.host_id, profile.side)?;
        let ready = wait_until(Duration::from_secs(5), || {
            let socket = socket.clone();
            async move { request_client_status(socket).await.is_ok() }
        })
        .await;
        if !ready {
            self.reap_tasks().await;
            bail!("client attachment did not become ready within 5 seconds");
        }
        Ok(())
    }

    async fn stop_client(&mut self) -> Result<()> {
        self.ensure_client_stopped().await
    }

    async fn ensure_client_stopped(&mut self) -> Result<()> {
        let profile =
            load_client_profile()?.ok_or_else(|| anyhow!("client profile is not configured"))?;
        let socket = crate::state::client_socket_path(&profile.host_id, profile.side)?;
        if request_client_status(socket.clone()).await.is_ok() {
            request_client_stop(socket.clone()).await?;
            let stopped = wait_until(Duration::from_secs(5), || {
                let socket = socket.clone();
                async move { request_client_status(socket).await.is_err() }
            })
            .await;
            if !stopped {
                bail!("client cleanup was not confirmed");
            }
        }

        self.await_client_task(Duration::from_secs(5)).await?;
        Ok(())
    }

    fn copy_invitation(&mut self, side: Side) -> Result<()> {
        let credentials = load_existing_host_credentials()?
            .ok_or_else(|| anyhow!("host credentials are not configured"))?;
        let command =
            format_attach_command(credentials.endpoint_id, &credentials.attach_secret, side);
        clipboard::write_text(&command)?;
        self.notice = Some(format!("{} invitation copied", side_label(side)));
        Ok(())
    }

    async fn reap_tasks(&mut self) {
        if self.host_task.as_ref().is_some_and(JoinHandle::is_finished) {
            let task = self.host_task.take().expect("host task was present");
            if let Ok(Err(err)) = task.await {
                self.last_error = Some(safe_error(&err));
            }
        }
        if self
            .client_task
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            let task = self.client_task.take().expect("client task was present");
            if let Ok(Err(err)) = task.await {
                self.last_error = Some(safe_error(&err));
            }
        }
    }

    async fn await_host_task(&mut self, timeout: Duration) -> Result<()> {
        let Some(task) = self.host_task.take() else {
            return Ok(());
        };
        match tokio::time::timeout(timeout, task).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(err))) => Err(err),
            Ok(Err(err)) => Err(anyhow!("host runtime task failed: {err}")),
            Err(_) => bail!("host cleanup was not confirmed"),
        }
    }

    async fn await_client_task(&mut self, timeout: Duration) -> Result<()> {
        let Some(task) = self.client_task.take() else {
            return Ok(());
        };
        match tokio::time::timeout(timeout, task).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(err))) => Err(err),
            Ok(Err(err)) => Err(anyhow!("client runtime task failed: {err}")),
            Err(_) => bail!("client cleanup was not confirmed"),
        }
    }

    fn abort_unfinished_tasks(&mut self) {
        if let Some(task) = self.host_task.take() {
            task.abort();
        }
        if let Some(task) = self.client_task.take() {
            task.abort();
        }
    }
}

async fn wait_until<F, Fut>(timeout: Duration, mut condition: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if condition().await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn host_status_from_payload(payload: StatusPayload) -> HostMenuStatus {
    let degraded = !payload.pointer_tap_healthy || payload.capture_tap_stopped > 0;
    HostMenuStatus {
        state: if degraded {
            HostMenuState::Degraded
        } else {
            HostMenuState::Running
        },
        endpoint_id: Some(payload.endpoint_id),
        active: Some(payload.active),
        pointer_mode: Some(payload.pointer_mode),
        attached_peers: payload.attached_peers,
        message: degraded.then(|| "pointer capture health is degraded".to_string()),
    }
}

fn client_status_from_payload(payload: ClientStatusPayload) -> ClientMenuStatus {
    let state = match payload.state {
        ClientLifecycleState::Starting => ClientMenuState::Starting,
        ClientLifecycleState::WaitingForHost => ClientMenuState::WaitingForHost,
        ClientLifecycleState::Authenticating => ClientMenuState::Authenticating,
        ClientLifecycleState::Connected => ClientMenuState::Connected(payload.side),
        ClientLifecycleState::Stopping => ClientMenuState::Stopping,
        ClientLifecycleState::Disconnected => ClientMenuState::Disconnected,
        ClientLifecycleState::WrongSecret => ClientMenuState::WrongSecret,
        ClientLifecycleState::SideAlreadyInUse => ClientMenuState::SideAlreadyInUse,
        ClientLifecycleState::NeedsPermission => ClientMenuState::NeedsPermission,
        ClientLifecycleState::CleanupUnconfirmed => ClientMenuState::CleanupUnconfirmed,
        ClientLifecycleState::Failed => ClientMenuState::Failed,
        ClientLifecycleState::Stopped => ClientMenuState::Stopped,
    };
    ClientMenuStatus {
        state,
        message: (!payload.message.is_empty()).then_some(payload.message),
    }
}

fn side_label(side: Side) -> &'static str {
    match side {
        Side::Left => "Left",
        Side::Right => "Right",
        Side::Up => "Up",
        Side::Down => "Down",
    }
}

fn safe_message(message: &str) -> String {
    if message.to_ascii_lowercase().contains("secret") {
        "operation failed without exposing credentials".to_string()
    } else {
        message.to_string()
    }
}

fn safe_error(error: &anyhow::Error) -> String {
    safe_message(&error.to_string())
}

fn host_permission_message(permissions: &macos_permissions::PermissionStatus) -> String {
    let missing = permissions.missing_host_permissions().join(" and ");
    let permission_target = std::env::current_exe()
        .map(|path| macos_permissions::permission_target_for_executable(&path))
        .unwrap_or_default();
    format!(
        "missing host permissions: {missing} (permission target: {}); add this app to System Settings",
        permission_target.display()
    )
}

fn profile_save_command() -> MenuCommand {
    MenuCommand::StartClient
}

fn host_status_label(snapshot: &MenuSnapshot) -> String {
    match snapshot.host.state {
        HostMenuState::Stopped => "This Mac - Host: Stopped".to_string(),
        HostMenuState::Starting => "This Mac - Host: Starting".to_string(),
        HostMenuState::Running => "This Mac - Host: Running".to_string(),
        HostMenuState::NeedsPermission => "This Mac - Host: Needs Permission".to_string(),
        HostMenuState::Degraded => "This Mac - Host: Degraded".to_string(),
        HostMenuState::Error => "This Mac - Host: Error".to_string(),
    }
}

fn client_status_label(snapshot: &MenuSnapshot) -> String {
    let state = match snapshot.client.state {
        ClientMenuState::NotConfigured => "Not configured".to_string(),
        ClientMenuState::Stopped => "Stopped".to_string(),
        ClientMenuState::Starting => "Starting".to_string(),
        ClientMenuState::WaitingForHost => "Waiting for Host".to_string(),
        ClientMenuState::Authenticating => "Authenticating".to_string(),
        ClientMenuState::Connected(side) => {
            format!("Connected ({})", side_label(side).to_ascii_lowercase())
        }
        ClientMenuState::Disconnected => "Disconnected".to_string(),
        ClientMenuState::Stopping => "Stopping".to_string(),
        ClientMenuState::NeedsPermission => "Needs Permission".to_string(),
        ClientMenuState::WrongSecret => "Wrong Secret".to_string(),
        ClientMenuState::SideAlreadyInUse => "Side Already In Use".to_string(),
        ClientMenuState::CleanupUnconfirmed => "Cleanup Unconfirmed".to_string(),
        ClientMenuState::Failed => "Failed".to_string(),
    };
    format!("This Mac - Client: {state}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ipc::AttachedPeerStatus, model::RemotePointerMode};

    #[test]
    fn host_status_conversion_marks_unhealthy_pointer_capture_degraded() {
        let payload = StatusPayload {
            endpoint_id: "endpoint".to_string(),
            runtime_id: "runtime".to_string(),
            active: ActiveTarget::Right,
            pointer_mode: RemotePointerMode::Confine,
            attached: vec![Side::Right],
            attached_peers: vec![AttachedPeerStatus {
                side: Side::Right,
                endpoint_id: "peer".to_string(),
                name: "remote".to_string(),
            }],
            captured_events: 0,
            normalized_events: 0,
            replay_failures: 0,
            capture_tap_user_disabled: 0,
            recovery_events: 0,
            captured_queue_full_mouse_dropped: 0,
            captured_queue_full_non_mouse_dropped: 0,
            writer_queue_full_dropped: 0,
            writer_queue_full_forced_local: 0,
            pointer_lock_active: false,
            pointer_tap_healthy: false,
            capture_tap_stopped: 0,
        };
        let status = host_status_from_payload(payload);
        assert_eq!(status.state, HostMenuState::Degraded);
        assert_eq!(status.attached_peers.len(), 1);
    }

    #[test]
    fn client_status_conversion_preserves_connected_side() {
        let status = client_status_from_payload(ClientStatusPayload {
            host_id: "host".to_string(),
            side: Side::Up,
            instance_id: "client-runtime".to_string(),
            state: ClientLifecycleState::Connected,
            message: "connected".to_string(),
        });
        assert_eq!(status.state, ClientMenuState::Connected(Side::Up));
    }

    #[test]
    fn safe_messages_do_not_echo_secret_context() {
        assert_eq!(
            safe_message("invalid attach secret abc123"),
            "operation failed without exposing credentials"
        );
        assert_eq!(safe_message("host is unavailable"), "host is unavailable");
    }

    #[test]
    fn saving_a_client_profile_starts_client_connection() {
        assert!(matches!(profile_save_command(), MenuCommand::StartClient));
    }
}

pub fn run() -> Result<()> {
    ui::run()
}

#[cfg(not(target_os = "macos"))]
mod ui {
    use anyhow::{Result, bail};

    pub(crate) fn run() -> Result<()> {
        bail!("the macOS menu bar app is supported on macOS only")
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_op_in_unsafe_fn)]
mod ui {
    use std::{ffi::CStr, thread, time::Duration};

    use anyhow::{Context, Result, anyhow};
    use cocoa::{
        appkit::{NSApp, NSApplication, NSApplicationActivationPolicyAccessory},
        base::{NO, YES, id, nil},
        foundation::{
            NSAutoreleasePool, NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString,
        },
    };
    use objc::{class, msg_send, sel, sel_impl};
    use tokio::runtime::Runtime;
    use tray_icon::{
        TrayIconBuilder,
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    };

    use super::{
        ClientMenuState, MenuCommand, MenuSnapshot, MenuSupervisor, client_status_label,
        host_status_label,
    };
    use crate::{
        clipboard, macos_permissions,
        model::Side,
        state::{
            ClientProfile, load_client_profile, parse_invitation, validate_client_profile,
            write_client_profile,
        },
    };

    const HOST_STATUS_ID: &str = "host_status";
    const HOST_ENDPOINT_ID: &str = "host_endpoint";
    const HOST_TARGET_ID: &str = "host_target";
    const HOST_ATTACHED_ID: &str = "host_attached";
    const CLIENT_STATUS_ID: &str = "client_status";
    const ERROR_ID: &str = "error";
    const START_HOST_ID: &str = "start_host";
    const STOP_HOST_ID: &str = "stop_host";
    const START_CLIENT_ID: &str = "start_client";
    const RECONNECT_CLIENT_ID: &str = "reconnect_client";
    const STOP_CLIENT_ID: &str = "stop_client";
    const EDIT_PROFILE_ID: &str = "edit_profile";
    const IMPORT_INVITATION_ID: &str = "import_invitation";
    const OPEN_SETTINGS_ID: &str = "open_settings";
    const CHECK_PERMISSIONS_ID: &str = "check_permissions";
    const REFRESH_ID: &str = "refresh";
    const QUIT_ID: &str = "quit";

    struct MenuItems {
        host_status: MenuItem,
        host_endpoint: MenuItem,
        host_target: MenuItem,
        host_attached: MenuItem,
        client_status: MenuItem,
        error: MenuItem,
        start_host: MenuItem,
        stop_host: MenuItem,
        start_client: MenuItem,
        reconnect_client: MenuItem,
        stop_client: MenuItem,
        copy_invitation: Submenu,
        copy_sides: [MenuItem; 4],
    }

    pub(crate) fn run() -> Result<()> {
        let _app_lock = crate::state::MenuAppLock::acquire()?;
        unsafe {
            let _pool = NSAutoreleasePool::new(nil);
            let app = NSApp();
            app.setActivationPolicy_(NSApplicationActivationPolicyAccessory);
            let _: () = msg_send![app, finishLaunching];

            let (command_tx, command_rx) = tokio::sync::mpsc::channel(32);
            let (snapshot_tx, mut snapshot_rx) = tokio::sync::mpsc::channel(8);
            let supervisor_thread = thread::spawn(move || {
                let Ok(runtime) = Runtime::new() else {
                    return;
                };
                let Ok(supervisor) = MenuSupervisor::new() else {
                    return;
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(&runtime, supervisor.run(command_rx, snapshot_tx));
            });

            run_onboarding(&command_tx)?;
            let (menu, items) = build_menu()?;
            let _tray = TrayIconBuilder::new()
                .with_menu(Box::new(menu.clone()))
                .with_title("Meow")
                .with_tooltip("Meow keyboard and mouse sharing")
                .build()
                .context("failed to create macOS menu bar item")?;

            let menu_events = MenuEvent::receiver();
            let mut should_quit = false;
            while !should_quit {
                pump_app_events(app);
                while let Ok(event) = menu_events.try_recv() {
                    if let Err(err) =
                        handle_menu_event(event.id.as_ref(), &command_tx, &mut should_quit)
                    {
                        simple_alert("Meow", &super::safe_error(&err), &["OK"]);
                    }
                }
                while let Ok(snapshot) = snapshot_rx.try_recv() {
                    render_snapshot(&items, &snapshot);
                }
                thread::sleep(Duration::from_millis(25));
            }
            let _ = command_tx.blocking_send(MenuCommand::Shutdown);
            let _ = supervisor_thread.join();
            Ok(())
        }
    }

    fn build_menu() -> Result<(Menu, MenuItems)> {
        let host_status =
            MenuItem::with_id(HOST_STATUS_ID, "This Mac - Host: Starting", false, None);
        let host_endpoint =
            MenuItem::with_id(HOST_ENDPOINT_ID, "Endpoint: unavailable", false, None);
        let host_target =
            MenuItem::with_id(HOST_TARGET_ID, "Active target: unavailable", false, None);
        let host_attached =
            MenuItem::with_id(HOST_ATTACHED_ID, "Attached clients: none", false, None);
        let client_status = MenuItem::with_id(
            CLIENT_STATUS_ID,
            "This Mac - Client: Not configured",
            false,
            None,
        );
        let error = MenuItem::with_id(ERROR_ID, "", false, None);
        let start_host = MenuItem::with_id(START_HOST_ID, "Start Host", true, None);
        let stop_host = MenuItem::with_id(STOP_HOST_ID, "Stop Host", false, None);
        let start_client = MenuItem::with_id(START_CLIENT_ID, "Start Client", false, None);
        let reconnect_client =
            MenuItem::with_id(RECONNECT_CLIENT_ID, "Reconnect Client", false, None);
        let stop_client = MenuItem::with_id(STOP_CLIENT_ID, "Stop Client", false, None);
        let edit_profile = MenuItem::with_id(EDIT_PROFILE_ID, "Edit Client Profile", true, None);
        let import_invitation =
            MenuItem::with_id(IMPORT_INVITATION_ID, "Paste Invitation", true, None);
        let copy_sides = [
            MenuItem::with_id("copy_left", "Left", false, None),
            MenuItem::with_id("copy_right", "Right", false, None),
            MenuItem::with_id("copy_up", "Up", false, None),
            MenuItem::with_id("copy_down", "Down", false, None),
        ];
        let copy_invitation = Submenu::with_id("copy_invitation", "Copy Invitation", false);
        for side in &copy_sides {
            copy_invitation.append(side)?;
        }

        let menu = Menu::new();
        menu.append(&host_status)?;
        menu.append(&host_endpoint)?;
        menu.append(&host_target)?;
        menu.append(&host_attached)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&start_host)?;
        menu.append(&stop_host)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&client_status)?;
        menu.append(&start_client)?;
        menu.append(&reconnect_client)?;
        menu.append(&stop_client)?;
        menu.append(&edit_profile)?;
        menu.append(&import_invitation)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&copy_invitation)?;
        menu.append(&PredefinedMenuItem::separator())?;
        let open_settings = MenuItem::with_id(OPEN_SETTINGS_ID, "Open System Settings", true, None);
        let check_permissions =
            MenuItem::with_id(CHECK_PERMISSIONS_ID, "Check Permissions", true, None);
        let refresh = MenuItem::with_id(REFRESH_ID, "Refresh Status", true, None);
        let quit = MenuItem::with_id(QUIT_ID, "Quit Menu Bar App", true, None);
        menu.append(&open_settings)?;
        menu.append(&check_permissions)?;
        menu.append(&refresh)?;
        menu.append(&error)?;
        menu.append(&quit)?;
        Ok((
            menu,
            MenuItems {
                host_status,
                host_endpoint,
                host_target,
                host_attached,
                client_status,
                error,
                start_host,
                stop_host,
                start_client,
                reconnect_client,
                stop_client,
                copy_invitation,
                copy_sides,
            },
        ))
    }

    fn render_snapshot(items: &MenuItems, snapshot: &MenuSnapshot) {
        items.host_status.set_text(host_status_label(snapshot));
        items.host_endpoint.set_text(format!(
            "Endpoint: {}",
            snapshot
                .host
                .endpoint_id
                .as_deref()
                .map(short_id)
                .unwrap_or("unavailable")
        ));
        items.host_target.set_text(format!(
            "Active target: {}",
            snapshot
                .host
                .active
                .map(|target| target.to_string())
                .unwrap_or_else(|| "unavailable".to_string())
        ));
        let attached = if snapshot.host.attached_peers.is_empty() {
            "none".to_string()
        } else {
            snapshot
                .host
                .attached_peers
                .iter()
                .map(|peer| {
                    format!(
                        "{} ({})",
                        side_label(peer.side),
                        short_id(&peer.endpoint_id)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        items
            .host_attached
            .set_text(format!("Attached clients: {attached}"));
        items.client_status.set_text(client_status_label(snapshot));
        items.error.set_text(
            snapshot
                .last_error
                .as_deref()
                .map(|error| format!("Error: {error}"))
                .unwrap_or_default(),
        );

        let host_busy = matches!(snapshot.host.state, super::HostMenuState::Starting);
        let host_running = matches!(
            snapshot.host.state,
            super::HostMenuState::Running | super::HostMenuState::Degraded
        );
        items.start_host.set_enabled(!host_busy && !host_running);
        items.stop_host.set_enabled(host_running);
        let client_configured = !matches!(snapshot.client.state, ClientMenuState::NotConfigured);
        let client_running = matches!(
            snapshot.client.state,
            ClientMenuState::Starting
                | ClientMenuState::WaitingForHost
                | ClientMenuState::Authenticating
                | ClientMenuState::Connected(_)
                | ClientMenuState::Stopping
        );
        items
            .start_client
            .set_enabled(client_configured && !client_running);
        items.stop_client.set_enabled(client_running);
        items.reconnect_client.set_enabled(matches!(
            snapshot.client.state,
            ClientMenuState::Disconnected | ClientMenuState::Failed
        ));
        items
            .copy_invitation
            .set_enabled(snapshot.credentials_available);
        for side in &items.copy_sides {
            side.set_enabled(snapshot.credentials_available);
        }
        if let Some(notice) = snapshot.notice.as_deref() {
            items.error.set_text(notice);
        }
    }

    fn handle_menu_event(
        id: &str,
        command_tx: &tokio::sync::mpsc::Sender<MenuCommand>,
        should_quit: &mut bool,
    ) -> Result<()> {
        let command = match id {
            START_HOST_ID => Some(MenuCommand::StartHost),
            STOP_HOST_ID => Some(MenuCommand::StopHost),
            START_CLIENT_ID => Some(MenuCommand::StartClient),
            RECONNECT_CLIENT_ID => Some(MenuCommand::ReconnectClient),
            STOP_CLIENT_ID => Some(MenuCommand::StopClient),
            REFRESH_ID => Some(MenuCommand::Refresh),
            OPEN_SETTINGS_ID => {
                macos_permissions::open_host_system_settings()?;
                None
            }
            CHECK_PERMISSIONS_ID => {
                show_permissions();
                Some(MenuCommand::Refresh)
            }
            EDIT_PROFILE_ID => {
                if let Some(profile) = edit_profile(load_client_profile()?)? {
                    write_client_profile(&profile)?;
                    return command_tx
                        .try_send(super::profile_save_command())
                        .map_err(|_| anyhow!("menu supervisor is unavailable"));
                }
                Some(MenuCommand::Refresh)
            }
            IMPORT_INVITATION_ID => {
                let imported = parse_invitation(&clipboard::read_text()?)?;
                if let Some(profile) = edit_profile(Some(imported))? {
                    write_client_profile(&profile)?;
                    return command_tx
                        .try_send(super::profile_save_command())
                        .map_err(|_| anyhow!("menu supervisor is unavailable"));
                }
                Some(MenuCommand::Refresh)
            }
            "copy_left" => Some(MenuCommand::CopyInvitation { side: Side::Left }),
            "copy_right" => Some(MenuCommand::CopyInvitation { side: Side::Right }),
            "copy_up" => Some(MenuCommand::CopyInvitation { side: Side::Up }),
            "copy_down" => Some(MenuCommand::CopyInvitation { side: Side::Down }),
            QUIT_ID => {
                *should_quit = true;
                None
            }
            _ => None,
        };
        if let Some(command) = command {
            command_tx
                .try_send(command)
                .map_err(|_| anyhow!("menu supervisor is unavailable"))?;
        }
        Ok(())
    }

    fn run_onboarding(command_tx: &tokio::sync::mpsc::Sender<MenuCommand>) -> Result<()> {
        if load_client_profile()?.is_some()
            || crate::state::load_existing_host_credentials()?.is_some()
        {
            return Ok(());
        }
        let response = simple_alert(
            "Set Up Meow",
            "Choose which role this Mac should manage. Host captures local input; client injects input received from another Mac.",
            &["Set Up Host", "Set Up Client", "Set Up Both", "Cancel"],
        );
        match response {
            1000 => command_tx
                .blocking_send(MenuCommand::StartHost)
                .map_err(|_| anyhow!("menu supervisor is unavailable"))?,
            1001 => {
                if let Some(profile) = edit_profile(None)? {
                    write_client_profile(&profile)?;
                    command_tx
                        .blocking_send(MenuCommand::StartClient)
                        .map_err(|_| anyhow!("menu supervisor is unavailable"))?;
                }
            }
            1002 => {
                if let Some(profile) = edit_profile(None)? {
                    write_client_profile(&profile)?;
                    command_tx
                        .blocking_send(MenuCommand::StartHost)
                        .map_err(|_| anyhow!("menu supervisor is unavailable"))?;
                    command_tx
                        .blocking_send(MenuCommand::StartClient)
                        .map_err(|_| anyhow!("menu supervisor is unavailable"))?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn edit_profile(existing: Option<ClientProfile>) -> Result<Option<ClientProfile>> {
        unsafe { edit_profile_inner(existing) }
    }

    unsafe fn edit_profile_inner(existing: Option<ClientProfile>) -> Result<Option<ClientProfile>> {
        let (host_field, secret_field, side_popup, view) = profile_accessory(existing.as_ref())?;
        let alert: id = msg_send![class!(NSAlert), alloc];
        let alert: id = msg_send![alert, init];
        let title = NSString::alloc(nil).init_str("Client Profile");
        let info = NSString::alloc(nil).init_str(
            "Enter a host ID and attach secret. Saving starts the client connection. Paste Invitation imports the canonical attach command.",
        );
        let _: () = msg_send![alert, setMessageText:title];
        let _: () = msg_send![alert, setInformativeText:info];
        let _: () = msg_send![alert, setAccessoryView:view];
        let _: id =
            msg_send![alert, addButtonWithTitle:NSString::alloc(nil).init_str("Save & Connect")];
        let _: id = msg_send![alert, addButtonWithTitle:NSString::alloc(nil).init_str("Cancel")];
        let response: i64 = msg_send![alert, runModal];
        if response != 1000 {
            return Ok(None);
        }

        let host_id = object_string(host_field);
        let secret = object_string(secret_field);
        let index: i64 = msg_send![side_popup, indexOfSelectedItem];
        let side = match index {
            0 => Side::Left,
            1 => Side::Right,
            2 => Side::Up,
            3 => Side::Down,
            _ => return Err(anyhow!("select a client side")),
        };
        let profile = ClientProfile {
            schema_version: 1,
            host_id,
            secret,
            side,
        };
        match validate_client_profile(&profile) {
            Ok(_) => Ok(Some(profile)),
            Err(err) => {
                simple_alert("Invalid Client Profile", &super::safe_error(&err), &["OK"]);
                Ok(None)
            }
        }
    }

    unsafe fn profile_accessory(existing: Option<&ClientProfile>) -> Result<(id, id, id, id)> {
        let view: id = msg_send![class!(NSView), alloc];
        let view: id = msg_send![view, initWithFrame:NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(430.0, 115.0)
        )];
        let host = text_field(
            NSRect::new(NSPoint::new(0.0, 75.0), NSSize::new(430.0, 28.0)),
            "Host endpoint ID",
        );
        let secret = secure_text_field(
            NSRect::new(NSPoint::new(0.0, 40.0), NSSize::new(430.0, 28.0)),
            "Attach secret",
        );
        let popup: id = msg_send![class!(NSPopUpButton), alloc];
        let popup: id = msg_send![popup, initWithFrame:NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(430.0, 28.0)
        ) pullsDown:NO];
        for title in ["Left", "Right", "Up", "Down"] {
            let _: () = msg_send![popup, addItemWithTitle:NSString::alloc(nil).init_str(title)];
        }
        if let Some(profile) = existing {
            let _: () =
                msg_send![host, setStringValue:NSString::alloc(nil).init_str(&profile.host_id)];
            let _: () =
                msg_send![secret, setStringValue:NSString::alloc(nil).init_str(&profile.secret)];
            let index = match profile.side {
                Side::Left => 0,
                Side::Right => 1,
                Side::Up => 2,
                Side::Down => 3,
            };
            let _: () = msg_send![popup, selectItemAtIndex:index];
        }
        let _: () = msg_send![view, addSubview:host];
        let _: () = msg_send![view, addSubview:secret];
        let _: () = msg_send![view, addSubview:popup];
        Ok((host, secret, popup, view))
    }

    unsafe fn text_field(frame: NSRect, placeholder: &str) -> id {
        let field: id = msg_send![class!(NSTextField), alloc];
        let field: id = msg_send![field, initWithFrame:frame];
        let _: () =
            msg_send![field, setPlaceholderString:NSString::alloc(nil).init_str(placeholder)];
        field
    }

    unsafe fn secure_text_field(frame: NSRect, placeholder: &str) -> id {
        let field: id = msg_send![class!(NSSecureTextField), alloc];
        let field: id = msg_send![field, initWithFrame:frame];
        let _: () =
            msg_send![field, setPlaceholderString:NSString::alloc(nil).init_str(placeholder)];
        field
    }

    unsafe fn object_string(object: id) -> String {
        if object == nil {
            return String::new();
        }
        let value: id = msg_send![object, stringValue];
        if value == nil {
            return String::new();
        }
        let bytes: *const std::ffi::c_char = msg_send![value, UTF8String];
        if bytes.is_null() {
            String::new()
        } else {
            CStr::from_ptr(bytes).to_string_lossy().into_owned()
        }
    }

    fn show_permissions() {
        let current = std::env::current_exe();
        let host = super::macos_permissions::check_host_permissions();
        let client = super::macos_permissions::check_client_permissions();
        let target = current
            .as_ref()
            .map(|path| super::macos_permissions::permission_target_for_executable(path))
            .unwrap_or_default();
        let message = format!(
            "Permission target: {}\nHost: Accessibility={} Input Monitoring={}\nClient: Accessibility={}",
            target.display(),
            yes_no(host.accessibility),
            yes_no(host.input_monitoring),
            yes_no(client.accessibility),
        );
        simple_alert("Meow Permissions", &message, &["OK"]);
    }

    fn yes_no(value: bool) -> &'static str {
        if value { "granted" } else { "missing" }
    }

    fn simple_alert(title: &str, information: &str, buttons: &[&str]) -> i64 {
        unsafe {
            let alert: id = msg_send![class!(NSAlert), alloc];
            let alert: id = msg_send![alert, init];
            let title = NSString::alloc(nil).init_str(title);
            let information = NSString::alloc(nil).init_str(information);
            let _: () = msg_send![alert, setMessageText:title];
            let _: () = msg_send![alert, setInformativeText:information];
            for button in buttons {
                let _: id =
                    msg_send![alert, addButtonWithTitle:NSString::alloc(nil).init_str(button)];
            }
            msg_send![alert, runModal]
        }
    }

    unsafe fn pump_app_events(app: id) {
        let date = NSDate::distantPast(nil);
        let event: id = msg_send![
            app,
            nextEventMatchingMask:u64::MAX
            untilDate:date
            inMode:NSDefaultRunLoopMode
            dequeue:YES
        ];
        if event != nil {
            let _: () = msg_send![app, sendEvent:event];
        }
        let _: () = msg_send![app, updateWindows];
    }

    fn side_label(side: Side) -> &'static str {
        match side {
            Side::Left => "left",
            Side::Right => "right",
            Side::Up => "up",
            Side::Down => "down",
        }
    }

    fn short_id(value: &str) -> &str {
        value.get(..8).unwrap_or(value)
    }
}
