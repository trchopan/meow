use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use rdev::{Event, EventType, listen};

use crate::{
    display::display_layout,
    ipc::{IpcCommand, request_ipc},
};

pub struct MonitorArgs {
    pub duration_secs: u64,
}

pub async fn run_monitor(args: MonitorArgs) -> Result<()> {
    println!("=== MEOW EVENT & TAP MONITOR (Karabiner-EventViewer Mode) ===");
    println!("Monitoring keyboard, mouse events, and CGEventTap health in real time.");
    if args.duration_secs > 0 {
        println!(
            "Running for {} seconds (or press Ctrl+C to stop)...",
            args.duration_secs
        );
    } else {
        println!("Running indefinitely (press Ctrl+C to stop)...");
    }
    println!("------------------------------------------------------------");

    // Display summary
    if let Ok(layout) = display_layout() {
        println!("Detected {} active display(s):", layout.displays.len());
        for (i, d) in layout.displays.iter().enumerate() {
            println!(
                "  Display #{i}: origin=({:.0}, {:.0}) size=({:.0}x{:.0})",
                d.origin_x, d.origin_y, d.width, d.height
            );
        }
        println!("------------------------------------------------------------");
    }

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    // Spawn background thread to query IPC daemon health periodically
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        while r.load(Ordering::Relaxed) {
            interval.tick().await;
            if let Ok(res) = request_ipc(IpcCommand::Status).await
                && let Some(status) = res.status
            {
                let degraded = !status.pointer_tap_healthy || status.capture_tap_stopped > 0;
                println!(
                    "[DAEMON HEALTH] target={} | tap_healthy={} | captured={} | dropped_mouse={} | peers={}",
                    status.active,
                    if degraded {
                        "DEGRADED (⚠️)"
                    } else {
                        "OK (✅)"
                    },
                    status.captured_events,
                    status.captured_queue_full_mouse_dropped,
                    status.attached_peers.len()
                );
            }
        }
    });

    // Run passive input listener
    let start_time = Instant::now();
    let duration = (args.duration_secs > 0).then(|| Duration::from_secs(args.duration_secs));

    let stop_flag = running.clone();
    let _listener_handle = thread::spawn(move || {
        let callback = move |event: Event| {
            if !stop_flag.load(Ordering::Relaxed) {
                return;
            }
            let now = Instant::now().duration_since(start_time).as_secs_f64();
            match event.event_type {
                EventType::KeyPress(key) => {
                    println!("[{:7.3}s] KEY_DOWN: {key:?}", now);
                }
                EventType::KeyRelease(key) => {
                    println!("[{:7.3}s] KEY_UP:   {key:?}", now);
                }
                EventType::ButtonPress(button) => {
                    println!("[{:7.3}s] MOUSE_DOWN: {button:?}", now);
                }
                EventType::ButtonRelease(button) => {
                    println!("[{:7.3}s] MOUSE_UP:   {button:?}", now);
                }
                EventType::MouseMove { x, y } => {
                    // Sample mouse movement to avoid flooding
                    static LAST_PRINT: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let now_ms = (now * 1000.0) as u64;
                    let last = LAST_PRINT.load(Ordering::Relaxed);
                    if now_ms.saturating_sub(last) >= 100 {
                        LAST_PRINT.store(now_ms, Ordering::Relaxed);
                        println!("[{:7.3}s] MOUSE_MOVE: pos=({:.1}, {:.1})", now, x, y);
                    }
                }
                EventType::Wheel { delta_x, delta_y } => {
                    println!("[{:7.3}s] WHEEL: delta=({}, {})", now, delta_x, delta_y);
                }
            }
        };

        if let Err(err) = listen(callback) {
            eprintln!("Warning: Failed to start passive event listener: {err:?}");
        }
    });

    if let Some(dur) = duration {
        tokio::time::sleep(dur).await;
        running.store(false, Ordering::Relaxed);
    } else {
        // Wait for Ctrl+C
        tokio::signal::ctrl_c().await.ok();
        running.store(false, Ordering::Relaxed);
    }

    println!("\nEvent monitor finished.");
    Ok(())
}
