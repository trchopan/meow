use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

use anyhow::Result;
use tokio::sync::mpsc;

use crate::{
    host_mouse,
    input::clamp_relative_delta,
    model::{ActiveTarget, CapturedEvent, CapturedInput},
};

#[cfg(target_os = "macos")]
pub(crate) fn run_macos_mouse_delta_capture(
    tx: mpsc::UnboundedSender<CapturedInput>,
    active_target: Arc<AtomicU8>,
    pointer_lock_active: Arc<AtomicBool>,
    pinned_pointer_pos: Arc<Mutex<Option<(f64, f64)>>>,
) -> Result<()> {
    use anyhow::anyhow;
    use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
    use core_graphics::event::{
        CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
        CGEventType, EventField,
    };
    use tracing::warn;

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
        move |_proxy, _event_type, event: &CGEvent| {
            let target = ActiveTarget::from_u8(active_target.load(Ordering::Relaxed));
            let is_remote = target.to_side().is_some();
            if !is_remote || !pointer_lock_active.load(Ordering::Relaxed) {
                return Some(event.clone());
            }

            let dx = clamp_relative_delta(
                event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as f64,
            );
            let dy = clamp_relative_delta(
                event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as f64,
            );

            if (dx != 0 || dy != 0)
                && tx
                    .send(CapturedInput {
                        target,
                        event: CapturedEvent::MouseMoveRelative { dx, dy },
                    })
                    .is_err()
            {
                warn!("failed to queue macOS relative mouse delta for forwarding");
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

    let run_loop = CFRunLoop::get_current();
    let loop_source = tap
        .mach_port
        .create_runloop_source(0)
        .map_err(|_| anyhow!("failed to create runloop source for macOS mouse CGEventTap"))?;
    unsafe {
        run_loop.add_source(&loop_source, kCFRunLoopCommonModes);
    }
    tap.enable();
    CFRunLoop::run_current();
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn run_macos_mouse_delta_capture(
    _tx: mpsc::UnboundedSender<CapturedInput>,
    _active_target: Arc<AtomicU8>,
    _pointer_lock_active: Arc<AtomicBool>,
    _pinned_pointer_pos: Arc<Mutex<Option<(f64, f64)>>>,
) -> Result<()> {
    Ok(())
}
