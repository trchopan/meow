use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Result, anyhow};

use crate::cli::ProbePointerLockArgs;

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
    use core_graphics::{
        display::CGDisplay,
        event::{
            CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
            CGEventType, EventField,
        },
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    pub(crate) async fn run(args: ProbePointerLockArgs) -> Result<()> {
        if args.duration_secs == 0 {
            return Err(anyhow!("duration must be > 0"));
        }

        let start_cursor = current_cursor_point()?;
        println!(
            "probe start: duration={}s start_cursor=({:.2},{:.2})",
            args.duration_secs, start_cursor.x, start_cursor.y
        );
        println!("pointer lock: ON");

        let guard = PointerLockGuard::enable()?;

        let total_dx = Arc::new(AtomicU64::new(0));
        let total_dy = Arc::new(AtomicU64::new(0));
        let event_count = Arc::new(AtomicU64::new(0));
        let pinned_cursor = (start_cursor.x, start_cursor.y);

        let total_dx_cb = total_dx.clone();
        let total_dy_cb = total_dy.clone();
        let event_count_cb = event_count.clone();

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
                let location = event.location();
                let dx = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X);
                let dy = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y);
                let sample = event_count_cb.fetch_add(1, Ordering::Relaxed) + 1;

                let pin_dx = location.x - pinned_cursor.0;
                let pin_dy = location.y - pinned_cursor.1;
                if let Err(err) = CGDisplay::warp_mouse_cursor_position(
                    core_graphics::geometry::CGPoint::new(pinned_cursor.0, pinned_cursor.1),
                ) {
                    println!("sample={} warp_back_failed={}", sample, err);
                }

                if dx >= 0 {
                    total_dx_cb.fetch_add(dx as u64, Ordering::Relaxed);
                } else {
                    total_dx_cb.fetch_add((-dx) as u64, Ordering::Relaxed);
                }
                if dy >= 0 {
                    total_dy_cb.fetch_add(dy as u64, Ordering::Relaxed);
                } else {
                    total_dy_cb.fetch_add((-dy) as u64, Ordering::Relaxed);
                }

                let cursor = current_cursor_point().unwrap_or(location);
                println!(
                    "sample={} type={:?} location=({:.2},{:.2}) delta=({},{}) pin_delta=({:.2},{:.2}) cursor_now=({:.2},{:.2})",
                    sample,
                    event_type,
                    location.x,
                    location.y,
                    dx,
                    dy,
                    pin_dx,
                    pin_dy,
                    cursor.x,
                    cursor.y
                );
                None
            },
        )
        .map_err(|_| anyhow!("failed to create CGEventTap (check Input Monitoring permission)"))?;

        let run_loop = CFRunLoop::get_current();
        let loop_source = tap
            .mach_port
            .create_runloop_source(0)
            .map_err(|_| anyhow!("failed to create runloop source for CGEventTap"))?;
        unsafe {
            run_loop.add_source(&loop_source, kCFRunLoopCommonModes);
        }
        tap.enable();

        let stop_loop = run_loop.clone();
        let duration = std::time::Duration::from_secs(args.duration_secs);
        std::thread::spawn(move || {
            std::thread::sleep(duration);
            stop_loop.stop();
        });

        CFRunLoop::run_current();

        let end_cursor = current_cursor_point()?;
        let total_samples = event_count.load(Ordering::Relaxed);
        let sum_abs_dx = total_dx.load(Ordering::Relaxed);
        let sum_abs_dy = total_dy.load(Ordering::Relaxed);
        println!(
            "probe end: samples={} sum_abs_dx={} sum_abs_dy={} end_cursor=({:.2},{:.2})",
            total_samples, sum_abs_dx, sum_abs_dy, end_cursor.x, end_cursor.y
        );

        drop(guard);
        println!("pointer lock: OFF");
        Ok(())
    }

    struct PointerLockGuard;

    impl PointerLockGuard {
        fn enable() -> Result<Self> {
            associate_mouse(false)?;
            Ok(Self)
        }
    }

    impl Drop for PointerLockGuard {
        fn drop(&mut self) {
            let _ = associate_mouse(true);
        }
    }

    fn associate_mouse(connected: bool) -> Result<()> {
        CGDisplay::associate_mouse_and_mouse_cursor_position(connected).map_err(|err| {
            anyhow!(
                "CGAssociateMouseAndMouseCursorPosition({}) failed with code {}",
                if connected { 1 } else { 0 },
                err
            )
        })
    }

    fn current_cursor_point() -> Result<core_graphics::geometry::CGPoint> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow!("failed to create CGEventSource"))?;
        let event = CGEvent::new(source).map_err(|_| anyhow!("failed to create CGEvent"))?;
        Ok(event.location())
    }
}

#[cfg(not(target_os = "macos"))]
mod macos {
    use super::*;

    pub(crate) async fn run(_args: ProbePointerLockArgs) -> Result<()> {
        Err(anyhow!("probe-pointer-lock is currently supported on macOS only"))
    }
}

pub(crate) async fn run_probe_pointer_lock(args: ProbePointerLockArgs) -> Result<()> {
    macos::run(args).await
}
