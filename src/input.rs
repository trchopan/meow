use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use rdev::{EventType, Key, grab};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{info, warn};

use crate::{
    host_mouse,
    model::{ActiveTarget, CapturedEvent, CapturedInput, RuntimeStats, ScreenEdge},
};

pub(crate) const DEFAULT_DETACH_KEY: &str = "ctrl+alt+cmd+l";
const EDGE_TOLERANCE_PX: f64 = 2.0;
const EDGE_PUSH_THRESHOLD_PX: f64 = 16.0;
const EDGE_PUSH_RESET_TIMEOUT: Duration = Duration::from_millis(250);

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
    tx: mpsc::Sender<CapturedInput>,
    runtime_stats: Arc<RuntimeStats>,
    active_target: Arc<AtomicU8>,
    pointer_lock_active: Arc<AtomicBool>,
    pointer_hidden: Arc<AtomicBool>,
    pinned_pointer_pos: Arc<Mutex<Option<(f64, f64)>>>,
    detach_chord: DetachChord,
) -> Result<()> {
    let send_ctx = CaptureSendContext {
        runtime_stats: runtime_stats.clone(),
        active_target: active_target.clone(),
        pointer_lock_active: pointer_lock_active.clone(),
        pointer_hidden: pointer_hidden.clone(),
        pinned_pointer_pos: pinned_pointer_pos.clone(),
    };

    let pressed_keys: Arc<Mutex<HashSet<Key>>> = Arc::new(Mutex::new(HashSet::new()));
    let last_mouse_pos: Arc<Mutex<Option<(f64, f64)>>> = Arc::new(Mutex::new(None));
    let local_edge_push: Arc<Mutex<EdgePushTracker>> = Arc::new(Mutex::new(EdgePushTracker::new()));

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
            ActiveTarget::Local => {
                if let EventType::MouseMove { x, y } = event.event_type {
                    let (dx, dy) = {
                        let mut last_pos = last_mouse_pos.lock().expect("mouse pos mutex poisoned");
                        let (dx, dy) = if let Some((last_x, last_y)) = *last_pos {
                            (x - last_x, y - last_y)
                        } else {
                            (0.0, 0.0)
                        };
                        *last_pos = Some((x, y));
                        (dx, dy)
                    };

                    let now = Instant::now();
                    let mut edge_push = local_edge_push.lock().expect("local edge mutex poisoned");
                    edge_push.reset_if_stale(now);
                    let push = detect_host_edge_push(x, y, dx, dy);
                    if let Some((edge, push_amount)) = push {
                        if edge_push.register_outward_push(edge, push_amount, now) {
                            try_send_captured_input(
                                &tx,
                                &send_ctx,
                                CapturedInput {
                                    target: ActiveTarget::Local,
                                    event: CapturedEvent::HostEdgeReached { edge },
                                },
                            );
                        }
                    } else {
                        edge_push.reset();
                    }
                }

                Some(event)
            }
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

                let drop_if_full =
                    matches!(&captured_event, CapturedEvent::MouseMoveRelative { .. });
                try_send_captured_input_with_policy(
                    &tx,
                    &send_ctx,
                    CapturedInput {
                        target,
                        event: captured_event,
                    },
                    drop_if_full,
                );
                None
            }
        }
    };

    grab(callback).map_err(|e| anyhow!("input grab failed: {e:?}"))
}

fn try_send_captured_input(
    tx: &mpsc::Sender<CapturedInput>,
    send_ctx: &CaptureSendContext,
    captured: CapturedInput,
) {
    try_send_captured_input_with_policy(tx, send_ctx, captured, false);
}

fn try_send_captured_input_with_policy(
    tx: &mpsc::Sender<CapturedInput>,
    send_ctx: &CaptureSendContext,
    captured: CapturedInput,
    drop_if_full: bool,
) {
    match tx.try_send(captured) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) if drop_if_full => {
            send_ctx
                .runtime_stats
                .captured_queue_full_mouse_dropped
                .fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Full(_)) => {
            send_ctx
                .runtime_stats
                .captured_queue_full_non_mouse_dropped
                .fetch_add(1, Ordering::Relaxed);
            force_local_on_capture_saturation(
                &send_ctx.active_target,
                &send_ctx.pointer_lock_active,
                &send_ctx.pointer_hidden,
                &send_ctx.pinned_pointer_pos,
            );
            warn!("captured input queue full; dropping non-mouse event");
        }
        Err(TrySendError::Closed(_)) => {
            warn!("captured input queue closed; dropping event");
        }
    }
}

#[derive(Clone)]
struct CaptureSendContext {
    runtime_stats: Arc<RuntimeStats>,
    active_target: Arc<AtomicU8>,
    pointer_lock_active: Arc<AtomicBool>,
    pointer_hidden: Arc<AtomicBool>,
    pinned_pointer_pos: Arc<Mutex<Option<(f64, f64)>>>,
}

fn force_local_on_capture_saturation(
    active_target: &Arc<AtomicU8>,
    pointer_lock_active: &Arc<AtomicBool>,
    pointer_hidden: &Arc<AtomicBool>,
    pinned_pointer_pos: &Arc<Mutex<Option<(f64, f64)>>>,
) {
    let target = ActiveTarget::from_u8(active_target.load(Ordering::Relaxed));
    if matches!(target, ActiveTarget::Local) {
        return;
    }

    active_target.store(ActiveTarget::Local.to_u8(), Ordering::Relaxed);
    pointer_lock_active.store(false, Ordering::Relaxed);
    if let Err(err) = host_mouse::set_pointer_dissociation(false) {
        warn!("failed to disable pointer dissociation after queue saturation: {err:#}");
    }
    let was_hidden = pointer_hidden.swap(false, Ordering::Relaxed);
    if was_hidden && let Err(err) = host_mouse::set_pointer_visible(true) {
        warn!("failed to show pointer after queue saturation: {err:#}");
        pointer_hidden.store(true, Ordering::Relaxed);
    }
    let mut pinned = pinned_pointer_pos
        .lock()
        .expect("pinned pointer mutex poisoned");
    *pinned = None;
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

fn detect_host_edge_push(x: f64, y: f64, dx: f64, dy: f64) -> Option<(ScreenEdge, f64)> {
    let (width, height) = rdev::display_size().unwrap_or((0, 0));
    let max_x = width.saturating_sub(1) as f64;
    let max_y = height.saturating_sub(1) as f64;

    if dx < 0.0 && x <= EDGE_TOLERANCE_PX {
        return Some((ScreenEdge::Left, -dx));
    }
    if dx > 0.0 && x >= (max_x - EDGE_TOLERANCE_PX).max(0.0) {
        return Some((ScreenEdge::Right, dx));
    }
    if dy < 0.0 && y <= EDGE_TOLERANCE_PX {
        return Some((ScreenEdge::Up, -dy));
    }
    if dy > 0.0 && y >= (max_y - EDGE_TOLERANCE_PX).max(0.0) {
        return Some((ScreenEdge::Down, dy));
    }

    None
}

struct EdgePushTracker {
    edge: Option<ScreenEdge>,
    accumulated_px: f64,
    last_update: Option<Instant>,
}

impl EdgePushTracker {
    fn new() -> Self {
        Self {
            edge: None,
            accumulated_px: 0.0,
            last_update: None,
        }
    }

    fn register_outward_push(&mut self, edge: ScreenEdge, push_px: f64, now: Instant) -> bool {
        if push_px <= 0.0 {
            return false;
        }

        if self.edge != Some(edge) {
            self.edge = Some(edge);
            self.accumulated_px = 0.0;
        }

        self.accumulated_px += push_px;
        self.last_update = Some(now);
        if self.accumulated_px >= EDGE_PUSH_THRESHOLD_PX {
            self.accumulated_px = 0.0;
            return true;
        }
        false
    }

    fn reset_if_stale(&mut self, now: Instant) {
        if let Some(last_update) = self.last_update
            && now.duration_since(last_update) >= EDGE_PUSH_RESET_TIMEOUT
        {
            self.reset();
        }
    }

    fn reset(&mut self) {
        self.edge = None;
        self.accumulated_px = 0.0;
        self.last_update = None;
    }
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
