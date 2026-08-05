use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
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
    model::{
        ActiveTarget, CapturedEvent, CapturedInput, PendingClipboardRequest, RuntimeStats,
        ScreenEdge, Side,
    },
};

pub(crate) const DEFAULT_DETACH_KEY: &str = "ctrl+alt+cmd+u";
pub(crate) const DEFAULT_CLIPBOARD_KEY: &str = "ctrl+alt+cmd+p";
pub(crate) const DEFAULT_UP_KEY: &str = "ctrl+alt+cmd+k";
pub(crate) const DEFAULT_DOWN_KEY: &str = "ctrl+alt+cmd+j";
pub(crate) const DEFAULT_LEFT_KEY: &str = "ctrl+alt+cmd+h";
pub(crate) const DEFAULT_RIGHT_KEY: &str = "ctrl+alt+cmd+l";
pub(crate) const DEFAULT_EDGE_ZONE_PX: u32 = 12;
pub(crate) const DEFAULT_EDGE_DWELL_MS: u64 = 150;
const EDGE_REARM_DISTANCE_MULTIPLIER: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetachChord {
    pub(crate) key: Key,
    pub(crate) ctrl: bool,
    pub(crate) alt: bool,
    pub(crate) meta: bool,
    pub(crate) shift: bool,
    pub(crate) config_value: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct HostEdgeConfig {
    pub(crate) zone_px: u32,
    pub(crate) dwell: Duration,
}

impl HostEdgeConfig {
    pub(crate) fn new(zone_px: u32, dwell_ms: u64) -> Self {
        Self {
            zone_px,
            dwell: Duration::from_millis(dwell_ms),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_input_grab(
    tx: mpsc::Sender<CapturedInput>,
    runtime_stats: Arc<RuntimeStats>,
    active_target: Arc<AtomicU8>,
    target_epoch: Arc<AtomicU64>,
    pointer_lock_active: Arc<AtomicBool>,
    pointer_hidden: Arc<AtomicBool>,
    pinned_pointer_pos: Arc<Mutex<Option<(f64, f64)>>>,
    pending_release_sides: Arc<AtomicU8>,
    pending_clipboard_request: Arc<Mutex<Option<PendingClipboardRequest>>>,
    directional_switch_tx: mpsc::UnboundedSender<ActiveTarget>,
    detach_chord: DetachChord,
    clipboard_chord: DetachChord,
    up_chord: DetachChord,
    down_chord: DetachChord,
    left_chord: DetachChord,
    right_chord: DetachChord,
    edge_config: HostEdgeConfig,
) -> Result<()> {
    let send_ctx = CaptureSendContext {
        runtime_stats: runtime_stats.clone(),
        active_target: active_target.clone(),
        target_epoch: target_epoch.clone(),
        pointer_lock_active: pointer_lock_active.clone(),
        pointer_hidden: pointer_hidden.clone(),
        pinned_pointer_pos: pinned_pointer_pos.clone(),
        pending_release_sides: pending_release_sides.clone(),
        pending_clipboard_request: pending_clipboard_request.clone(),
    };

    let pressed_keys: Arc<Mutex<HashSet<Key>>> = Arc::new(Mutex::new(HashSet::new()));
    let last_mouse_pos: Arc<Mutex<Option<(f64, f64)>>> = Arc::new(Mutex::new(None));
    let mouse_position_generation = Arc::new(AtomicU64::new(0));
    let local_edge_zone: Arc<Mutex<EdgeZoneTracker>> = Arc::new(Mutex::new(EdgeZoneTracker::new()));
    let stop_edge_timer = Arc::new(AtomicBool::new(false));

    let timer_last_mouse_pos = last_mouse_pos.clone();
    let timer_position_generation = mouse_position_generation.clone();
    let timer_edge_zone = local_edge_zone.clone();
    let timer_active_target = active_target.clone();
    let timer_tx = tx.clone();
    let timer_send_ctx = send_ctx.clone();
    let timer_stop = stop_edge_timer.clone();
    let edge_timer = std::thread::spawn(move || {
        let mut previous_target = ActiveTarget::Local;
        let mut last_generation = 0;
        while !timer_stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));

            let target = ActiveTarget::from_u8(timer_active_target.load(Ordering::Relaxed));
            let generation = timer_position_generation.load(Ordering::Relaxed);
            let position = *timer_last_mouse_pos
                .lock()
                .expect("mouse position mutex poisoned");
            let mut edge_zone = timer_edge_zone.lock().expect("local edge mutex poisoned");

            if target != ActiveTarget::Local {
                edge_zone.reset();
                previous_target = target;
                last_generation = generation;
                continue;
            }
            if previous_target != ActiveTarget::Local {
                edge_zone.disarm(previous_target.to_side().map(edge_for_side));
                previous_target = ActiveTarget::Local;
                last_generation = generation;
                continue;
            }
            if generation == 0 {
                continue;
            }
            if generation == last_generation && edge_zone.edge.is_none() {
                continue;
            }
            last_generation = generation;

            let Some((x, y)) = position else {
                continue;
            };
            let display = rdev::display_size().unwrap_or((0, 0));
            edge_zone.rearm_if_far(x, y, display.0, display.1, edge_config.zone_px);
            let Some(edge) =
                detect_host_edge_zone_in_display(x, y, display.0, display.1, edge_config.zone_px)
            else {
                edge_zone.reset();
                continue;
            };
            try_send_host_edge(
                &timer_tx,
                &timer_send_ctx,
                &mut edge_zone,
                edge,
                Instant::now(),
                edge_config.dwell,
            );
        }
    });

    let callback_previous_target = Arc::new(AtomicU8::new(ActiveTarget::Local.to_u8()));
    let clipboard_key_active = Arc::new(AtomicBool::new(false));
    let callback_clipboard_key_active = clipboard_key_active.clone();
    let directional_key_active = Arc::new(Mutex::new(HashSet::new()));
    let callback_directional_key_active = directional_key_active.clone();
    let callback = move |event: rdev::Event| -> Option<rdev::Event> {
        runtime_stats
            .captured_events
            .fetch_add(1, Ordering::Relaxed);
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
        let previous_target =
            ActiveTarget::from_u8(callback_previous_target.load(Ordering::Relaxed));
        if target == ActiveTarget::Local && previous_target != ActiveTarget::Local {
            let mut edge_zone = local_edge_zone.lock().expect("local edge mutex poisoned");
            edge_zone.disarm(previous_target.to_side().map(edge_for_side));
        }
        callback_previous_target.store(target.to_u8(), Ordering::Relaxed);

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
            target_epoch.fetch_add(1, Ordering::AcqRel);
            pending_clipboard_request
                .lock()
                .expect("clipboard request mutex poisoned")
                .take();
            if let Some(side) = previous_target.to_side() {
                pending_release_sides.fetch_or(side.release_bit(), Ordering::AcqRel);
            }
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

        if cfg!(target_os = "macos") {
            let clipboard_modifiers_match = (!clipboard_chord.ctrl || is_ctrl_down)
                && (!clipboard_chord.alt || is_alt_down)
                && (!clipboard_chord.meta || is_meta_down)
                && (!clipboard_chord.shift || is_shift_down);
            match event.event_type {
                EventType::KeyPress(key)
                    if key == clipboard_chord.key && clipboard_modifiers_match =>
                {
                    callback_clipboard_key_active.store(true, Ordering::Relaxed);
                    try_send_captured_input(
                        &tx,
                        &send_ctx,
                        CapturedInput {
                            target,
                            event: CapturedEvent::ClipboardPaste,
                        },
                    );
                    return None;
                }
                EventType::KeyRelease(key)
                    if key == clipboard_chord.key
                        && callback_clipboard_key_active.load(Ordering::Relaxed) =>
                {
                    callback_clipboard_key_active.store(false, Ordering::Relaxed);
                    return None;
                }
                _ => {}
            }
        }

        let directional_chords = [
            (&up_chord, ActiveTarget::Up),
            (&down_chord, ActiveTarget::Down),
            (&left_chord, ActiveTarget::Left),
            (&right_chord, ActiveTarget::Right),
        ];
        match event.event_type {
            EventType::KeyPress(key) => {
                if let Some((_, target)) = directional_chords.iter().find(|(chord, _)| {
                    chord_matches_key(
                        chord,
                        key,
                        is_ctrl_down,
                        is_alt_down,
                        is_meta_down,
                        is_shift_down,
                    )
                }) {
                    let first_press = callback_directional_key_active
                        .lock()
                        .expect("directional key mutex poisoned")
                        .insert(key);
                    if first_press && directional_switch_tx.send(*target).is_err() {
                        warn!("directional shortcut channel closed; dropping target switch");
                    }
                    return None;
                }
            }
            EventType::KeyRelease(key)
                if callback_directional_key_active
                    .lock()
                    .expect("directional key mutex poisoned")
                    .remove(&key) =>
            {
                return None;
            }
            _ => {}
        }

        match target {
            ActiveTarget::Local => {
                if let EventType::MouseMove { x, y } = event.event_type {
                    {
                        let mut last_pos = last_mouse_pos.lock().expect("mouse pos mutex poisoned");
                        *last_pos = Some((x, y));
                    }
                    mouse_position_generation.fetch_add(1, Ordering::Relaxed);

                    let now = Instant::now();
                    let mut edge_zone = local_edge_zone.lock().expect("local edge mutex poisoned");
                    let display = rdev::display_size().unwrap_or((0, 0));
                    edge_zone.rearm_if_far(x, y, display.0, display.1, edge_config.zone_px);
                    if let Some(edge) = detect_host_edge_zone_in_display(
                        x,
                        y,
                        display.0,
                        display.1,
                        edge_config.zone_px,
                    ) {
                        try_send_host_edge(
                            &tx,
                            &send_ctx,
                            &mut edge_zone,
                            edge,
                            now,
                            edge_config.dwell,
                        );
                    } else {
                        edge_zone.reset();
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
                    other => normalize_non_motion_event(other),
                };
                runtime_stats
                    .normalized_events
                    .fetch_add(1, Ordering::Relaxed);

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

    let result = grab(callback).map_err(|e| anyhow!("input grab failed: {e:?}"));
    stop_edge_timer.store(true, Ordering::Relaxed);
    let _ = edge_timer.join();
    result
}

pub(crate) fn normalize_non_motion_event(event: EventType) -> CapturedEvent {
    match event {
        EventType::ButtonPress(button) => CapturedEvent::MouseButton {
            button,
            pressed: true,
        },
        EventType::ButtonRelease(button) => CapturedEvent::MouseButton {
            button,
            pressed: false,
        },
        EventType::Wheel { delta_x, delta_y } => CapturedEvent::MouseWheel { delta_x, delta_y },
        other => CapturedEvent::Raw(other),
    }
}

fn try_send_captured_input(
    tx: &mpsc::Sender<CapturedInput>,
    send_ctx: &CaptureSendContext,
    captured: CapturedInput,
) -> bool {
    try_send_captured_input_with_policy(tx, send_ctx, captured, false)
}

fn try_send_captured_input_with_policy(
    tx: &mpsc::Sender<CapturedInput>,
    send_ctx: &CaptureSendContext,
    captured: CapturedInput,
    drop_if_full: bool,
) -> bool {
    match tx.try_send(captured) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) if drop_if_full => {
            send_ctx
                .runtime_stats
                .captured_queue_full_mouse_dropped
                .fetch_add(1, Ordering::Relaxed);
            false
        }
        Err(TrySendError::Full(_)) => {
            send_ctx
                .runtime_stats
                .captured_queue_full_non_mouse_dropped
                .fetch_add(1, Ordering::Relaxed);
            force_local_on_capture_saturation(
                &send_ctx.active_target,
                &send_ctx.target_epoch,
                &send_ctx.pointer_lock_active,
                &send_ctx.pointer_hidden,
                &send_ctx.pinned_pointer_pos,
                &send_ctx.pending_release_sides,
                &send_ctx.pending_clipboard_request,
            );
            send_ctx
                .runtime_stats
                .recovery_events
                .fetch_add(1, Ordering::Relaxed);
            warn!("captured input queue full; dropping non-mouse event");
            false
        }
        Err(TrySendError::Closed(_)) => {
            warn!("captured input queue closed; dropping event");
            false
        }
    }
}

fn try_send_host_edge(
    tx: &mpsc::Sender<CapturedInput>,
    send_ctx: &CaptureSendContext,
    edge_zone: &mut EdgeZoneTracker,
    edge: ScreenEdge,
    now: Instant,
    dwell: Duration,
) {
    if !edge_zone.enter_or_stay(edge, now, dwell) {
        return;
    }

    let sent = try_send_captured_input(
        tx,
        send_ctx,
        CapturedInput {
            target: ActiveTarget::Local,
            event: CapturedEvent::HostEdgeReached { edge },
        },
    );
    if sent {
        edge_zone.mark_triggered();
    }
}

#[derive(Clone)]
struct CaptureSendContext {
    runtime_stats: Arc<RuntimeStats>,
    active_target: Arc<AtomicU8>,
    target_epoch: Arc<AtomicU64>,
    pointer_lock_active: Arc<AtomicBool>,
    pointer_hidden: Arc<AtomicBool>,
    pinned_pointer_pos: Arc<Mutex<Option<(f64, f64)>>>,
    pending_release_sides: Arc<AtomicU8>,
    pending_clipboard_request: Arc<Mutex<Option<PendingClipboardRequest>>>,
}

fn force_local_on_capture_saturation(
    active_target: &Arc<AtomicU8>,
    target_epoch: &Arc<AtomicU64>,
    pointer_lock_active: &Arc<AtomicBool>,
    pointer_hidden: &Arc<AtomicBool>,
    pinned_pointer_pos: &Arc<Mutex<Option<(f64, f64)>>>,
    pending_release_sides: &Arc<AtomicU8>,
    pending_clipboard_request: &Arc<Mutex<Option<PendingClipboardRequest>>>,
) {
    let target = ActiveTarget::from_u8(active_target.load(Ordering::Relaxed));
    if matches!(target, ActiveTarget::Local) {
        return;
    }

    active_target.store(ActiveTarget::Local.to_u8(), Ordering::Relaxed);
    target_epoch.fetch_add(1, Ordering::AcqRel);
    pending_clipboard_request
        .lock()
        .expect("clipboard request mutex poisoned")
        .take();
    if let Some(side) = target.to_side() {
        pending_release_sides.fetch_or(side.release_bit(), Ordering::AcqRel);
    }
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
    parse_chord(chord, "detach_key", DEFAULT_DETACH_KEY)
}

pub(crate) fn parse_clipboard_chord(chord: &str) -> Result<DetachChord> {
    parse_chord(chord, "clipboard_key", DEFAULT_CLIPBOARD_KEY)
}

pub(crate) fn parse_directional_chord(
    chord: &str,
    setting_name: &str,
    example: &str,
) -> Result<DetachChord> {
    parse_chord(chord, setting_name, example)
}

fn chord_matches_key(
    chord: &DetachChord,
    key: Key,
    ctrl: bool,
    alt: bool,
    meta: bool,
    shift: bool,
) -> bool {
    chord.key == key
        && (!chord.ctrl || ctrl)
        && (!chord.alt || alt)
        && (!chord.meta || meta)
        && (!chord.shift || shift)
}

fn parse_chord(chord: &str, setting_name: &str, example: &str) -> Result<DetachChord> {
    let mut ctrl = false;
    let mut alt = false;
    let mut meta = false;
    let mut shift = false;
    let mut key = None;

    for token in chord.split('+') {
        let token = token.trim().to_ascii_lowercase();
        if token.is_empty() {
            bail!("{setting_name} contains an empty token")
        }
        match token.as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" | "option" => alt = true,
            "cmd" | "command" | "meta" | "super" | "win" | "windows" => meta = true,
            "shift" => shift = true,
            _ => {
                if key.is_some() {
                    bail!("{setting_name} must include exactly one non-modifier key")
                }
                key = Some(parse_detach_key(&token, setting_name)?);
            }
        }
    }

    let key =
        key.ok_or_else(|| anyhow!("{setting_name} must include a key, for example {example}"))?;
    Ok(DetachChord {
        key,
        ctrl,
        alt,
        meta,
        shift,
        config_value: chord.trim().to_string(),
    })
}

pub(crate) fn parse_detach_key(token: &str, setting_name: &str) -> Result<Key> {
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
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        _ => {
            bail!(
                "unsupported {setting_name} key token {:?}; supported keys: a-z, 0-9, space, tab, enter, escape, up, down, left, right",
                token
            )
        }
    };
    Ok(key)
}

pub(crate) fn default_detach_key() -> String {
    DEFAULT_DETACH_KEY.to_string()
}

pub(crate) fn default_clipboard_key() -> String {
    DEFAULT_CLIPBOARD_KEY.to_string()
}

pub(crate) fn default_up_key() -> String {
    DEFAULT_UP_KEY.to_string()
}
pub(crate) fn default_down_key() -> String {
    DEFAULT_DOWN_KEY.to_string()
}
pub(crate) fn default_left_key() -> String {
    DEFAULT_LEFT_KEY.to_string()
}
pub(crate) fn default_right_key() -> String {
    DEFAULT_RIGHT_KEY.to_string()
}

pub(crate) fn clamp_relative_delta(delta: f64) -> i32 {
    const MAX_RELATIVE_MOUSE_DELTA: i32 = 10_000;
    (delta.round() as i32).clamp(-MAX_RELATIVE_MOUSE_DELTA, MAX_RELATIVE_MOUSE_DELTA)
}

fn detect_host_edge_zone_in_display(
    x: f64,
    y: f64,
    width: u64,
    height: u64,
    zone_px: u32,
) -> Option<ScreenEdge> {
    if width == 0 || height == 0 {
        return None;
    }

    let zone = zone_px as f64;
    let max_x = width.saturating_sub(1) as f64;
    let max_y = height.saturating_sub(1) as f64;

    if x <= zone {
        return Some(ScreenEdge::Left);
    }
    if x >= (max_x - zone).max(0.0) {
        return Some(ScreenEdge::Right);
    }
    if y <= zone {
        return Some(ScreenEdge::Up);
    }
    if y >= (max_y - zone).max(0.0) {
        return Some(ScreenEdge::Down);
    }

    None
}

struct EdgeZoneTracker {
    edge: Option<ScreenEdge>,
    entered_at: Option<Instant>,
    triggered: bool,
    disarmed_edge: Option<ScreenEdge>,
}

impl EdgeZoneTracker {
    fn new() -> Self {
        Self {
            edge: None,
            entered_at: None,
            triggered: false,
            disarmed_edge: None,
        }
    }

    fn enter_or_stay(&mut self, edge: ScreenEdge, now: Instant, dwell: Duration) -> bool {
        if self.disarmed_edge == Some(edge) {
            return false;
        }
        if self.edge != Some(edge) {
            self.edge = Some(edge);
            self.entered_at = Some(now);
            self.triggered = false;
        }

        !self.triggered
            && self
                .entered_at
                .is_some_and(|entered_at| now.duration_since(entered_at) >= dwell)
    }

    fn disarm(&mut self, edge: Option<ScreenEdge>) {
        self.edge = None;
        self.entered_at = None;
        self.triggered = false;
        self.disarmed_edge = edge;
    }

    fn rearm_if_far(&mut self, x: f64, y: f64, width: u64, height: u64, zone_px: u32) {
        let Some(edge) = self.disarmed_edge else {
            return;
        };
        if is_far_from_edge(x, y, width, height, edge, zone_px) {
            self.disarmed_edge = None;
        }
    }

    fn mark_triggered(&mut self) {
        self.triggered = true;
    }

    fn reset(&mut self) {
        self.edge = None;
        self.entered_at = None;
        self.triggered = false;
        self.disarmed_edge = None;
    }
}

fn edge_for_side(side: Side) -> ScreenEdge {
    match side {
        Side::Left => ScreenEdge::Left,
        Side::Right => ScreenEdge::Right,
        Side::Up => ScreenEdge::Up,
        Side::Down => ScreenEdge::Down,
    }
}

fn is_far_from_edge(
    x: f64,
    y: f64,
    width: u64,
    height: u64,
    edge: ScreenEdge,
    zone_px: u32,
) -> bool {
    let rearm_distance = zone_px.saturating_mul(EDGE_REARM_DISTANCE_MULTIPLIER) as f64;
    let max_x = width.saturating_sub(1) as f64;
    let max_y = height.saturating_sub(1) as f64;
    let rearm_x = rearm_distance.min((max_x - 1.0).max(0.0));
    let rearm_y = rearm_distance.min((max_y - 1.0).max(0.0));

    match edge {
        ScreenEdge::Left => x > rearm_x,
        ScreenEdge::Right => x < (max_x - rearm_x).max(0.0),
        ScreenEdge::Up => y > rearm_y,
        ScreenEdge::Down => y < (max_y - rearm_y).max(0.0),
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
    fn default_detach_chord_is_ctrl_alt_cmd_u() {
        let chord = parse_detach_chord(&default_detach_key()).expect("valid detach chord");
        assert_eq!(chord.key, Key::KeyU);
        assert!(chord.ctrl && chord.alt && chord.meta);
    }

    #[test]
    fn default_clipboard_chord_is_ctrl_alt_cmd_p() {
        let chord = parse_clipboard_chord(&default_clipboard_key()).expect("valid clipboard chord");
        assert_eq!(chord.key, Key::KeyP);
        assert!(chord.ctrl && chord.alt && chord.meta);
        assert!(!chord.shift);
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
    fn parse_directional_chord_accepts_vim_motion_keys() {
        for (value, key) in [
            (default_up_key(), Key::KeyK),
            (default_down_key(), Key::KeyJ),
            (default_left_key(), Key::KeyH),
            (default_right_key(), Key::KeyL),
        ] {
            let chord =
                parse_directional_chord(&value, "direction_key", &value).expect("valid chord");
            assert_eq!(chord.key, key);
            assert!(chord.ctrl && chord.alt && chord.meta);
        }
    }

    #[test]
    fn directional_chord_matching_requires_configured_modifiers() {
        let chord = parse_directional_chord("ctrl+alt+cmd+k", "up_key", DEFAULT_UP_KEY)
            .expect("valid chord");
        assert!(!chord_matches_key(
            &chord,
            Key::KeyK,
            true,
            true,
            false,
            false
        ));
        assert!(chord_matches_key(
            &chord,
            Key::KeyK,
            true,
            true,
            true,
            false
        ));
        assert!(!chord_matches_key(
            &chord,
            Key::KeyJ,
            true,
            true,
            true,
            false
        ));
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
        assert!(err.to_string().contains("unsupported detach_key key token"));
    }

    #[test]
    fn parse_detach_chord_rejects_missing_key() {
        let err = parse_detach_chord("ctrl+alt").expect_err("missing key should fail");
        assert!(
            err.to_string()
                .contains("detach_key must include a key, for example")
        );
    }

    #[test]
    fn normalize_non_motion_event_maps_all_standard_mouse_buttons() {
        for button in [
            rdev::Button::Left,
            rdev::Button::Middle,
            rdev::Button::Right,
        ] {
            assert!(matches!(
                normalize_non_motion_event(EventType::ButtonPress(button)),
                CapturedEvent::MouseButton {
                    button: mapped,
                    pressed: true
                } if mapped == button
            ));
            assert!(matches!(
                normalize_non_motion_event(EventType::ButtonRelease(button)),
                CapturedEvent::MouseButton {
                    button: mapped,
                    pressed: false
                } if mapped == button
            ));
        }
    }

    #[test]
    fn normalize_non_motion_event_preserves_wheel_axes() {
        assert!(matches!(
            normalize_non_motion_event(EventType::Wheel {
                delta_x: -3,
                delta_y: 7,
            }),
            CapturedEvent::MouseWheel {
                delta_x: -3,
                delta_y: 7,
            }
        ));
    }

    #[test]
    fn host_edge_zone_detects_each_display_edge() {
        let display = (1920, 1080);
        let zone = 12;

        assert_eq!(
            detect_host_edge_zone_in_display(12.0, 500.0, display.0, display.1, zone),
            Some(ScreenEdge::Left)
        );
        assert_eq!(
            detect_host_edge_zone_in_display(1907.0, 500.0, display.0, display.1, zone),
            Some(ScreenEdge::Right)
        );
        assert_eq!(
            detect_host_edge_zone_in_display(500.0, 12.0, display.0, display.1, zone),
            Some(ScreenEdge::Up)
        );
        assert_eq!(
            detect_host_edge_zone_in_display(500.0, 1067.0, display.0, display.1, zone),
            Some(ScreenEdge::Down)
        );
        assert_eq!(detect_host_edge_zone_in_display(0.0, 0.0, 0, 0, zone), None);
    }

    #[test]
    fn host_edge_zone_ignores_pointer_outside_zone() {
        assert_eq!(
            detect_host_edge_zone_in_display(100.0, 500.0, 1920, 1080, 12),
            None
        );
    }

    #[test]
    fn host_edge_zone_switches_once_until_rearmed() {
        let mut tracker = EdgeZoneTracker::new();
        let start = Instant::now();
        let dwell = Duration::from_millis(150);

        assert!(!tracker.enter_or_stay(ScreenEdge::Right, start, dwell));
        assert!(tracker.enter_or_stay(
            ScreenEdge::Right,
            start + Duration::from_millis(150),
            dwell
        ));
        tracker.mark_triggered();
        assert!(!tracker.enter_or_stay(
            ScreenEdge::Right,
            start + Duration::from_millis(300),
            dwell
        ));

        tracker.reset();
        assert!(!tracker.enter_or_stay(
            ScreenEdge::Right,
            start + Duration::from_millis(301),
            dwell
        ));
    }

    #[test]
    fn host_edge_zone_direction_change_starts_new_dwell() {
        let mut tracker = EdgeZoneTracker::new();
        let start = Instant::now();
        let dwell = Duration::from_millis(150);

        assert!(!tracker.enter_or_stay(ScreenEdge::Right, start, dwell));
        assert!(!tracker.enter_or_stay(
            ScreenEdge::Left,
            start + Duration::from_millis(149),
            dwell
        ));
        assert!(tracker.enter_or_stay(ScreenEdge::Left, start + Duration::from_millis(299), dwell));
    }

    #[test]
    fn host_edge_zone_can_complete_after_idle_dwell() {
        let mut tracker = EdgeZoneTracker::new();
        let start = Instant::now();
        let dwell = Duration::from_millis(150);

        assert!(!tracker.enter_or_stay(ScreenEdge::Right, start, dwell));
        assert!(tracker.enter_or_stay(
            ScreenEdge::Right,
            start + Duration::from_millis(150),
            dwell
        ));
    }

    #[test]
    fn host_edge_zone_requires_inward_rearm_after_remote_return() {
        let mut tracker = EdgeZoneTracker::new();
        let start = Instant::now();
        let dwell = Duration::from_millis(150);

        tracker.disarm(Some(ScreenEdge::Right));
        assert!(!tracker.enter_or_stay(ScreenEdge::Right, start + dwell, dwell));

        tracker.rearm_if_far(1894.0, 500.0, 1920, 1080, 12);
        assert!(!tracker.enter_or_stay(
            ScreenEdge::Right,
            start + dwell + Duration::from_millis(1),
            dwell
        ));

        tracker.rearm_if_far(25.0, 500.0, 1920, 1080, 12);
        assert!(!tracker.enter_or_stay(
            ScreenEdge::Right,
            start + dwell + Duration::from_millis(2),
            dwell
        ));
        assert!(tracker.enter_or_stay(
            ScreenEdge::Right,
            start + dwell + Duration::from_millis(151),
            dwell
        ));
    }

    #[test]
    fn host_edge_zone_rearms_when_pointer_is_far_from_return_edge() {
        let mut tracker = EdgeZoneTracker::new();
        let start = Instant::now();
        let dwell = Duration::from_millis(150);

        tracker.disarm(Some(ScreenEdge::Right));
        tracker.rearm_if_far(25.0, 500.0, 1920, 1080, 12);

        assert!(!tracker.enter_or_stay(ScreenEdge::Right, start, dwell));
        assert!(tracker.enter_or_stay(ScreenEdge::Right, start + dwell, dwell));
    }

    #[test]
    fn edge_rearm_distance_has_explicit_boundaries_for_each_edge() {
        let display = (1920, 1080);
        let zone = 12;

        assert!(!is_far_from_edge(
            24.0,
            500.0,
            display.0,
            display.1,
            ScreenEdge::Left,
            zone
        ));
        assert!(is_far_from_edge(
            25.0,
            500.0,
            display.0,
            display.1,
            ScreenEdge::Left,
            zone
        ));
        assert!(!is_far_from_edge(
            1895.0,
            500.0,
            display.0,
            display.1,
            ScreenEdge::Right,
            zone
        ));
        assert!(is_far_from_edge(
            1894.0,
            500.0,
            display.0,
            display.1,
            ScreenEdge::Right,
            zone
        ));
        assert!(!is_far_from_edge(
            500.0,
            24.0,
            display.0,
            display.1,
            ScreenEdge::Up,
            zone
        ));
        assert!(is_far_from_edge(
            500.0,
            25.0,
            display.0,
            display.1,
            ScreenEdge::Up,
            zone
        ));
        assert!(!is_far_from_edge(
            500.0,
            1055.0,
            display.0,
            display.1,
            ScreenEdge::Down,
            zone
        ));
        assert!(is_far_from_edge(
            500.0,
            1054.0,
            display.0,
            display.1,
            ScreenEdge::Down,
            zone
        ));
    }

    #[test]
    fn edge_rearm_distance_remains_reachable_for_large_zones() {
        let display = (1440, 900);
        let zone = 1000;

        assert!(is_far_from_edge(
            1439.0,
            450.0,
            display.0,
            display.1,
            ScreenEdge::Left,
            zone
        ));
        assert!(is_far_from_edge(
            0.0,
            450.0,
            display.0,
            display.1,
            ScreenEdge::Right,
            zone
        ));
        assert!(is_far_from_edge(
            720.0,
            899.0,
            display.0,
            display.1,
            ScreenEdge::Up,
            zone
        ));
        assert!(is_far_from_edge(
            720.0,
            0.0,
            display.0,
            display.1,
            ScreenEdge::Down,
            zone
        ));
    }

    #[test]
    fn host_edge_zone_retries_when_capture_queue_is_full() {
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(CapturedInput {
            target: ActiveTarget::Local,
            event: CapturedEvent::Raw(EventType::MouseMove { x: 0.0, y: 0.0 }),
        })
        .expect("test queue should accept its first item");

        let send_ctx = CaptureSendContext {
            runtime_stats: Arc::new(RuntimeStats::default()),
            active_target: Arc::new(AtomicU8::new(ActiveTarget::Local.to_u8())),
            target_epoch: Arc::new(AtomicU64::new(0)),
            pointer_lock_active: Arc::new(AtomicBool::new(false)),
            pointer_hidden: Arc::new(AtomicBool::new(false)),
            pinned_pointer_pos: Arc::new(Mutex::new(None)),
            pending_release_sides: Arc::new(AtomicU8::new(0)),
            pending_clipboard_request: Arc::new(Mutex::new(None)),
        };
        let mut tracker = EdgeZoneTracker::new();
        let start = Instant::now();
        let dwell = Duration::from_millis(150);

        try_send_host_edge(
            &tx,
            &send_ctx,
            &mut tracker,
            ScreenEdge::Right,
            start,
            dwell,
        );
        try_send_host_edge(
            &tx,
            &send_ctx,
            &mut tracker,
            ScreenEdge::Right,
            start + dwell,
            dwell,
        );

        assert!(!tracker.triggered);
    }

    #[test]
    fn normalize_non_motion_event_keeps_keyboard_events_raw() {
        assert!(matches!(
            normalize_non_motion_event(EventType::KeyPress(Key::KeyA)),
            CapturedEvent::Raw(EventType::KeyPress(Key::KeyA))
        ));
    }
}
