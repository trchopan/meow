use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

use anyhow::{Result, anyhow, bail};
use rdev::{EventType, Key, grab};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{
    host_mouse,
    model::{ActiveTarget, CapturedEvent, CapturedInput},
};

pub(crate) const DEFAULT_DETACH_KEY: &str = "ctrl+alt+cmd+l";

#[derive(Debug, Clone)]
pub(crate) struct DetachChord {
    pub(crate) key: Key,
    pub(crate) ctrl: bool,
    pub(crate) alt: bool,
    pub(crate) meta: bool,
    pub(crate) shift: bool,
    pub(crate) config_value: String,
}

pub(crate) fn run_input_grab(
    tx: mpsc::UnboundedSender<CapturedInput>,
    active_target: Arc<AtomicU8>,
    pointer_lock_active: Arc<AtomicBool>,
    pointer_hidden: Arc<AtomicBool>,
    pinned_pointer_pos: Arc<Mutex<Option<(f64, f64)>>>,
    detach_chord: DetachChord,
) -> Result<()> {
    let pressed_keys: Arc<Mutex<HashSet<Key>>> = Arc::new(Mutex::new(HashSet::new()));
    let last_mouse_pos: Arc<Mutex<Option<(f64, f64)>>> = Arc::new(Mutex::new(None));

    let callback = move |event: rdev::Event| -> Option<rdev::Event> {
        {
            let mut keys = pressed_keys.lock().expect("pressed key mutex poisoned");
            match event.event_type {
                EventType::KeyPress(key) => {
                    keys.insert(key);
                }
                EventType::KeyRelease(key) => {
                    keys.remove(&key);
                }
                _ => {}
            }
        }

        let target = ActiveTarget::from_u8(active_target.load(Ordering::Relaxed));

        if matches!(target, ActiveTarget::Local) {
            if let EventType::MouseMove { x, y } = event.event_type {
                let mut last_pos = last_mouse_pos.lock().expect("mouse pos mutex poisoned");
                *last_pos = Some((x, y));
            }
        }

        let (is_ctrl_down, is_alt_down, is_meta_down, is_shift_down) = {
            let keys = pressed_keys.lock().expect("pressed key mutex poisoned");
            let ctrl = keys.contains(&Key::ControlLeft) || keys.contains(&Key::ControlRight);
            let alt = keys.contains(&Key::Alt) || keys.contains(&Key::AltGr);
            let meta = keys.contains(&Key::MetaLeft) || keys.contains(&Key::MetaRight);
            let shift = keys.contains(&Key::ShiftLeft) || keys.contains(&Key::ShiftRight);
            (ctrl, alt, meta, shift)
        };

        if matches!(event.event_type, EventType::KeyPress(key) if key == detach_chord.key)
            && (!detach_chord.ctrl || is_ctrl_down)
            && (!detach_chord.alt || is_alt_down)
            && (!detach_chord.meta || is_meta_down)
            && (!detach_chord.shift || is_shift_down)
        {
            let previous_target = target;
            active_target.store(ActiveTarget::Local.to_u8(), Ordering::Relaxed);
            pointer_lock_active.store(false, Ordering::Relaxed);
            if let Err(err) = host_mouse::set_pointer_dissociation(false) {
                warn!("failed to disable pointer dissociation after detach chord: {err:#}");
            }
            let was_hidden = pointer_hidden.swap(false, Ordering::Relaxed);
            if was_hidden && let Err(err) = host_mouse::set_pointer_visible(true) {
                warn!("failed to show pointer after detach chord: {err:#}");
                pointer_hidden.store(true, Ordering::Relaxed);
            }
            {
                let mut pinned = pinned_pointer_pos
                    .lock()
                    .expect("pinned pointer mutex poisoned");
                *pinned = None;
            }
            info!(
                "escape chord {} detected in-grab; switched target from {} to local",
                detach_chord.config_value, previous_target
            );
            return None;
        }

        match target {
            ActiveTarget::Local => Some(event),
            _ => {
                let captured_event = match event.event_type {
                    EventType::MouseMove { x, y } => {
                        #[cfg(target_os = "macos")]
                        if pointer_lock_active.load(Ordering::Relaxed) {
                            return None;
                        }

                        let (dx, dy) = if pointer_lock_active.load(Ordering::Relaxed) {
                            let pinned = {
                                let pinned = pinned_pointer_pos
                                    .lock()
                                    .expect("pinned pointer mutex poisoned");
                                *pinned
                            };

                            if let Some((pin_x, pin_y)) = pinned {
                                let dx = x - pin_x;
                                let dy = y - pin_y;
                                if let Err(err) = host_mouse::warp_pointer(pin_x, pin_y) {
                                    warn!(
                                        "failed to warp pointer to pinned position ({pin_x:.2},{pin_y:.2}): {err:#}"
                                    );
                                }
                                (dx, dy)
                            } else {
                                (0.0, 0.0)
                            }
                        } else {
                            let mut last_pos =
                                last_mouse_pos.lock().expect("mouse pos mutex poisoned");
                            let (dx, dy) = if let Some((last_x, last_y)) = *last_pos {
                                (x - last_x, y - last_y)
                            } else {
                                (0.0, 0.0)
                            };
                            *last_pos = Some((x, y));
                            (dx, dy)
                        };

                        CapturedEvent::MouseMoveRelative {
                            dx: clamp_relative_delta(dx),
                            dy: clamp_relative_delta(dy),
                        }
                    }
                    other => CapturedEvent::Raw(other),
                };

                let _ = tx.send(CapturedInput {
                    target,
                    event: captured_event,
                });
                None
            }
        }
    };

    grab(callback).map_err(|e| anyhow!("input grab failed: {e:?}"))
}

pub(crate) fn parse_detach_chord(chord: &str) -> Result<DetachChord> {
    let mut ctrl = false;
    let mut alt = false;
    let mut meta = false;
    let mut shift = false;
    let mut key = None;

    for token in chord.split('+') {
        let token = token.trim().to_ascii_lowercase();
        if token.is_empty() {
            bail!("detach_key contains an empty token")
        }
        match token.as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" | "option" => alt = true,
            "cmd" | "command" | "meta" | "super" | "win" | "windows" => meta = true,
            "shift" => shift = true,
            _ => {
                if key.is_some() {
                    bail!("detach_key must include exactly one non-modifier key")
                }
                key = Some(parse_detach_key(&token)?);
            }
        }
    }

    let key = key.ok_or_else(|| {
        anyhow!(
            "detach_key must include a key, for example {}",
            DEFAULT_DETACH_KEY
        )
    })?;
    Ok(DetachChord {
        key,
        ctrl,
        alt,
        meta,
        shift,
        config_value: chord.trim().to_string(),
    })
}

pub(crate) fn parse_detach_key(token: &str) -> Result<Key> {
    let key = match token {
        "a" => Key::KeyA,
        "b" => Key::KeyB,
        "c" => Key::KeyC,
        "d" => Key::KeyD,
        "e" => Key::KeyE,
        "f" => Key::KeyF,
        "g" => Key::KeyG,
        "h" => Key::KeyH,
        "i" => Key::KeyI,
        "j" => Key::KeyJ,
        "k" => Key::KeyK,
        "l" => Key::KeyL,
        "m" => Key::KeyM,
        "n" => Key::KeyN,
        "o" => Key::KeyO,
        "p" => Key::KeyP,
        "q" => Key::KeyQ,
        "r" => Key::KeyR,
        "s" => Key::KeyS,
        "t" => Key::KeyT,
        "u" => Key::KeyU,
        "v" => Key::KeyV,
        "w" => Key::KeyW,
        "x" => Key::KeyX,
        "y" => Key::KeyY,
        "z" => Key::KeyZ,
        "0" => Key::Num0,
        "1" => Key::Num1,
        "2" => Key::Num2,
        "3" => Key::Num3,
        "4" => Key::Num4,
        "5" => Key::Num5,
        "6" => Key::Num6,
        "7" => Key::Num7,
        "8" => Key::Num8,
        "9" => Key::Num9,
        "space" => Key::Space,
        "tab" => Key::Tab,
        "enter" | "return" => Key::Return,
        "esc" | "escape" => Key::Escape,
        _ => {
            bail!(
                "unsupported detach key token {:?}; supported keys: a-z, 0-9, space, tab, enter, escape",
                token
            )
        }
    };
    Ok(key)
}

pub(crate) fn default_detach_key() -> String {
    DEFAULT_DETACH_KEY.to_string()
}

pub(crate) fn clamp_relative_delta(delta: f64) -> i32 {
    const MAX_RELATIVE_MOUSE_DELTA: i32 = 10_000;
    (delta.round() as i32).clamp(-MAX_RELATIVE_MOUSE_DELTA, MAX_RELATIVE_MOUSE_DELTA)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_detach_chord_accepts_modifiers_and_key_case_insensitively() {
        let chord = parse_detach_chord("Ctrl+Alt+Cmd+L").expect("valid chord should parse");
        assert_eq!(chord.key, Key::KeyL);
        assert!(chord.ctrl);
        assert!(chord.alt);
        assert!(chord.meta);
        assert!(!chord.shift);
        assert_eq!(chord.config_value, "Ctrl+Alt+Cmd+L");
    }

    #[test]
    fn parse_detach_chord_accepts_shift_and_named_key() {
        let chord = parse_detach_chord("shift+escape").expect("valid named key should parse");
        assert_eq!(chord.key, Key::Escape);
        assert!(!chord.ctrl);
        assert!(!chord.alt);
        assert!(!chord.meta);
        assert!(chord.shift);
    }

    #[test]
    fn parse_detach_chord_rejects_multiple_non_modifier_keys() {
        let err = parse_detach_chord("ctrl+a+b").expect_err("multiple keys should fail");
        assert!(
            err.to_string()
                .contains("must include exactly one non-modifier key")
        );
    }

    #[test]
    fn parse_detach_chord_rejects_unknown_key() {
        let err = parse_detach_chord("ctrl+f13").expect_err("unsupported key should fail");
        assert!(err.to_string().contains("unsupported detach key token"));
    }

    #[test]
    fn parse_detach_chord_rejects_missing_key() {
        let err = parse_detach_chord("ctrl+alt").expect_err("missing key should fail");
        assert!(
            err.to_string()
                .contains("detach_key must include a key, for example")
        );
    }
}
