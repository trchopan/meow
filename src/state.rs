use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use iroh::{EndpointId, SecretKey};
use rand::{Rng, distributions::Alphanumeric, thread_rng};
use serde::{Deserialize, Serialize};

use crate::input::{
    default_clipboard_key, default_detach_key, default_down_key, default_left_key,
    default_right_key, default_up_key, parse_clipboard_chord, parse_detach_chord,
    parse_directional_chord,
};
use crate::model::RemotePointerMode;
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
    pub(crate) clipboard_key: String,
    pub(crate) up_key: String,
    pub(crate) down_key: String,
    pub(crate) left_key: String,
    pub(crate) right_key: String,
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

pub(crate) fn host_key_path() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("host.key"))
}

pub(crate) fn host_state_path() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("host_state.json"))
}

pub(crate) async fn reset_identity() -> Result<()> {
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
        let bytes = fs::read(&key_path)
            .with_context(|| format!("failed to read {}", key_path.display()))?;
        if bytes.len() != 32 {
            bail!("invalid host key length in {}", key_path.display());
        }
        let mut key_bytes = [0u8; 32];
        key_bytes.copy_from_slice(&bytes);
        return Ok(SecretKey::from_bytes(&key_bytes));
    }

    let secret = SecretKey::generate();
    write_secret_key_file(&key_path, &secret.to_bytes())?;
    Ok(secret)
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
        let (state, repaired) = repair_host_state_for_endpoint(state, endpoint_id);
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
        parse_clipboard_chord(&state.clipboard_key).with_context(|| {
            format!(
                "invalid clipboard_key {:?} in {}",
                state.clipboard_key,
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
        clipboard_key: default_clipboard_key(),
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
        (
            "clipboard_key",
            parse_clipboard_chord(&state.clipboard_key)?,
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

fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("missing parent directory for {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

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

fn repair_host_state_for_endpoint(
    mut state: PersistedHostState,
    endpoint_id: EndpointId,
) -> (PersistedHostState, bool) {
    let mut changed = false;

    if state.attach_secret.trim().is_empty() {
        state.attach_secret = random_secret();
        changed = true;
    }

    let endpoint_id_str = endpoint_id.to_string();
    if state.endpoint_id != endpoint_id_str {
        state.endpoint_id = endpoint_id_str;
        changed = true;
    }

    if state.detach_key.trim().is_empty() {
        state.detach_key = default_detach_key();
        changed = true;
    }

    if state.clipboard_key.trim().is_empty() {
        state.clipboard_key = default_clipboard_key();
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
            clipboard_key: " ".to_string(),
            up_key: " ".to_string(),
            down_key: " ".to_string(),
            left_key: " ".to_string(),
            right_key: " ".to_string(),
        };

        let (repaired, changed) = repair_host_state_for_endpoint(state, endpoint_id);

        assert!(changed);
        assert!(!repaired.attach_secret.trim().is_empty());
        assert_eq!(repaired.detach_key, default_detach_key());
        assert_eq!(repaired.clipboard_key, default_clipboard_key());
        assert_eq!(repaired.up_key, default_up_key());
        assert_eq!(repaired.down_key, default_down_key());
        assert_eq!(repaired.left_key, default_left_key());
        assert_eq!(repaired.right_key, default_right_key());
        assert_eq!(repaired.remote_pointer_mode, RemotePointerMode::Confine);
    }

    #[test]
    fn repair_updates_endpoint_id_when_mismatched() {
        let endpoint_id = sample_endpoint_id();
        let state = PersistedHostState {
            schema_version: 1,
            endpoint_id: "old-endpoint".to_string(),
            attach_secret: "secret".to_string(),
            detach_key: default_detach_key(),
            remote_pointer_mode: default_remote_pointer_mode(),
            clipboard_key: default_clipboard_key(),
            up_key: default_up_key(),
            down_key: default_down_key(),
            left_key: default_left_key(),
            right_key: default_right_key(),
        };

        let (repaired, changed) = repair_host_state_for_endpoint(state, endpoint_id);

        assert!(changed);
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
            clipboard_key: default_clipboard_key(),
            up_key: default_up_key(),
            down_key: default_down_key(),
            left_key: default_left_key(),
            right_key: default_right_key(),
        };

        let (repaired, changed) = repair_host_state_for_endpoint(state, endpoint_id);

        assert!(!changed);
        assert_eq!(repaired.endpoint_id, endpoint_id.to_string());
        assert_eq!(repaired.attach_secret, "already-good");
        assert_eq!(repaired.detach_key, default_detach_key());
        assert_eq!(repaired.remote_pointer_mode, default_remote_pointer_mode());
        assert_eq!(repaired.clipboard_key, default_clipboard_key());
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
            clipboard_key: "ctrl+alt+cmd+p".to_string(),
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
            clipboard_key: default_clipboard_key(),
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
}
