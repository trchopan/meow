use crate::{ipc::StatusPayload, model::Side};

pub(crate) fn print_host_ready(endpoint_id: &str, secret: &str) {
    println!("Host endpoint id: {endpoint_id}");
    println!("Session secret: {secret}");
    println!();
    println!("On remote machine, run:");
    println!("  meow attach {endpoint_id} {secret} --side right");
    println!();
    println!("Use meow local/right/left/up/down to switch target.");
}

pub(crate) fn print_identity_reset_complete() {
    println!("identity reset complete");
    println!("next `meow host` run will generate a new host id");
}

pub(crate) fn print_rotate_secret_complete(endpoint_id: &str, secret: &str) {
    println!("attach secret rotated");
    println!("new attach command:");
    println!("  meow attach {endpoint_id} {secret} --side right");
}

pub(crate) fn print_status_response(message: &str, status: Option<&StatusPayload>) {
    println!("{message}");
    if let Some(status) = status {
        println!("endpoint: {}", status.endpoint_id);
        println!("active: {}", status.active);
        println!("pointer_mode: {}", status.pointer_mode);
        println!("attached: {}", format_attached_sides(&status.attached));
        println!(
            "drops: captured_mouse={} captured_non_mouse={} writer_dropped={} writer_forced_local={}",
            status.captured_queue_full_mouse_dropped,
            status.captured_queue_full_non_mouse_dropped,
            status.writer_queue_full_dropped,
            status.writer_queue_full_forced_local,
        );
        println!(
            "events: captured={} normalized={} replay_failures={} tap_user_disabled={} recoveries={}",
            status.captured_events,
            status.normalized_events,
            status.replay_failures,
            status.capture_tap_user_disabled,
            status.recovery_events,
        );
    }
}

fn format_attached_sides(attached: &[Side]) -> String {
    if attached.is_empty() {
        "none".to_string()
    } else {
        attached
            .iter()
            .map(|s| format!("{s:?}").to_lowercase())
            .collect::<Vec<_>>()
            .join(", ")
    }
}
