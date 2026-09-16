use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

use anyhow::Result;
#[cfg(target_os = "macos")]
use core_foundation::runloop::CFRunLoop;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::{
    host_mouse,
    input::clamp_relative_delta,
    model::{ActiveTarget, CapturedEvent, CapturedInput, RuntimeStats, TARGET_TRANSITION_LOCK},
};

fn should_capture_motion(target: ActiveTarget, pointer_lock_active: bool) -> bool {
    target.to_side().is_some() && pointer_lock_active
}

const MAX_USER_DISABLE_RETRIES: u8 = 3;

#[derive(Clone)]
pub(crate) struct MouseDeltaControl {
    stop_requested: Arc<AtomicBool>,
    #[cfg(target_os = "macos")]
    run_loop: Arc<Mutex<Option<CFRunLoop>>>,
}

impl MouseDeltaControl {
    pub(crate) fn new() -> Self {
        Self {
            stop_requested: Arc::new(AtomicBool::new(false)),
            #[cfg(target_os = "macos")]
            run_loop: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
        #[cfg(target_os = "macos")]
        if let Some(run_loop) = self
            .run_loop
            .lock()
            .expect("mouse delta run loop mutex poisoned")
            .as_ref()
            .cloned()
        {
            run_loop.stop();
        }
    }

    #[cfg(target_os = "macos")]
    fn set_run_loop(&self, run_loop: CFRunLoop) {
        *self
            .run_loop
            .lock()
            .expect("mouse delta run loop mutex poisoned") = Some(run_loop);
        if self.stop_requested.load(Ordering::Acquire) {
            self.run_loop
                .lock()
                .expect("mouse delta run loop mutex poisoned")
                .as_ref()
                .expect("mouse delta run loop was just installed")
                .stop();
        }
    }

    #[cfg(target_os = "macos")]
    fn clear_run_loop(&self) {
        *self
            .run_loop
            .lock()
            .expect("mouse delta run loop mutex poisoned") = None;
    }
}

fn next_user_disable_attempt(attempt: u8, port_ready: bool) -> Option<u8> {
    port_ready
        .then_some(attempt)
        .filter(|attempt| *attempt < MAX_USER_DISABLE_RETRIES)
        .map(|attempt| attempt.saturating_add(1))
}

#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_macos_mouse_delta_capture(
    control: MouseDeltaControl,
    tx: mpsc::Sender<CapturedInput>,
    runtime_stats: Arc<RuntimeStats>,
    active_target: Arc<AtomicU8>,
    pointer_lock_active: Arc<AtomicBool>,
    pointer_hidden: Arc<AtomicBool>,
    pinned_pointer_pos: Arc<Mutex<Option<(f64, f64)>>>,
    pending_release_sides: Arc<AtomicU8>,
    pointer_tap_healthy: Arc<AtomicBool>,
) -> Result<()> {
    use anyhow::anyhow;
    use core_foundation::base::TCFType;
    use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
    use core_graphics::event::{
        CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
        CGEventType, EventField,
    };
    use std::sync::atomic::AtomicPtr;
    use tracing::{info, warn};

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventTapEnable(tap: *mut std::ffi::c_void, enable: bool);
    }

    let tap_port = Arc::new(AtomicPtr::<std::ffi::c_void>::new(std::ptr::null_mut()));
    let callback_tap_port = tap_port.clone();
    let user_disable_retries = Arc::new(AtomicU8::new(0));
    let callback_user_disable_retries = user_disable_retries.clone();
    let callback_pointer_tap_healthy = pointer_tap_healthy.clone();
    let callback_runtime_stats = runtime_stats.clone();

    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        vec![
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
        ],
        move |_proxy, event_type, event: &CGEvent| {
            let _transition_guard = TARGET_TRANSITION_LOCK
                .lock()
                .expect("target transition mutex poisoned");
            if matches!(
                event_type,
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
            ) {
                callback_pointer_tap_healthy.store(false, Ordering::Release);
                let port = callback_tap_port.load(Ordering::Relaxed);
                if matches!(event_type, CGEventType::TapDisabledByTimeout) {
                    if !port.is_null() {
                        unsafe { CGEventTapEnable(port, true) };
                        info!("re-enabled macOS mouse CGEventTap after timeout");
                    } else {
                        warn!("macOS mouse CGEventTap timed out before its port was ready");
                    }
                } else {
                    let attempt = callback_user_disable_retries.load(Ordering::Relaxed);
                    if let Some(attempt) = next_user_disable_attempt(attempt, !port.is_null()) {
                        callback_user_disable_retries.store(attempt, Ordering::Relaxed);
                        unsafe { CGEventTapEnable(port, true) };
                        info!(
                            "re-enabled macOS mouse CGEventTap after user-input disable (attempt {attempt}/{MAX_USER_DISABLE_RETRIES})"
                        );
                    } else if attempt >= MAX_USER_DISABLE_RETRIES {
                        warn!(
                            "macOS mouse CGEventTap remained disabled after {MAX_USER_DISABLE_RETRIES} user-input recovery attempts"
                        );
                    } else {
                        warn!("macOS mouse CGEventTap was disabled by user input before its port was ready");
                    }
                    callback_runtime_stats
                        .capture_tap_user_disabled
                        .fetch_add(1, Ordering::Relaxed);
                }
                let previous_target = ActiveTarget::from_u8(active_target.load(Ordering::Relaxed));
                active_target.store(ActiveTarget::Local.to_u8(), Ordering::Relaxed);
                if let Some(side) = previous_target.to_side() {
                    pending_release_sides.fetch_or(side.release_bit(), Ordering::AcqRel);
                }
                pointer_lock_active.store(false, Ordering::Relaxed);
                callback_runtime_stats
                    .recovery_events
                    .fetch_add(1, Ordering::Relaxed);
                if let Err(err) = host_mouse::set_pointer_dissociation(false) {
                    warn!("failed to restore pointer association after tap disable: {err:#}");
                }
                if pointer_hidden.swap(false, Ordering::Relaxed)
                    && let Err(err) = host_mouse::set_pointer_visible(true)
                {
                    warn!("failed to show pointer after tap disable: {err:#}");
                    pointer_hidden.store(true, Ordering::Relaxed);
                }
                *pinned_pointer_pos
                    .lock()
                    .expect("pinned pointer mutex poisoned") = None;
                return Some(event.clone());
            }
            callback_pointer_tap_healthy.store(true, Ordering::Release);
            let target = ActiveTarget::from_u8(active_target.load(Ordering::Relaxed));
            if !should_capture_motion(target, pointer_lock_active.load(Ordering::Relaxed)) {
                return Some(event.clone());
            }

            let dx = clamp_relative_delta(
                event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as f64,
            );
            let dy = clamp_relative_delta(
                event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as f64,
            );

            if dx != 0 || dy != 0 {
                match tx.try_send(CapturedInput {
                    target,
                    event: CapturedEvent::MouseMoveRelative { dx, dy },
                }) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        callback_runtime_stats
                            .captured_queue_full_mouse_dropped
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Closed(_)) => {
                        warn!("failed to queue macOS relative mouse delta for forwarding");
                    }
                }
            }

            let pinned = {
                let pinned = pinned_pointer_pos
                    .lock()
                    .expect("pinned pointer mutex poisoned");
                *pinned
            };
            if let Some((pin_x, pin_y)) = pinned
                && let Err(err) = host_mouse::warp_pointer(pin_x, pin_y)
            {
                warn!("failed to warp pointer to pinned position ({pin_x:.2},{pin_y:.2}): {err:#}");
            }

            None
        },
    )
    .map_err(|_| anyhow!("failed to create macOS mouse CGEventTap"))?;

    tap_port.store(
        tap.mach_port.as_concrete_TypeRef() as *mut std::ffi::c_void,
        Ordering::Relaxed,
    );

    let run_loop = CFRunLoop::get_current();
    control.set_run_loop(run_loop.clone());
    let loop_source = tap
        .mach_port
        .create_runloop_source(0)
        .map_err(|_| anyhow!("failed to create runloop source for macOS mouse CGEventTap"))?;
    unsafe {
        run_loop.add_source(&loop_source, kCFRunLoopCommonModes);
    }
    tap.enable();
    pointer_tap_healthy.store(true, Ordering::Release);
    CFRunLoop::run_current();
    control.clear_run_loop();
    pointer_tap_healthy.store(false, Ordering::Release);
    runtime_stats
        .capture_tap_stopped
        .fetch_add(1, Ordering::Relaxed);
    Ok(())
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_macos_mouse_delta_capture(
    _control: MouseDeltaControl,
    _tx: mpsc::Sender<CapturedInput>,
    _runtime_stats: Arc<RuntimeStats>,
    _active_target: Arc<AtomicU8>,
    _pointer_lock_active: Arc<AtomicBool>,
    _pointer_hidden: Arc<AtomicBool>,
    _pinned_pointer_pos: Arc<Mutex<Option<(f64, f64)>>>,
    _pending_release_sides: Arc<AtomicU8>,
    _pointer_tap_healthy: Arc<AtomicBool>,
) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motion_capture_requires_remote_target_and_pointer_lock() {
        assert!(!should_capture_motion(ActiveTarget::Local, true));
        assert!(!should_capture_motion(ActiveTarget::Right, false));
        assert!(should_capture_motion(ActiveTarget::Right, true));
    }

    #[test]
    fn user_disable_recovery_is_bounded_and_requires_port() {
        assert_eq!(next_user_disable_attempt(0, true), Some(1));
        assert_eq!(
            next_user_disable_attempt(MAX_USER_DISABLE_RETRIES, true),
            None
        );
        assert_eq!(next_user_disable_attempt(0, false), None);
    }
}
