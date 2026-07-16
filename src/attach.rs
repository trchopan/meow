use std::collections::HashSet;
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use enigo::{Enigo, MouseButton, MouseControllable};
use iroh::{Endpoint, EndpointId, SecretKey, endpoint::presets};
use rdev::{Button, EventType, Key, simulate};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    cli::AttachArgs,
    model::ScreenEdge,
    protocol::{
        ALPN, AuthRequest, AuthResponse, ClientToHostMessage, HostToClientMessage,
        MAX_AUTH_MSG_SIZE, read_framed_with_limit, read_framed_with_size, send_client_feedback,
        write_framed,
    },
};

const EDGE_TOLERANCE_PX: i32 = 2;
const EDGE_PUSH_THRESHOLD_PX: i32 = 16;
const EDGE_PUSH_RESET_TIMEOUT: Duration = Duration::from_millis(250);

pub(crate) async fn run_attach(args: AttachArgs) -> Result<()> {
    let host_id = EndpointId::from_str(&args.host_id).context("invalid host endpoint id")?;
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(SecretKey::generate())
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .context("failed to create iroh endpoint")?;

    let connection = endpoint
        .connect(host_id, ALPN)
        .await
        .context("failed to connect to host")?;

    let (mut send, mut recv) = connection.open_bi().await?;
    let auth = AuthRequest {
        secret: args.secret,
        side: args.side,
        name: format!("remote-{}", Uuid::new_v4().simple()),
    };
    write_framed(&mut send, &auth).await?;

    let response: AuthResponse = tokio::time::timeout(
        Duration::from_secs(5),
        read_framed_with_limit(&mut recv, MAX_AUTH_MSG_SIZE),
    )
    .await
    .context("timed out waiting for auth response")??;
    if !response.ok {
        bail!("host denied attach: {}", response.message);
    }

    println!("Attached to host as {:?}", args.side);
    info!("client attach complete, waiting for forwarded events");

    let mut enigo = Enigo::new();
    let mut probe = args.probe_received.then(|| {
        ClientReceiveProbe::new(
            &mut enigo,
            args.probe_duration_secs,
            !args.probe_summary_only,
        )
    });
    let probe_start = Instant::now();
    let mut last_signaled_edge: Option<ScreenEdge> = None;
    let mut edge_push = EdgePushTracker::new();
    let mut input_state = ClientInputState::default();
    let mut sequence_tracker = SequenceTracker::default();

    let mut probe_completed = false;
    let run_result: Result<()> = loop {
        if let Some(probe) = probe.as_ref()
            && probe.is_finished(probe_start)
        {
            probe_completed = true;
            break Ok(());
        }

        let (message, frame_size): (HostToClientMessage, usize) =
            match read_framed_with_size(&mut recv).await {
                Ok(frame) => frame,
                Err(err) => break Err(err),
            };
        debug!("client received {} byte(s) on forwarded stream", frame_size);
        let elapsed = probe_start.elapsed();
        match message {
            HostToClientMessage::Input { seq, event } => {
                let seq_status = sequence_tracker.observe(seq);
                let meta = ProbeMessageMeta {
                    seq,
                    bytes_len: frame_size,
                    elapsed,
                };
                if let Some(probe) = probe.as_mut() {
                    probe.note_sequence(seq_status);
                    probe.on_input_event(
                        &event,
                        meta,
                        &mut enigo,
                        &mut input_state,
                        args.no_inject,
                    );
                    probe.note_held_counts(&input_state);
                }
                debug!("client received input event: {:?}", event);
                if probe.is_none() && !args.no_inject {
                    if let Err(err) = inject_input_event(&event, &mut enigo, &mut input_state) {
                        warn!("failed injecting input event: {err}");
                    } else {
                        debug!("client injection ok for event: {:?}", event);
                    }
                }
            }
            HostToClientMessage::MouseMoveRelative { seq, dx, dy } => {
                let seq_status = sequence_tracker.observe(seq);
                let meta = ProbeMessageMeta {
                    seq,
                    bytes_len: frame_size,
                    elapsed,
                };
                if let Some(probe) = probe.as_mut() {
                    probe.note_sequence(seq_status);
                    probe.on_relative_mouse(dx, dy, meta, &mut enigo, args.no_inject);
                    probe.note_held_counts(&input_state);
                }
                debug!("client received relative mouse move: dx={dx}, dy={dy}");
                if probe.is_none() && !args.no_inject {
                    let before = enigo.mouse_location();
                    enigo.mouse_move_relative(dx, dy);
                    let after = enigo.mouse_location();
                    let display = enigo.main_display_size();
                    edge_push.reset_if_stale(Instant::now());
                    let push = detect_client_edge_push(before, after, display, dx, dy);
                    let Some((edge, push_amount)) = push else {
                        edge_push.reset();
                        last_signaled_edge = None;
                        continue;
                    };

                    if edge_push.register_outward_push(edge, push_amount, Instant::now())
                        && Some(edge) != last_signaled_edge
                    {
                        let message = ClientToHostMessage::ClientEdgeReached { edge };
                        if let Err(err) = send_client_feedback(&connection, &message).await {
                            warn!("failed sending edge feedback to host: {err:#}");
                        } else {
                            debug!("sent edge feedback to host: {:?}", edge);
                            last_signaled_edge = Some(edge);
                        }
                    }
                }
            }
            HostToClientMessage::ReleaseAll { seq } => {
                let seq_status = sequence_tracker.observe(seq);
                if let Some(probe) = probe.as_mut() {
                    probe.note_sequence(seq_status);
                    probe.on_release_all(
                        seq,
                        frame_size,
                        &mut enigo,
                        &mut input_state,
                        elapsed,
                        args.no_inject,
                    );
                    probe.note_held_counts(&input_state);
                } else if !args.no_inject {
                    let failures = release_all_pressed_inputs(&mut enigo, &mut input_state);
                    if failures > 0 {
                        warn!("release-all had {failures} injection failure(s)");
                    }
                }
            }
        }
    };

    if !args.no_inject {
        let failures = release_all_pressed_inputs(&mut enigo, &mut input_state);
        if failures > 0 {
            warn!("attach cleanup had {failures} injection failure(s)");
        }
    }

    if let Some(probe) = probe.as_mut() {
        probe.note_held_counts(&input_state);
        if probe_completed {
            probe.print_summary();
            println!("probe complete");
            return Ok(());
        }
    }

    run_result
}

fn detect_client_edge_push(
    before: (i32, i32),
    after: (i32, i32),
    display: (i32, i32),
    dx: i32,
    dy: i32,
) -> Option<(ScreenEdge, i32)> {
    let max_x = display.0.saturating_sub(1);
    let max_y = display.1.saturating_sub(1);
    let actual_dx = after.0 - before.0;
    let actual_dy = after.1 - before.1;

    if dx < 0 && after.0 <= EDGE_TOLERANCE_PX {
        let requested = -dx;
        let actual_outward = (-actual_dx).max(0);
        let blocked = requested.saturating_sub(actual_outward);
        if blocked > 0 {
            return Some((ScreenEdge::Left, blocked));
        }
    }
    if dx > 0 && after.0 >= max_x.saturating_sub(EDGE_TOLERANCE_PX) {
        let requested = dx;
        let actual_outward = actual_dx.max(0);
        let blocked = requested.saturating_sub(actual_outward);
        if blocked > 0 {
            return Some((ScreenEdge::Right, blocked));
        }
    }
    if dy < 0 && after.1 <= EDGE_TOLERANCE_PX {
        let requested = -dy;
        let actual_outward = (-actual_dy).max(0);
        let blocked = requested.saturating_sub(actual_outward);
        if blocked > 0 {
            return Some((ScreenEdge::Up, blocked));
        }
    }
    if dy > 0 && after.1 >= max_y.saturating_sub(EDGE_TOLERANCE_PX) {
        let requested = dy;
        let actual_outward = actual_dy.max(0);
        let blocked = requested.saturating_sub(actual_outward);
        if blocked > 0 {
            return Some((ScreenEdge::Down, blocked));
        }
    }

    None
}

fn inject_input_event(
    event: &EventType,
    enigo: &mut Enigo,
    state: &mut ClientInputState,
) -> Result<()> {
    if should_skip_client_key_event(event) {
        return Ok(());
    }

    if let Some((button, pressed)) = map_mouse_button_event(event) {
        if pressed {
            enigo.mouse_down(button);
        } else {
            enigo.mouse_up(button);
        }
        if let EventType::ButtonPress(raw_button) = event {
            state.pressed_buttons.insert(*raw_button);
        } else if let EventType::ButtonRelease(raw_button) = event {
            state.pressed_buttons.remove(raw_button);
        }
        return Ok(());
    }

    simulate(event).map_err(|err| anyhow::anyhow!("{err:?}"))?;
    match event {
        EventType::KeyPress(key) => {
            state.pressed_keys.insert(*key);
        }
        EventType::KeyRelease(key) => {
            state.pressed_keys.remove(key);
        }
        _ => {}
    }
    Ok(())
}

fn map_mouse_button_event(event: &EventType) -> Option<(MouseButton, bool)> {
    match event {
        EventType::ButtonPress(button) => {
            map_rdev_mouse_button(*button).map(|mapped| (mapped, true))
        }
        EventType::ButtonRelease(button) => {
            map_rdev_mouse_button(*button).map(|mapped| (mapped, false))
        }
        _ => None,
    }
}

fn map_rdev_mouse_button(button: Button) -> Option<MouseButton> {
    match button {
        Button::Left => Some(MouseButton::Left),
        Button::Middle => Some(MouseButton::Middle),
        Button::Right => Some(MouseButton::Right),
        Button::Unknown(_) => None,
    }
}

#[cfg(target_os = "macos")]
fn should_skip_client_key_event(event: &EventType) -> bool {
    matches!(
        event,
        EventType::KeyPress(Key::Function)
            | EventType::KeyRelease(Key::Function)
            | EventType::KeyPress(Key::Unknown(63))
            | EventType::KeyRelease(Key::Unknown(63))
    )
}

#[cfg(not(target_os = "macos"))]
fn should_skip_client_key_event(_event: &EventType) -> bool {
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProbeButtonEvent {
    pressed: bool,
    middle: bool,
}

fn classify_probe_button_event(event: &EventType) -> Option<ProbeButtonEvent> {
    match event {
        EventType::ButtonPress(button) => Some(ProbeButtonEvent {
            pressed: true,
            middle: matches!(button, Button::Middle),
        }),
        EventType::ButtonRelease(button) => Some(ProbeButtonEvent {
            pressed: false,
            middle: matches!(button, Button::Middle),
        }),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SequenceStatus {
    InOrder,
    Gap { missing: u64 },
    DuplicateOrOutOfOrder,
}

#[derive(Default)]
struct SequenceTracker {
    last_seq: Option<u64>,
}

impl SequenceTracker {
    fn observe(&mut self, seq: u64) -> SequenceStatus {
        let status = match self.last_seq {
            None => SequenceStatus::InOrder,
            Some(last) if seq == last.saturating_add(1) => SequenceStatus::InOrder,
            Some(last) if seq > last.saturating_add(1) => SequenceStatus::Gap {
                missing: seq.saturating_sub(last).saturating_sub(1),
            },
            Some(_) => SequenceStatus::DuplicateOrOutOfOrder,
        };
        if self.last_seq.is_none_or(|last| seq > last) {
            self.last_seq = Some(seq);
        }
        status
    }
}

#[derive(Default)]
struct ClientInputState {
    pressed_keys: HashSet<Key>,
    pressed_buttons: HashSet<Button>,
}

fn release_all_pressed_inputs(enigo: &mut Enigo, state: &mut ClientInputState) -> u64 {
    let mut failures = 0u64;

    let mut buttons = state.pressed_buttons.iter().copied().collect::<Vec<_>>();
    buttons.sort_by_key(button_release_order);
    for button in buttons {
        if let Some(mapped) = map_rdev_mouse_button(button) {
            enigo.mouse_up(mapped);
        }
        state.pressed_buttons.remove(&button);
    }

    let mut keys = state.pressed_keys.iter().copied().collect::<Vec<_>>();
    keys.sort_by_key(key_release_order);
    for key in keys {
        let event = EventType::KeyRelease(key);
        if should_skip_client_key_event(&event) {
            state.pressed_keys.remove(&key);
            continue;
        }
        if let Err(err) = simulate(&event) {
            warn!("failed releasing stuck key {:?}: {err:?}", key);
            failures = failures.saturating_add(1);
            continue;
        }
        state.pressed_keys.remove(&key);
    }

    failures
}

fn button_release_order(button: &Button) -> u8 {
    match button {
        Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
        Button::Unknown(_) => 3,
    }
}

fn key_release_order(key: &Key) -> u8 {
    if is_modifier_key(*key) { 1 } else { 0 }
}

fn is_modifier_key(key: Key) -> bool {
    matches!(
        key,
        Key::ShiftLeft
            | Key::ShiftRight
            | Key::ControlLeft
            | Key::ControlRight
            | Key::Alt
            | Key::AltGr
            | Key::MetaLeft
            | Key::MetaRight
    )
}

struct EdgePushTracker {
    edge: Option<ScreenEdge>,
    accumulated_px: i32,
    last_update: Option<Instant>,
}

impl EdgePushTracker {
    fn new() -> Self {
        Self {
            edge: None,
            accumulated_px: 0,
            last_update: None,
        }
    }

    fn register_outward_push(&mut self, edge: ScreenEdge, push_px: i32, now: Instant) -> bool {
        if push_px <= 0 {
            return false;
        }

        if self.edge != Some(edge) {
            self.edge = Some(edge);
            self.accumulated_px = 0;
        }

        self.accumulated_px = self.accumulated_px.saturating_add(push_px);
        self.last_update = Some(now);
        if self.accumulated_px >= EDGE_PUSH_THRESHOLD_PX {
            self.accumulated_px = 0;
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
        self.accumulated_px = 0;
        self.last_update = None;
    }
}

struct ClientReceiveProbe {
    start_cursor: (i32, i32),
    display_size: (i32, i32),
    duration_secs: u64,
    total_messages: u64,
    relative_messages: u64,
    input_messages: u64,
    input_button_presses: u64,
    input_button_releases: u64,
    middle_button_presses: u64,
    middle_button_releases: u64,
    input_injection_failures: u64,
    release_all_messages: u64,
    sequence_gaps: u64,
    sequence_out_of_order: u64,
    held_keys: usize,
    held_buttons: usize,
    sum_dx: i64,
    sum_dy: i64,
    sum_abs_dx: u64,
    sum_abs_dy: u64,
    sum_cursor_dx: i64,
    sum_cursor_dy: i64,
    edge_clamps: u64,
    zero_delta_messages: u64,
    max_abs_dx: i32,
    max_abs_dy: i32,
    total_bytes: u64,
    min_bytes: Option<usize>,
    max_bytes: usize,
    first_message_elapsed: Option<std::time::Duration>,
    last_message_elapsed: Option<std::time::Duration>,
    max_inter_message_gap: std::time::Duration,
    inter_message_gaps_ms: Vec<f64>,
    verbose_events: bool,
}

#[derive(Clone, Copy)]
struct ProbeMessageMeta {
    seq: u64,
    bytes_len: usize,
    elapsed: std::time::Duration,
}

impl ClientReceiveProbe {
    fn new(enigo: &mut Enigo, duration_secs: u64, verbose_events: bool) -> Self {
        let display_size = enigo.main_display_size();
        let start_cursor = enigo.mouse_location();
        println!(
            "client probe start: duration={}s display=({},{}) cursor_start=({},{})",
            duration_secs, display_size.0, display_size.1, start_cursor.0, start_cursor.1
        );
        Self {
            start_cursor,
            display_size,
            duration_secs,
            total_messages: 0,
            relative_messages: 0,
            input_messages: 0,
            input_button_presses: 0,
            input_button_releases: 0,
            middle_button_presses: 0,
            middle_button_releases: 0,
            input_injection_failures: 0,
            release_all_messages: 0,
            sequence_gaps: 0,
            sequence_out_of_order: 0,
            held_keys: 0,
            held_buttons: 0,
            sum_dx: 0,
            sum_dy: 0,
            sum_abs_dx: 0,
            sum_abs_dy: 0,
            sum_cursor_dx: 0,
            sum_cursor_dy: 0,
            edge_clamps: 0,
            zero_delta_messages: 0,
            max_abs_dx: 0,
            max_abs_dy: 0,
            total_bytes: 0,
            min_bytes: None,
            max_bytes: 0,
            first_message_elapsed: None,
            last_message_elapsed: None,
            max_inter_message_gap: std::time::Duration::ZERO,
            inter_message_gaps_ms: Vec::new(),
            verbose_events,
        }
    }

    fn is_finished(&self, probe_start: Instant) -> bool {
        self.duration_secs > 0 && probe_start.elapsed().as_secs() >= self.duration_secs
    }

    fn on_input_event(
        &mut self,
        event: &EventType,
        meta: ProbeMessageMeta,
        enigo: &mut Enigo,
        input_state: &mut ClientInputState,
        no_inject: bool,
    ) {
        self.note_message(meta.bytes_len, meta.elapsed);
        self.total_messages += 1;
        self.input_messages += 1;
        if let Some(button_event) = classify_probe_button_event(event) {
            if button_event.pressed {
                self.input_button_presses += 1;
                if button_event.middle {
                    self.middle_button_presses += 1;
                }
            } else {
                self.input_button_releases += 1;
                if button_event.middle {
                    self.middle_button_releases += 1;
                }
            }
        }

        let injection_status = if no_inject {
            "skip"
        } else if let Err(err) = inject_input_event(event, enigo, input_state) {
            self.input_injection_failures += 1;
            warn!("probe failed injecting input event {:?}: {err}", event);
            "fail"
        } else {
            "ok"
        };

        if self.verbose_events {
            println!(
                "probe t={:.3}s msg={} seq={} bytes={} type=input event={:?} inject={}",
                meta.elapsed.as_secs_f64(),
                self.total_messages,
                meta.seq,
                meta.bytes_len,
                event,
                injection_status
            );
        }
    }

    fn on_relative_mouse(
        &mut self,
        dx: i32,
        dy: i32,
        meta: ProbeMessageMeta,
        enigo: &mut Enigo,
        no_inject: bool,
    ) {
        self.note_message(meta.bytes_len, meta.elapsed);
        self.total_messages += 1;
        self.relative_messages += 1;
        self.sum_dx += dx as i64;
        self.sum_dy += dy as i64;
        self.sum_abs_dx += dx.unsigned_abs() as u64;
        self.sum_abs_dy += dy.unsigned_abs() as u64;
        self.max_abs_dx = self.max_abs_dx.max(dx.abs());
        self.max_abs_dy = self.max_abs_dy.max(dy.abs());
        if dx == 0 && dy == 0 {
            self.zero_delta_messages += 1;
        }

        let before = enigo.mouse_location();
        let (width, height) = self.display_size;
        let expected_x = (before.0 + dx).clamp(0, width.saturating_sub(1));
        let expected_y = (before.1 + dy).clamp(0, height.saturating_sub(1));

        if !no_inject {
            enigo.mouse_move_relative(dx, dy);
        }

        let after = enigo.mouse_location();
        let actual_dx = after.0 - before.0;
        let actual_dy = after.1 - before.1;
        self.sum_cursor_dx += actual_dx as i64;
        self.sum_cursor_dy += actual_dy as i64;

        let hit_edge = after.0 <= 0
            || after.1 <= 0
            || after.0 >= width.saturating_sub(1)
            || after.1 >= height.saturating_sub(1)
            || after.0 != expected_x
            || after.1 != expected_y;
        if hit_edge {
            self.edge_clamps += 1;
        }

        if self.verbose_events {
            println!(
                "probe t={:.3}s msg={} seq={} bytes={} type=rel dx={} dy={} before=({}, {}) after=({}, {}) actual=({}, {}) edge={} sum_abs=({}, {})",
                meta.elapsed.as_secs_f64(),
                self.total_messages,
                meta.seq,
                meta.bytes_len,
                dx,
                dy,
                before.0,
                before.1,
                after.0,
                after.1,
                actual_dx,
                actual_dy,
                hit_edge,
                self.sum_abs_dx,
                self.sum_abs_dy
            );
        }
    }

    fn on_release_all(
        &mut self,
        seq: u64,
        bytes_len: usize,
        enigo: &mut Enigo,
        input_state: &mut ClientInputState,
        elapsed: std::time::Duration,
        no_inject: bool,
    ) {
        self.note_message(bytes_len, elapsed);
        self.total_messages += 1;
        self.release_all_messages += 1;

        let status = if no_inject {
            "skip"
        } else {
            let failures = release_all_pressed_inputs(enigo, input_state);
            self.input_injection_failures = self.input_injection_failures.saturating_add(failures);
            if failures == 0 { "ok" } else { "fail" }
        };

        if self.verbose_events {
            println!(
                "probe t={:.3}s msg={} seq={} bytes={} type=release_all inject={}",
                elapsed.as_secs_f64(),
                self.total_messages,
                seq,
                bytes_len,
                status
            );
        }
    }

    fn note_sequence(&mut self, status: SequenceStatus) {
        match status {
            SequenceStatus::InOrder => {}
            SequenceStatus::Gap { missing } => {
                self.sequence_gaps = self.sequence_gaps.saturating_add(missing);
            }
            SequenceStatus::DuplicateOrOutOfOrder => {
                self.sequence_out_of_order = self.sequence_out_of_order.saturating_add(1);
            }
        }
    }

    fn note_held_counts(&mut self, input_state: &ClientInputState) {
        self.held_keys = input_state.pressed_keys.len();
        self.held_buttons = input_state.pressed_buttons.len();
    }

    fn print_summary(&self) {
        let observed_secs = self
            .last_message_elapsed
            .unwrap_or(std::time::Duration::ZERO)
            .as_secs_f64()
            .max(0.001);
        let msg_rate = self.total_messages as f64 / observed_secs;
        let byte_rate = self.total_bytes as f64 / observed_secs;
        let avg_bytes = if self.total_messages == 0 {
            0.0
        } else {
            self.total_bytes as f64 / self.total_messages as f64
        };
        let p50_gap_ms = percentile(&self.inter_message_gaps_ms, 50.0);
        let p95_gap_ms = percentile(&self.inter_message_gaps_ms, 95.0);
        let p99_gap_ms = percentile(&self.inter_message_gaps_ms, 99.0);

        println!("client probe summary:");
        println!(
            "  display_size=({}, {})",
            self.display_size.0, self.display_size.1
        );
        println!(
            "  start_cursor=({}, {})",
            self.start_cursor.0, self.start_cursor.1
        );
        println!("  total_messages={}", self.total_messages);
        println!("  relative_messages={}", self.relative_messages);
        println!("  input_messages={}", self.input_messages);
        println!(
            "  input_button_presses={} input_button_releases={}",
            self.input_button_presses, self.input_button_releases
        );
        println!(
            "  middle_button_presses={} middle_button_releases={}",
            self.middle_button_presses, self.middle_button_releases
        );
        println!(
            "  input_injection_failures={}",
            self.input_injection_failures
        );
        println!("  release_all_messages={}", self.release_all_messages);
        println!("  sequence_gaps={}", self.sequence_gaps);
        println!("  sequence_out_of_order={}", self.sequence_out_of_order);
        println!(
            "  held_keys_end={} held_buttons_end={}",
            self.held_keys, self.held_buttons
        );
        println!("  sum_dx={} sum_dy={}", self.sum_dx, self.sum_dy);
        println!(
            "  sum_abs_dx={} sum_abs_dy={}",
            self.sum_abs_dx, self.sum_abs_dy
        );
        println!(
            "  sum_cursor_dx={} sum_cursor_dy={}",
            self.sum_cursor_dx, self.sum_cursor_dy
        );
        println!("  edge_clamps={}", self.edge_clamps);
        println!("  zero_delta_messages={}", self.zero_delta_messages);
        println!(
            "  max_abs_dx={} max_abs_dy={}",
            self.max_abs_dx, self.max_abs_dy
        );
        println!(
            "  bytes_total={} avg_bytes_per_msg={:.2}",
            self.total_bytes, avg_bytes
        );
        println!(
            "  bytes_min={} bytes_max={}",
            self.min_bytes.unwrap_or(0),
            self.max_bytes
        );
        println!("  msg_rate_per_sec={:.2}", msg_rate);
        println!("  bytes_rate_per_sec={:.2}", byte_rate);
        println!(
            "  first_msg_t={:.3}s last_msg_t={:.3}s max_inter_msg_gap={:.3}s",
            self.first_message_elapsed
                .unwrap_or(std::time::Duration::ZERO)
                .as_secs_f64(),
            self.last_message_elapsed
                .unwrap_or(std::time::Duration::ZERO)
                .as_secs_f64(),
            self.max_inter_message_gap.as_secs_f64()
        );
        println!(
            "  inter_msg_gap_ms_p50={:.3} inter_msg_gap_ms_p95={:.3} inter_msg_gap_ms_p99={:.3}",
            p50_gap_ms, p95_gap_ms, p99_gap_ms
        );
    }

    fn note_message(&mut self, bytes_len: usize, elapsed: std::time::Duration) {
        self.total_bytes = self.total_bytes.saturating_add(bytes_len as u64);
        self.max_bytes = self.max_bytes.max(bytes_len);
        self.min_bytes = Some(match self.min_bytes {
            Some(current) => current.min(bytes_len),
            None => bytes_len,
        });

        if self.first_message_elapsed.is_none() {
            self.first_message_elapsed = Some(elapsed);
        }
        if let Some(last) = self.last_message_elapsed {
            let gap = elapsed.saturating_sub(last);
            self.max_inter_message_gap = self.max_inter_message_gap.max(gap);
            self.inter_message_gaps_ms.push(gap.as_secs_f64() * 1000.0);
        }
        self.last_message_elapsed = Some(elapsed);
    }
}

fn percentile(values: &[f64], percentile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let rank = ((percentile / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank]
}

pub(crate) async fn run_test_inject() -> Result<()> {
    println!("running local injection test in 2 seconds...");
    println!("this will move mouse slightly and type 'meowtest'");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let (x, y) = rdev::display_size().unwrap_or((0, 0));
    let center_x = (x as f64 / 2.0).max(20.0);
    let center_y = (y as f64 / 2.0).max(20.0);

    let sequence = [
        EventType::MouseMove {
            x: center_x,
            y: center_y,
        },
        EventType::MouseMove {
            x: center_x + 20.0,
            y: center_y + 20.0,
        },
        EventType::MouseMove {
            x: center_x,
            y: center_y,
        },
        EventType::KeyPress(Key::KeyM),
        EventType::KeyRelease(Key::KeyM),
        EventType::KeyPress(Key::KeyE),
        EventType::KeyRelease(Key::KeyE),
        EventType::KeyPress(Key::KeyO),
        EventType::KeyRelease(Key::KeyO),
        EventType::KeyPress(Key::KeyW),
        EventType::KeyRelease(Key::KeyW),
        EventType::KeyPress(Key::KeyT),
        EventType::KeyRelease(Key::KeyT),
        EventType::KeyPress(Key::KeyE),
        EventType::KeyRelease(Key::KeyE),
        EventType::KeyPress(Key::KeyS),
        EventType::KeyRelease(Key::KeyS),
        EventType::KeyPress(Key::KeyT),
        EventType::KeyRelease(Key::KeyT),
    ];

    for event in sequence {
        debug!("test-inject event: {:?}", event);
        match simulate(&event) {
            Ok(()) => debug!("test-inject simulate ok: {:?}", event),
            Err(err) => {
                warn!("test-inject simulate failed for {:?}: {err:?}", event);
                bail!("local input injection test failed; check Accessibility permission")
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(35)).await;
    }

    println!("test-inject finished");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_client_edge_push_left_when_blocked() {
        let edge = detect_client_edge_push((1, 100), (0, 100), (1920, 1080), -20, 0);
        assert_eq!(edge, Some((ScreenEdge::Left, 19)));
    }

    #[test]
    fn detect_client_edge_push_right_when_blocked() {
        let edge = detect_client_edge_push((1918, 100), (1919, 100), (1920, 1080), 12, 0);
        assert_eq!(edge, Some((ScreenEdge::Right, 11)));
    }

    #[test]
    fn detect_client_edge_push_none_when_not_at_edge() {
        let edge = detect_client_edge_push((100, 100), (104, 100), (1920, 1080), 4, 0);
        assert_eq!(edge, None);
    }

    #[test]
    fn map_mouse_button_event_maps_middle_press() {
        let mapped = map_mouse_button_event(&EventType::ButtonPress(Button::Middle));
        assert_eq!(mapped, Some((MouseButton::Middle, true)));
    }

    #[test]
    fn map_mouse_button_event_maps_left_release() {
        let mapped = map_mouse_button_event(&EventType::ButtonRelease(Button::Left));
        assert_eq!(mapped, Some((MouseButton::Left, false)));
    }

    #[test]
    fn map_mouse_button_event_ignores_unknown_button() {
        let mapped = map_mouse_button_event(&EventType::ButtonPress(Button::Unknown(8)));
        assert_eq!(mapped, None);
    }

    #[test]
    fn map_mouse_button_event_ignores_non_button_event() {
        let mapped = map_mouse_button_event(&EventType::KeyPress(Key::KeyA));
        assert_eq!(mapped, None);
    }

    #[test]
    fn classify_probe_button_event_tracks_middle_press() {
        let event = classify_probe_button_event(&EventType::ButtonPress(Button::Middle));
        assert_eq!(
            event,
            Some(ProbeButtonEvent {
                pressed: true,
                middle: true,
            })
        );
    }

    #[test]
    fn classify_probe_button_event_tracks_release_non_middle() {
        let event = classify_probe_button_event(&EventType::ButtonRelease(Button::Left));
        assert_eq!(
            event,
            Some(ProbeButtonEvent {
                pressed: false,
                middle: false,
            })
        );
    }

    #[test]
    fn classify_probe_button_event_ignores_non_button_event() {
        let event = classify_probe_button_event(&EventType::KeyPress(Key::KeyA));
        assert_eq!(event, None);
    }

    #[test]
    fn sequence_tracker_detects_gap_and_out_of_order() {
        let mut tracker = SequenceTracker::default();
        assert_eq!(tracker.observe(1), SequenceStatus::InOrder);
        assert_eq!(tracker.observe(3), SequenceStatus::Gap { missing: 1 });
        assert_eq!(tracker.observe(2), SequenceStatus::DuplicateOrOutOfOrder);
    }

    #[test]
    fn key_release_order_puts_modifiers_last() {
        assert!(key_release_order(&Key::KeyA) < key_release_order(&Key::MetaLeft));
        assert!(key_release_order(&Key::KeyA) < key_release_order(&Key::ControlLeft));
    }

    #[test]
    fn should_skip_client_key_event_for_function_key_is_platform_specific() {
        let press = EventType::KeyPress(Key::Function);
        let release = EventType::KeyRelease(Key::Function);

        #[cfg(target_os = "macos")]
        {
            assert!(should_skip_client_key_event(&press));
            assert!(should_skip_client_key_event(&release));
        }

        #[cfg(not(target_os = "macos"))]
        {
            assert!(!should_skip_client_key_event(&press));
            assert!(!should_skip_client_key_event(&release));
        }
    }

    #[test]
    fn should_skip_client_key_event_keeps_non_function_keys() {
        let event = EventType::KeyPress(Key::KeyA);
        assert!(!should_skip_client_key_event(&event));
    }

    #[test]
    fn should_skip_client_key_event_for_raw_function_keycode_on_macos() {
        let press = EventType::KeyPress(Key::Unknown(63));
        let release = EventType::KeyRelease(Key::Unknown(63));

        #[cfg(target_os = "macos")]
        {
            assert!(should_skip_client_key_event(&press));
            assert!(should_skip_client_key_event(&release));
        }

        #[cfg(not(target_os = "macos"))]
        {
            assert!(!should_skip_client_key_event(&press));
            assert!(!should_skip_client_key_event(&release));
        }
    }
}
