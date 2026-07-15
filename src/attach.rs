use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use enigo::{Enigo, MouseControllable};
use iroh::{Endpoint, EndpointId, SecretKey, endpoint::presets};
use rdev::{EventType, Key, simulate};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    cli::AttachArgs,
    model::ScreenEdge,
    protocol::{
        ALPN, AuthRequest, AuthResponse, MAX_MSG_SIZE, WireMessage, read_framed, send_wire_message,
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

    let response: AuthResponse = read_framed(&mut recv).await?;
    if !response.ok {
        bail!("host denied attach: {}", response.message);
    }

    println!("Attached to host as {:?}", args.side);
    info!("client attach complete, waiting for forwarded events");

    let mut enigo = Enigo::new();
    let mut probe = args
        .probe_received
        .then(|| ClientReceiveProbe::new(&mut enigo, args.probe_duration_secs));
    let probe_start = Instant::now();
    let mut last_signaled_edge: Option<ScreenEdge> = None;
    let mut edge_push = EdgePushTracker::new();

    loop {
        if let Some(probe) = probe.as_ref()
            && probe.is_finished(probe_start)
        {
            probe.print_summary();
            println!("probe complete");
            return Ok(());
        }

        let mut recv = connection.accept_uni().await?;
        let bytes = recv.read_to_end(MAX_MSG_SIZE).await?;
        debug!(
            "client received {} byte(s) on forwarded stream",
            bytes.len()
        );
        let message: WireMessage = bincode::deserialize(&bytes)?;
        match message {
            WireMessage::Input { event } => {
                if let Some(probe) = probe.as_mut() {
                    probe.on_input_event(&event, bytes.len(), probe_start.elapsed());
                }
                debug!("client received input event: {:?}", event);
                if !args.no_inject {
                    if let Err(err) = simulate(&event) {
                        warn!("failed injecting input event: {err:?}");
                    } else {
                        debug!("client simulate ok for event: {:?}", event);
                    }
                }
            }
            WireMessage::MouseMoveRelative { dx, dy } => {
                if let Some(probe) = probe.as_mut() {
                    probe.on_relative_mouse(
                        dx,
                        dy,
                        bytes.len(),
                        &mut enigo,
                        probe_start.elapsed(),
                        args.no_inject,
                    );
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
                        let message = WireMessage::ClientEdgeReached { edge };
                        if let Err(err) = send_wire_message(&connection, &message).await {
                            warn!("failed sending edge feedback to host: {err:#}");
                        } else {
                            debug!("sent edge feedback to host: {:?}", edge);
                            last_signaled_edge = Some(edge);
                        }
                    }
                }
            }
            WireMessage::ClientEdgeReached { .. } => {
                debug!("ignoring unexpected host->client edge feedback message");
            }
        }
    }
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
}

impl ClientReceiveProbe {
    fn new(enigo: &mut Enigo, duration_secs: u64) -> Self {
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
        }
    }

    fn is_finished(&self, probe_start: Instant) -> bool {
        self.duration_secs > 0 && probe_start.elapsed().as_secs() >= self.duration_secs
    }

    fn on_input_event(
        &mut self,
        event: &EventType,
        bytes_len: usize,
        elapsed: std::time::Duration,
    ) {
        self.note_message(bytes_len, elapsed);
        self.total_messages += 1;
        self.input_messages += 1;
        println!(
            "probe t={:.3}s msg={} bytes={} type=input event={:?}",
            elapsed.as_secs_f64(),
            self.total_messages,
            bytes_len,
            event
        );
    }

    fn on_relative_mouse(
        &mut self,
        dx: i32,
        dy: i32,
        bytes_len: usize,
        enigo: &mut Enigo,
        elapsed: std::time::Duration,
        no_inject: bool,
    ) {
        self.note_message(bytes_len, elapsed);
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

        println!(
            "probe t={:.3}s msg={} bytes={} type=rel dx={} dy={} before=({}, {}) after=({}, {}) actual=({}, {}) edge={} sum_abs=({}, {})",
            elapsed.as_secs_f64(),
            self.total_messages,
            bytes_len,
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
        }
        self.last_message_elapsed = Some(elapsed);
    }
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
}
