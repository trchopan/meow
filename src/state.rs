use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, anyhow, bail};
use iroh::{EndpointId, SecretKey};
use rand::{Rng, distributions::Alphanumeric, thread_rng};
use serde::{Deserialize, Serialize};

use crate::input::{
    default_copy_file_key, default_detach_key, default_down_key, default_left_key,
    default_paste_key, default_right_key, default_up_key, parse_copy_file_chord,
    parse_detach_chord, parse_directional_chord, parse_paste_chord,
};
use crate::model::{RemotePointerMode, Side};
use crate::presentation::{print_identity_reset_complete, print_rotate_secret_complete};

fn default_remote_pointer_mode() -> RemotePointerMode {
    RemotePointerMode::EdgeToEdge
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PersistedHostState {
    pub(crate) schema_version: u8,
    pub(crate) endpoint_id: String,
    pub(crate) attach_secret: String,
    pub(crate) detach_key: String,
    #[serde(default = "default_remote_pointer_mode")]
    pub(crate) remote_pointer_mode: RemotePointerMode,
    #[serde(alias = "clipboard_key")]
    pub(crate) paste_key: String,
    #[serde(default = "default_copy_file_key")]
    pub(crate) copy_file_key: String,
    pub(crate) up_key: String,
    pub(crate) down_key: String,
    pub(crate) left_key: String,
    pub(crate) right_key: String,
}

pub(crate) struct HostCredentials {
    pub(crate) endpoint_id: EndpointId,
    pub(crate) attach_secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ClientProfile {
    pub(crate) schema_version: u8,
    pub(crate) host_id: String,
    pub(crate) secret: String,
    pub(crate) side: Side,
}

pub(crate) fn app_data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("MEOW_STATE_DIR")
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path));
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))?;
    Ok(home.join(".local").join("share").join("meow"))
}

pub(crate) fn socket_path() -> Result<PathBuf> {
    let dir = app_data_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("meow.sock"))
}

pub(crate) fn client_identity_path() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("client.id"))
}

pub(crate) fn client_profile_path() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("client_profile.json"))
}

pub(crate) fn client_socket_path(host_id: &str, side: Side) -> Result<PathBuf> {
    let endpoint_id = EndpointId::from_str(host_id).context("invalid host endpoint id")?;
    let state_dir = app_data_dir()?;
    let candidate = state_dir.join(format!(
        "c{}-{}.sock",
        client_path_key(&endpoint_id),
        side_name(side)
    ));
    if candidate.to_string_lossy().len() < 90 {
        return Ok(candidate);
    }

    let fallback_key = blake3::hash(
        format!(
            "{}:{}:{}",
            state_dir.display(),
            endpoint_id,
            side_name(side)
        )
        .as_bytes(),
    )
    .to_hex();
    Ok(std::env::temp_dir().join(format!("meow-c-{}.sock", &fallback_key[..16])))
}

pub(crate) fn client_status_path(host_id: &str, side: Side) -> Result<PathBuf> {
    let endpoint_id = EndpointId::from_str(host_id).context("invalid host endpoint id")?;
    Ok(app_data_dir()?.join(format!(
        "c{}-{}.json",
        client_path_key(&endpoint_id),
        side_name(side)
    )))
}

fn client_path_key(endpoint_id: &EndpointId) -> String {
    let canonical = endpoint_id.to_string();
    let digest = blake3::hash(canonical.as_bytes()).to_hex();
    digest[..32].to_string()
}

pub(crate) fn load_or_create_client_identity() -> Result<String> {
    let path = client_identity_path()?;
    let app_dir = app_data_dir()?;
    fs::create_dir_all(&app_dir)
        .with_context(|| format!("failed to create {}", app_dir.display()))?;
    load_or_create_client_identity_at(&path, &app_dir.join("client.id.lock"))
}

fn load_or_create_client_identity_at(path: &Path, lock_path: &Path) -> Result<String> {
    let _identity_lock = AdvisoryFileLock::acquire(lock_path)?;
    if path.exists() {
        let identity = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let identity = identity.trim();
        if !identity.is_empty() {
            return Ok(identity.to_string());
        }
    }

    let identity = uuid::Uuid::new_v4().simple().to_string();
    write_file_atomic(path, identity.as_bytes())?;
    Ok(identity)
}

struct AdvisoryFileLock {
    file: fs::File,
}

impl AdvisoryFileLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == ErrorKind::WouldBlock {
                    bail!("advisory lock is already held: {}", path.display());
                }
                return Err(err).with_context(|| format!("failed to lock {}", path.display()));
            }
        }

        #[cfg(not(unix))]
        {
            let _ = path;
            bail!("advisory locking is unsupported on this platform");
        }

        Ok(Self { file })
    }
}

pub(crate) struct HostRuntimeLock {
    _lock: AdvisoryFileLock,
}

impl HostRuntimeLock {
    pub(crate) fn acquire() -> Result<Self> {
        let path = if synthetic_runtime_enabled() {
            app_data_dir()?.join("host-runtime.lock")
        } else {
            global_runtime_lock_path()?
        };
        ensure_lock_parent(&path)?;
        Ok(Self {
            _lock: AdvisoryFileLock::acquire(&path)
                .context("another Meow host runtime is already active")?,
        })
    }
}

fn synthetic_runtime_enabled() -> bool {
    ["MEOW_DEV_SMOKE", "MEOW_BENCH_FLUSH"]
        .into_iter()
        .any(|name| {
            std::env::var(name)
                .map(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
                .unwrap_or(false)
        })
}

pub(crate) struct MenuAppLock {
    _lock: AdvisoryFileLock,
}

impl MenuAppLock {
    pub(crate) fn acquire() -> Result<Self> {
        let path = global_menu_lock_path()?;
        ensure_lock_parent(&path)?;
        Ok(Self {
            _lock: AdvisoryFileLock::acquire(&path)
                .context("another Meow menu bar app is already running")?,
        })
    }
}

fn global_runtime_lock_path() -> Result<PathBuf> {
    Ok(default_app_data_dir()?.join("host-runtime.lock"))
}

fn global_menu_lock_path() -> Result<PathBuf> {
    Ok(default_app_data_dir()?.join("menu-app.lock"))
}

fn default_app_data_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))?;
    Ok(home.join(".local").join("share").join("meow"))
}

fn ensure_lock_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("missing lock parent for {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to protect {}", parent.display()))?;
    }
    Ok(())
}

impl Drop for AdvisoryFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

pub(crate) struct ClientAttachLock {
    _lock: AdvisoryFileLock,
}

impl ClientAttachLock {
    pub(crate) fn acquire(host_id: &str, side: Side) -> Result<Self> {
        let app_dir = app_data_dir()?;
        fs::create_dir_all(&app_dir)
            .with_context(|| format!("failed to create {}", app_dir.display()))?;
        let key = blake3::hash(format!("{host_id}:{}", side_name(side)).as_bytes()).to_hex();
        let path = app_dir.join(format!("attach-{}-{}.lock", &key[..32], side_name(side)));
        Ok(Self {
            _lock: AdvisoryFileLock::acquire(&path)
                .with_context(|| format!("already attached to this host as {side:?}"))?,
        })
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Left => "left",
        Side::Right => "right",
        Side::Up => "up",
        Side::Down => "down",
    }
}

pub(crate) fn host_key_path() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("host.key"))
}

pub(crate) fn host_state_path() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("host_state.json"))
}

pub(crate) fn load_existing_host_credentials() -> Result<Option<HostCredentials>> {
    load_existing_host_credentials_from_paths(&host_key_path()?, &host_state_path()?)
}

fn load_existing_host_credentials_from_paths(
    key_path: &Path,
    state_path: &Path,
) -> Result<Option<HostCredentials>> {
    let key_exists = key_path.exists();
    let state_exists = state_path.exists();

    if !key_exists && !state_exists {
        return Ok(None);
    }
    if !key_exists || !state_exists {
        bail!("host credentials are incomplete; host.key and host_state.json must exist together");
    }

    let secret_key = read_host_secret_key(key_path)?;
    let endpoint_id = EndpointId::from(secret_key.public());
    let bytes =
        fs::read(state_path).with_context(|| format!("failed to read {}", state_path.display()))?;
    let state: PersistedHostState = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", state_path.display()))?;
    if state.endpoint_id != endpoint_id.to_string() {
        bail!(
            "host identity mismatch between {} and {}",
            key_path.display(),
            state_path.display()
        );
    }
    if state.attach_secret.trim().is_empty() {
        bail!("host attach secret is empty in {}", state_path.display());
    }

    Ok(Some(HostCredentials {
        endpoint_id,
        attach_secret: state.attach_secret,
    }))
}

pub(crate) fn load_client_profile() -> Result<Option<ClientProfile>> {
    let path = client_profile_path()?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(load_client_profile_at(&path)?))
}

pub(crate) fn load_client_profile_at(path: &Path) -> Result<ClientProfile> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let profile: ClientProfile = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    validate_client_profile(&profile)?;
    Ok(profile)
}

pub(crate) fn write_client_profile(profile: &ClientProfile) -> Result<()> {
    validate_client_profile(profile)?;
    let path = client_profile_path()?;
    let bytes = serde_json::to_vec_pretty(profile)?;
    write_file_atomic(&path, &bytes)
}

pub(crate) fn validate_client_profile(profile: &ClientProfile) -> Result<EndpointId> {
    if profile.schema_version != 1 {
        bail!(
            "unsupported client profile schema version {}",
            profile.schema_version
        );
    }
    if profile.host_id.trim().is_empty() {
        bail!("client profile host ID is empty");
    }
    let endpoint_id =
        EndpointId::from_str(&profile.host_id).context("invalid client profile host ID")?;
    if profile.secret.trim().is_empty() {
        bail!("client profile secret is empty");
    }
    Ok(endpoint_id)
}

pub(crate) fn parse_invitation(input: &str) -> Result<ClientProfile> {
    let parts = input.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 6 || parts[0] != "meow" || parts[1] != "attach" || parts[4] != "--side" {
        bail!("expected: meow attach <host-id> <secret> --side <left|right|up|down>");
    }

    let side = match parts[5] {
        "left" => Side::Left,
        "right" => Side::Right,
        "up" => Side::Up,
        "down" => Side::Down,
        _ => bail!("invalid invitation side"),
    };
    let profile = ClientProfile {
        schema_version: 1,
        host_id: parts[2].to_string(),
        secret: parts[3].to_string(),
        side,
    };
    validate_client_profile(&profile)?;
    Ok(profile)
}

pub(crate) fn format_attach_command(
    endpoint_id: EndpointId,
    attach_secret: &str,
    side: Side,
) -> String {
    format!(
        "meow attach {endpoint_id} {attach_secret} --side {}",
        side_name(side)
    )
}

pub(crate) async fn reset_identity() -> Result<()> {
    let _runtime_lock = HostRuntimeLock::acquire()?;
    if crate::ipc::is_daemon_running().await {
        bail!("host daemon is running, stop it first with `meow stop`");
    }

    let key_path = host_key_path()?;
    let state_path = host_state_path()?;

    if key_path.exists() {
        fs::remove_file(&key_path)
            .with_context(|| format!("failed to remove {}", key_path.display()))?;
    }
    if state_path.exists() {
        fs::remove_file(&state_path)
            .with_context(|| format!("failed to remove {}", state_path.display()))?;
    }

    print_identity_reset_complete();
    Ok(())
}

pub(crate) async fn rotate_secret() -> Result<()> {
    let _runtime_lock = HostRuntimeLock::acquire()?;
    if crate::ipc::is_daemon_running().await {
        bail!("host daemon is running, stop it first with `meow stop`");
    }

    let endpoint_id = EndpointId::from(load_or_create_host_secret_key()?.public());
    let state_path = host_state_path()?;
    let mut state = load_or_create_host_state(endpoint_id)?;

    state.endpoint_id = endpoint_id.to_string();
    state.attach_secret = random_secret();
    write_host_state_file(&state_path, &state)?;

    print_rotate_secret_complete(&state.endpoint_id, &state.attach_secret);
    Ok(())
}

pub(crate) fn load_or_create_host_secret_key() -> Result<SecretKey> {
    let key_path = host_key_path()?;
    let app_dir = app_data_dir()?;
    fs::create_dir_all(&app_dir)
        .with_context(|| format!("failed to create {}", app_dir.display()))?;

    if key_path.exists() {
        return read_host_secret_key(&key_path);
    }

    let secret = SecretKey::generate();
    write_secret_key_file(&key_path, &secret.to_bytes())?;
    Ok(secret)
}

fn read_host_secret_key(path: &Path) -> Result<SecretKey> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    if bytes.len() != 32 {
        bail!("invalid host key length in {}", path.display());
    }
    let mut key_bytes = [0u8; 32];
    key_bytes.copy_from_slice(&bytes);
    Ok(SecretKey::from_bytes(&key_bytes))
}

fn write_secret_key_file(path: &Path, key: &[u8; 32]) -> Result<()> {
    write_file_atomic(path, key)
}

pub(crate) fn load_or_create_host_state(endpoint_id: EndpointId) -> Result<PersistedHostState> {
    let state_path = host_state_path()?;
    if state_path.exists() {
        let bytes = fs::read(&state_path)
            .with_context(|| format!("failed to read {}", state_path.display()))?;
        let state: PersistedHostState = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", state_path.display()))?;
        if state.endpoint_id != endpoint_id.to_string() {
            bail!(
                "host identity mismatch between {} and {}",
                host_key_path()?.display(),
                state_path.display()
            );
        }
        let (state, repaired) = repair_host_state(state);
        parse_detach_chord(&state.detach_key).with_context(|| {
            format!(
                "invalid detach_key {:?} in {}",
                state.detach_key,
                state_path.display()
            )
        })?;
        for (name, value) in [
            ("up_key", &state.up_key),
            ("down_key", &state.down_key),
            ("left_key", &state.left_key),
            ("right_key", &state.right_key),
        ] {
            parse_directional_chord(value, name, value).with_context(|| {
                format!("invalid {name} {:?} in {}", value, state_path.display())
            })?;
        }
        validate_shortcut_conflicts(&state)?;
        let changed = repaired;
        parse_paste_chord(&state.paste_key).with_context(|| {
            format!(
                "invalid paste_key {:?} in {}",
                state.paste_key,
                state_path.display()
            )
        })?;
        if changed {
            write_host_state_file(&state_path, &state)?;
        }
        return Ok(state);
    }

    let state = PersistedHostState {
        schema_version: 1,
        endpoint_id: endpoint_id.to_string(),
        attach_secret: random_secret(),
        detach_key: default_detach_key(),
        remote_pointer_mode: default_remote_pointer_mode(),
        paste_key: default_paste_key(),
        copy_file_key: default_copy_file_key(),
        up_key: default_up_key(),
        down_key: default_down_key(),
        left_key: default_left_key(),
        right_key: default_right_key(),
    };
    write_host_state_file(&state_path, &state)?;
    Ok(state)
}

fn validate_shortcut_conflicts(state: &PersistedHostState) -> Result<()> {
    let shortcuts = [
        ("detach_key", parse_detach_chord(&state.detach_key)?),
        ("paste_key", parse_paste_chord(&state.paste_key)?),
        (
            "copy_file_key",
            parse_copy_file_chord(&state.copy_file_key)?,
        ),
        (
            "up_key",
            parse_directional_chord(&state.up_key, "up_key", &state.up_key)?,
        ),
        (
            "down_key",
            parse_directional_chord(&state.down_key, "down_key", &state.down_key)?,
        ),
        (
            "left_key",
            parse_directional_chord(&state.left_key, "left_key", &state.left_key)?,
        ),
        (
            "right_key",
            parse_directional_chord(&state.right_key, "right_key", &state.right_key)?,
        ),
    ];
    for (index, (name, chord)) in shortcuts.iter().enumerate() {
        for (other_name, other_chord) in shortcuts.iter().skip(index + 1) {
            if chord.key == other_chord.key
                && (modifiers_are_subset(chord, other_chord)
                    || modifiers_are_subset(other_chord, chord))
            {
                bail!("shortcut conflict: {name} and {other_name}");
            }
        }
    }
    Ok(())
}

fn modifiers_are_subset(a: &crate::input::DetachChord, b: &crate::input::DetachChord) -> bool {
    (!a.ctrl || b.ctrl) && (!a.alt || b.alt) && (!a.meta || b.meta) && (!a.shift || b.shift)
}

pub(crate) fn write_host_state_file(path: &Path, state: &PersistedHostState) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state)?;
    write_file_atomic(path, &bytes)
}

pub(crate) fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("missing parent directory for {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to protect {}", parent.display()))?;
    }

    let temp_path = unique_temp_path(path);

    let write_result: Result<()> = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temp_path)
                .with_context(|| format!("failed to create {}", temp_path.display()))?;
            file.write_all(bytes)
                .with_context(|| format!("failed to write {}", temp_path.display()))?;
            file.sync_all()
                .with_context(|| format!("failed to sync {}", temp_path.display()))?;
        }

        #[cfg(not(unix))]
        {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp_path)
                .with_context(|| format!("failed to create {}", temp_path.display()))?;
            file.write_all(bytes)
                .with_context(|| format!("failed to write {}", temp_path.display()))?;
            file.sync_all()
                .with_context(|| format!("failed to sync {}", temp_path.display()))?;
        }

        fs::rename(&temp_path, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;

        #[cfg(unix)]
        {
            let dir = fs::File::open(parent)
                .with_context(|| format!("failed to open {}", parent.display()))?;
            dir.sync_all()
                .with_context(|| format!("failed to sync {}", parent.display()))?;
        }
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }

    write_result
}

fn unique_temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "meow-state".to_string());
    let random_id: u64 = thread_rng().r#gen();
    let temp_name = format!(".{file_name}.tmp-{}-{random_id}", std::process::id());
    path.with_file_name(temp_name)
}

pub(crate) fn random_secret() -> String {
    thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

fn repair_host_state(mut state: PersistedHostState) -> (PersistedHostState, bool) {
    let mut changed = false;

    if state.attach_secret.trim().is_empty() {
        state.attach_secret = random_secret();
        changed = true;
    }

    if state.detach_key.trim().is_empty() {
        state.detach_key = default_detach_key();
        changed = true;
    }

    if state.paste_key.trim().is_empty() {
        state.paste_key = default_paste_key();
        changed = true;
    }
    if state.copy_file_key.trim().is_empty() {
        state.copy_file_key = default_copy_file_key();
        changed = true;
    }

    for (value, default) in [
        (&mut state.up_key, default_up_key()),
        (&mut state.down_key, default_down_key()),
        (&mut state.left_key, default_left_key()),
        (&mut state.right_key, default_right_key()),
    ] {
        if value.trim().is_empty() {
            *value = default;
            changed = true;
        }
    }

    (state, changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_endpoint_id() -> EndpointId {
        EndpointId::from(SecretKey::generate().public())
    }

    #[test]
    fn repair_fills_missing_secret_and_detach_key() {
        let endpoint_id = sample_endpoint_id();
        let state = PersistedHostState {
            schema_version: 1,
            endpoint_id: endpoint_id.to_string(),
            attach_secret: "   ".to_string(),
            detach_key: " ".to_string(),
            remote_pointer_mode: RemotePointerMode::Confine,
            paste_key: " ".to_string(),
            copy_file_key: " ".to_string(),
            up_key: " ".to_string(),
            down_key: " ".to_string(),
            left_key: " ".to_string(),
            right_key: " ".to_string(),
        };

        let (repaired, changed) = repair_host_state(state);

        assert!(changed);
        assert!(!repaired.attach_secret.trim().is_empty());
        assert_eq!(repaired.detach_key, default_detach_key());
        assert_eq!(repaired.paste_key, default_paste_key());
        assert_eq!(repaired.copy_file_key, default_copy_file_key());
        assert_eq!(repaired.up_key, default_up_key());
        assert_eq!(repaired.down_key, default_down_key());
        assert_eq!(repaired.left_key, default_left_key());
        assert_eq!(repaired.right_key, default_right_key());
        assert_eq!(repaired.remote_pointer_mode, RemotePointerMode::Confine);
    }

    #[test]
    fn repair_keeps_endpoint_id_when_valid() {
        let endpoint_id = sample_endpoint_id();
        let state = PersistedHostState {
            schema_version: 1,
            endpoint_id: endpoint_id.to_string(),
            attach_secret: "secret".to_string(),
            detach_key: default_detach_key(),
            remote_pointer_mode: default_remote_pointer_mode(),
            paste_key: default_paste_key(),
            copy_file_key: default_copy_file_key(),
            up_key: default_up_key(),
            down_key: default_down_key(),
            left_key: default_left_key(),
            right_key: default_right_key(),
        };

        let (repaired, changed) = repair_host_state(state);

        assert!(!changed);
        assert_eq!(repaired.endpoint_id, endpoint_id.to_string());
    }

    #[test]
    fn repair_keeps_valid_state_unchanged() {
        let endpoint_id = sample_endpoint_id();
        let state = PersistedHostState {
            schema_version: 1,
            endpoint_id: endpoint_id.to_string(),
            attach_secret: "already-good".to_string(),
            detach_key: default_detach_key(),
            remote_pointer_mode: default_remote_pointer_mode(),
            paste_key: default_paste_key(),
            copy_file_key: default_copy_file_key(),
            up_key: default_up_key(),
            down_key: default_down_key(),
            left_key: default_left_key(),
            right_key: default_right_key(),
        };

        let (repaired, changed) = repair_host_state(state);

        assert!(!changed);
        assert_eq!(repaired.endpoint_id, endpoint_id.to_string());
        assert_eq!(repaired.attach_secret, "already-good");
        assert_eq!(repaired.detach_key, default_detach_key());
        assert_eq!(repaired.remote_pointer_mode, default_remote_pointer_mode());
        assert_eq!(repaired.paste_key, default_paste_key());
        assert_eq!(repaired.copy_file_key, default_copy_file_key());
        assert_eq!(repaired.up_key, default_up_key());
        assert_eq!(repaired.down_key, default_down_key());
        assert_eq!(repaired.left_key, default_left_key());
        assert_eq!(repaired.right_key, default_right_key());
    }

    #[test]
    fn shortcut_conflicts_are_rejected_when_matching_can_overlap() {
        let endpoint_id = sample_endpoint_id();
        let state = PersistedHostState {
            schema_version: 1,
            endpoint_id: endpoint_id.to_string(),
            attach_secret: "secret".to_string(),
            detach_key: "ctrl+alt+cmd+l".to_string(),
            remote_pointer_mode: default_remote_pointer_mode(),
            paste_key: "ctrl+alt+cmd+p".to_string(),
            copy_file_key: "ctrl+alt+cmd+y".to_string(),
            up_key: "ctrl+alt+cmd+up".to_string(),
            down_key: "ctrl+alt+cmd+down".to_string(),
            left_key: "ctrl+alt+cmd+right".to_string(),
            right_key: "ctrl+alt+cmd+right".to_string(),
        };

        let err = validate_shortcut_conflicts(&state).expect_err("conflict should fail");
        assert!(err.to_string().contains("left_key and right_key"));
    }

    #[test]
    fn current_state_round_trips_with_all_shortcuts() {
        let endpoint_id = sample_endpoint_id();
        let state = PersistedHostState {
            schema_version: 1,
            endpoint_id: endpoint_id.to_string(),
            attach_secret: "secret".to_string(),
            detach_key: default_detach_key(),
            remote_pointer_mode: default_remote_pointer_mode(),
            paste_key: default_paste_key(),
            copy_file_key: default_copy_file_key(),
            up_key: default_up_key(),
            down_key: default_down_key(),
            left_key: default_left_key(),
            right_key: default_right_key(),
        };
        let encoded = serde_json::to_vec(&state).expect("serialize current state");
        let decoded: PersistedHostState =
            serde_json::from_slice(&encoded).expect("deserialize current state");
        assert_eq!(decoded.detach_key, default_detach_key());
        assert_eq!(decoded.up_key, default_up_key());
        assert_eq!(decoded.down_key, default_down_key());
        assert_eq!(decoded.left_key, default_left_key());
        assert_eq!(decoded.right_key, default_right_key());
    }

    #[test]
    fn state_without_shortcut_fields_is_rejected() {
        let raw = serde_json::json!({
            "schema_version": 1,
            "endpoint_id": "endpoint",
            "attach_secret": "secret",
            "remote_pointer_mode": "edge_to_edge"
        });
        assert!(serde_json::from_value::<PersistedHostState>(raw).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn advisory_lock_allows_only_one_owner() {
        let path = std::env::temp_dir().join(format!(
            "meow-test-client-lock-{}.lock",
            uuid::Uuid::new_v4().simple()
        ));
        let first = AdvisoryFileLock::acquire(&path).expect("first lock should succeed");
        assert!(AdvisoryFileLock::acquire(&path).is_err());
        drop(first);
        let second = AdvisoryFileLock::acquire(&path).expect("lock should be released");
        drop(second);
        fs::remove_file(path).expect("remove test lock");
    }

    #[test]
    fn client_identity_is_stable_across_loads() {
        let base = std::env::temp_dir().join(format!(
            "meow-test-client-identity-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&base).expect("create identity test directory");
        let identity_path = base.join("client.id");
        let lock_path = base.join("client.id.lock");

        let first = load_or_create_client_identity_at(&identity_path, &lock_path)
            .expect("create client identity");
        let second = load_or_create_client_identity_at(&identity_path, &lock_path)
            .expect("load client identity");
        assert_eq!(first, second);

        fs::remove_dir_all(base).expect("remove identity test directory");
    }

    #[test]
    fn invitation_parser_accepts_only_canonical_attach_commands() {
        let endpoint_id = sample_endpoint_id();
        let profile = parse_invitation(&format!("meow attach {endpoint_id} secret --side right"))
            .expect("canonical invitation should parse");
        assert_eq!(profile.host_id, endpoint_id.to_string());
        assert_eq!(profile.side, Side::Right);

        assert!(parse_invitation("meow attach id secret --side right; whoami").is_err());
        assert!(parse_invitation("sh -c 'meow attach id secret --side right'").is_err());
    }

    #[test]
    fn invitation_round_trip_formats_explicit_side() {
        let endpoint_id = sample_endpoint_id();
        let command = format_attach_command(endpoint_id, "secret", Side::Left);
        let profile = parse_invitation(&command).expect("formatted invitation should parse");
        assert_eq!(profile.side, Side::Left);
        assert_eq!(profile.secret, "secret");
    }

    #[test]
    fn read_only_credentials_reject_endpoint_mismatch() {
        let key = SecretKey::generate();
        let other_endpoint = sample_endpoint_id();
        let state = PersistedHostState {
            schema_version: 1,
            endpoint_id: other_endpoint.to_string(),
            attach_secret: "secret".to_string(),
            detach_key: default_detach_key(),
            remote_pointer_mode: default_remote_pointer_mode(),
            paste_key: default_paste_key(),
            copy_file_key: default_copy_file_key(),
            up_key: default_up_key(),
            down_key: default_down_key(),
            left_key: default_left_key(),
            right_key: default_right_key(),
        };
        let base = std::env::temp_dir().join(format!(
            "meow-credentials-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&base).expect("create credentials directory");
        let key_path = base.join("host.key");
        let state_path = base.join("host_state.json");
        fs::write(&key_path, key.to_bytes()).expect("write host key");
        write_host_state_file(&state_path, &state).expect("write host state");

        assert!(load_existing_host_credentials_from_paths(&key_path, &state_path).is_err());
        fs::remove_dir_all(base).expect("remove credentials directory");
    }
}
