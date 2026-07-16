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
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
        ],
        move |_proxy, event_type, event: &CGEvent| {
            let target = ActiveTarget::from_u8(active_target.load(Ordering::Relaxed));
            let is_remote = target.to_side().is_some();
            if !is_remote || !pointer_lock_active.load(Ordering::Relaxed) {
                return Some(event.clone());
            }

            if let Some(button_event) = map_other_mouse_button_event(event_type, event) {
                if tx
                    .send(CapturedInput {
                        target,
                        event: CapturedEvent::Raw(button_event),
                    })
                    .is_err()
                {
                    warn!("failed to queue macOS middle mouse button event for forwarding");
                }
                return None;
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

#[cfg(target_os = "macos")]
fn map_other_mouse_button_event(
    event_type: core_graphics::event::CGEventType,
    event: &core_graphics::event::CGEvent,
) -> Option<rdev::EventType> {
    use core_graphics::event::{CGEventType, EventField};
    use rdev::{Button, EventType};

    let button_number = event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
    if button_number != 2 {
        return None;
    }

    match event_type {
        CGEventType::OtherMouseDown => Some(EventType::ButtonPress(Button::Middle)),
        CGEventType::OtherMouseUp => Some(EventType::ButtonRelease(Button::Middle)),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn map_other_mouse_button_event_maps_middle_press() {
        use core_graphics::event::{CGEvent, CGEventType, CGMouseButton, EventField};
        use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
        use core_graphics::geometry::CGPoint;
        use rdev::{Button, EventType};

        let source =
            CGEventSource::new(CGEventSourceStateID::CombinedSessionState).expect("event source");
        let event = CGEvent::new_mouse_event(
            source,
            CGEventType::OtherMouseDown,
            CGPoint::new(100.0, 200.0),
            CGMouseButton::Center,
        )
        .expect("mouse event");
        event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, 2);

        let mapped = map_other_mouse_button_event(CGEventType::OtherMouseDown, &event);
        assert_eq!(mapped, Some(EventType::ButtonPress(Button::Middle)));
    }
}
