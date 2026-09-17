use std::{path::Path, process::Command as StdCommand};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    display::display_layout,
    ipc::{IpcCommand, StatusPayload, request_ipc},
    macos_permissions,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckLevel {
    Pass,
    Warn,
    Fail,
}

impl CheckLevel {
    pub fn icon(&self) -> &'static str {
        match self {
            CheckLevel::Pass => "✅",
            CheckLevel::Warn => "⚠️",
            CheckLevel::Fail => "❌",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub name: String,
    pub level: CheckLevel,
    pub detail: String,
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub timestamp: String,
    pub os_version: String,
    pub arch: String,
    pub executable_path: String,
    pub bundle_target: String,
    pub checks: Vec<DoctorCheck>,
    pub overall: CheckLevel,
    pub(crate) daemon_status: Option<StatusPayload>,
}

pub async fn run_doctor_checks() -> DoctorReport {
    let mut checks = Vec::new();

    // 1. OS & Architecture Check
    let os_version = get_macos_version();
    let arch = std::env::consts::ARCH.to_string();
    checks.push(DoctorCheck {
        name: "Operating System".to_string(),
        level: CheckLevel::Pass,
        detail: format!("macOS {os_version} ({arch})"),
        remediation: None,
    });

    // 2. Binary / App Target
    let current_exe = std::env::current_exe().unwrap_or_default();
    let target = macos_permissions::permission_target_for_executable(&current_exe);
    let is_bundle = target.extension().is_some_and(|ext| ext == "app");

    checks.push(DoctorCheck {
        name: "Execution Target".to_string(),
        level: CheckLevel::Pass,
        detail: format!(
            "Binary: {} | Target: {} (is_bundle={is_bundle})",
            current_exe.display(),
            target.display()
        ),
        remediation: None,
    });

    // 3. Code Signing Check
    let (cs_level, cs_detail, cs_rem) = check_code_signature(&target);
    checks.push(DoctorCheck {
        name: "Code Signature".to_string(),
        level: cs_level,
        detail: cs_detail,
        remediation: cs_rem,
    });

    // 4. Permissions Check
    let host_perms = macos_permissions::check_host_permissions();
    let client_perms = macos_permissions::check_client_permissions();

    let perm_level = if host_perms.accessibility && host_perms.input_monitoring {
        CheckLevel::Pass
    } else if host_perms.accessibility || host_perms.input_monitoring {
        CheckLevel::Warn
    } else {
        CheckLevel::Fail
    };

    let missing = host_perms.missing_host_permissions();
    let perm_detail = if missing.is_empty() {
        "Accessibility and Input Monitoring are granted".to_string()
    } else {
        format!("Missing host permissions: {}", missing.join(", "))
    };

    let perm_remediation = if !missing.is_empty() {
        Some(format!(
            "Open System Settings -> Privacy & Security, and grant {} to {}.\nIf permissions are stuck, try:\n  tccutil reset Accessibility com.meow.inputsharing\n  tccutil reset ListenEvent com.meow.inputsharing",
            missing.join(" and "),
            target.display()
        ))
    } else {
        None
    };

    checks.push(DoctorCheck {
        name: "TCC Permissions (Host)".to_string(),
        level: perm_level,
        detail: perm_detail,
        remediation: perm_remediation,
    });

    checks.push(DoctorCheck {
        name: "TCC Permissions (Client)".to_string(),
        level: if client_perms.accessibility {
            CheckLevel::Pass
        } else {
            CheckLevel::Warn
        },
        detail: if client_perms.accessibility {
            "Accessibility is granted for client injection".to_string()
        } else {
            "Accessibility missing for client injection".to_string()
        },
        remediation: (!client_perms.accessibility).then(|| {
            format!(
                "Open System Settings -> Privacy & Security -> Accessibility, and grant access to {}.",
                target.display()
            )
        }),
    });

    // 5. Transient CGEventTap Check (Crucial for verifying if WindowServer actually allows taps)
    let (tap_level, tap_detail, tap_rem) = test_event_tap_viability(&target);
    checks.push(DoctorCheck {
        name: "CGEventTap Viability".to_string(),
        level: tap_level,
        detail: tap_detail,
        remediation: tap_rem,
    });

    // 6. Displays and Layout Check
    match display_layout() {
        Ok(layout) => {
            let count = layout.displays.len();
            let summary = layout
                .displays
                .iter()
                .map(|d| {
                    format!(
                        "[{:.0},{:.0} {:.0}x{:.0}]",
                        d.origin_x, d.origin_y, d.width, d.height
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            checks.push(DoctorCheck {
                name: "Display Topology".to_string(),
                level: if count > 0 {
                    CheckLevel::Pass
                } else {
                    CheckLevel::Warn
                },
                detail: format!(
                    "{count} display(s) detected: {summary} (main: {:.0}x{:.0})",
                    layout.main.width, layout.main.height
                ),
                remediation: None,
            });
        }
        Err(err) => {
            checks.push(DoctorCheck {
                name: "Display Topology".to_string(),
                level: CheckLevel::Warn,
                detail: format!("Failed to enumerate displays: {err}"),
                remediation: None,
            });
        }
    }

    // 7. IPC Host Daemon Check
    let mut daemon_status = None;
    match request_ipc(IpcCommand::Status).await {
        Ok(res) if res.ok => {
            if let Some(status) = res.status {
                let degraded = !status.pointer_tap_healthy || status.capture_tap_stopped > 0;
                let level = if degraded {
                    CheckLevel::Warn
                } else {
                    CheckLevel::Pass
                };
                let detail = format!(
                    "Daemon running: target={}, peers={}, captured={}, pointer_tap_healthy={}",
                    status.active,
                    status.attached_peers.len(),
                    status.captured_events,
                    status.pointer_tap_healthy
                );
                daemon_status = Some(status);
                checks.push(DoctorCheck {
                    name: "Host Daemon IPC".to_string(),
                    level,
                    detail,
                    remediation: if degraded {
                        Some("Pointer tap health is degraded. Restart host daemon or check CGEventTap.".to_string())
                    } else {
                        None
                    },
                });
            } else {
                checks.push(DoctorCheck {
                    name: "Host Daemon IPC".to_string(),
                    level: CheckLevel::Pass,
                    detail: "Host daemon is running (no detailed payload returned)".to_string(),
                    remediation: None,
                });
            }
        }
        _ => {
            checks.push(DoctorCheck {
                name: "Host Daemon IPC".to_string(),
                level: CheckLevel::Pass,
                detail: "Host daemon is not currently running (can be started on demand)"
                    .to_string(),
                remediation: None,
            });
        }
    }

    // 8. Iroh Network Stack Test
    match test_iroh_network_bind().await {
        Ok(node_id) => {
            checks.push(DoctorCheck {
                name: "Iroh Network Stack".to_string(),
                level: CheckLevel::Pass,
                detail: format!("Local Iroh endpoint successfully bound (node: {node_id})"),
                remediation: None,
            });
        }
        Err(err) => {
            checks.push(DoctorCheck {
                name: "Iroh Network Stack".to_string(),
                level: CheckLevel::Fail,
                detail: format!("Failed to initialize Iroh network endpoint: {err}"),
                remediation: Some(
                    "Ensure network interface is active and no firewall blocks UDP.".to_string(),
                ),
            });
        }
    }

    // Determine overall status
    let overall = if checks.iter().any(|c| c.level == CheckLevel::Fail) {
        CheckLevel::Fail
    } else if checks.iter().any(|c| c.level == CheckLevel::Warn) {
        CheckLevel::Warn
    } else {
        CheckLevel::Pass
    };

    DoctorReport {
        timestamp: get_timestamp(),
        os_version,
        arch,
        executable_path: current_exe.display().to_string(),
        bundle_target: target.display().to_string(),
        checks,
        overall,
        daemon_status,
    }
}

fn get_macos_version() -> String {
    let output = StdCommand::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output();
    output
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "Unknown".to_string())
}

fn check_code_signature(target: &Path) -> (CheckLevel, String, Option<String>) {
    let output = StdCommand::new("/usr/bin/codesign")
        .args(["-v", "--display", "--verbose=1"])
        .arg(target)
        .output();

    match output {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if out.status.success() || stderr.contains("Signature=") {
                let id_line = stderr
                    .lines()
                    .find(|l| l.contains("Identifier="))
                    .unwrap_or("Identifier=unknown");
                (
                    CheckLevel::Pass,
                    format!("Valid code signature ({id_line})"),
                    None,
                )
            } else {
                (
                    CheckLevel::Warn,
                    "Target is unsigned or has an ad-hoc signature without stable designated requirements".to_string(),
                    Some(format!("Run: codesign --force --deep -s - \"{}\"", target.display())),
                )
            }
        }
        Err(err) => (
            CheckLevel::Warn,
            format!("Unable to verify code signature: {err}"),
            None,
        ),
    }
}

fn test_event_tap_viability(target: &Path) -> (CheckLevel, String, Option<String>) {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::event::{
            CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
            CGEventType,
        };

        let tap_res = std::panic::catch_unwind(|| {
            CGEventTap::new(
                CGEventTapLocation::HID,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                vec![CGEventType::MouseMoved],
                |_proxy, _type, _event: &CGEvent| None,
            )
        });

        match tap_res {
            Ok(Ok(_tap)) => (
                CheckLevel::Pass,
                "Transient CGEventTap created successfully (WindowServer authorized)".to_string(),
                None,
            ),
            Ok(Err(err)) => (
                CheckLevel::Fail,
                format!(
                    "CGEventTap creation failed ({err:?}). macOS TCC permissions may be desynced."
                ),
                Some(format!(
                    "Even if System Settings shows enabled, macOS may have invalidated the tap signature.\nRun in Terminal:\n  tccutil reset Accessibility com.meow.inputsharing\n  tccutil reset ListenEvent com.meow.inputsharing\nThen relaunch {}",
                    target.display()
                )),
            ),
            Err(_) => (
                CheckLevel::Fail,
                "CGEventTap creation panicked or crashed".to_string(),
                Some(
                    "Check macOS Console.app for crash logs related to WindowServer or Meow."
                        .to_string(),
                ),
            ),
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        (
            CheckLevel::Pass,
            "Not on macOS; CGEventTap check skipped".to_string(),
            None,
        )
    }
}

async fn test_iroh_network_bind() -> Result<String> {
    let secret = iroh::SecretKey::generate();
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(secret)
        .bind()
        .await?;
    let node_id = endpoint.id().to_string();
    endpoint.close().await;
    Ok(node_id[..8].to_string())
}

fn get_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{now}")
}

pub fn format_doctor_terminal(report: &DoctorReport) -> String {
    let mut out = String::new();
    out.push_str("==============================================\n");
    out.push_str(&format!(
        "  MEOW DOCTOR REPORT (Status: {} {:?})\n",
        report.overall.icon(),
        report.overall
    ));
    out.push_str("==============================================\n\n");
    out.push_str(&format!(
        "OS: macOS {} ({})\n",
        report.os_version, report.arch
    ));
    out.push_str(&format!("Target: {}\n\n", report.bundle_target));

    out.push_str("Checks:\n");
    for check in &report.checks {
        out.push_str(&format!(
            "  {} {}: {}\n",
            check.level.icon(),
            check.name,
            check.detail
        ));
        if let Some(rem) = &check.remediation {
            for line in rem.lines() {
                out.push_str(&format!("     💡 {line}\n"));
            }
        }
    }

    out.push('\n');
    if report.overall == CheckLevel::Pass {
        out.push_str("All checks passed! Meow is ready to share input.\n");
    } else {
        out.push_str(
            "Some issues were identified above. Follow the recommendations to resolve them.\n",
        );
    }

    out
}

pub fn format_doctor_markdown(report: &DoctorReport) -> String {
    let mut md = String::new();
    md.push_str("### Meow Doctor Diagnostic Summary\n\n");
    md.push_str(&format!(
        "- **Overall Status**: {} `{:?}`\n",
        report.overall.icon(),
        report.overall
    ));
    md.push_str(&format!(
        "- **OS**: macOS {} (`{}`)\n",
        report.os_version, report.arch
    ));
    md.push_str(&format!("- **Target**: `{}`\n\n", report.bundle_target));

    md.push_str("| Check | Status | Details |\n");
    md.push_str("| :--- | :---: | :--- |\n");
    for check in &report.checks {
        md.push_str(&format!(
            "| {} | {} | {} |\n",
            check.name,
            check.level.icon(),
            check.detail
        ));
    }

    let remediations: Vec<&String> = report
        .checks
        .iter()
        .filter_map(|c| c.remediation.as_ref())
        .collect();
    if !remediations.is_empty() {
        md.push_str("\n<details><summary><b>Recommended Remediation Actions</b></summary>\n\n");
        for rem in remediations {
            md.push_str(&format!("- {}\n", rem.replace('\n', "\n  ")));
        }
        md.push_str("\n</details>\n");
    }

    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_report_formats_terminal_output() {
        let report = DoctorReport {
            timestamp: "12345".to_string(),
            os_version: "15.0".to_string(),
            arch: "aarch64".to_string(),
            executable_path: "/test/meow".to_string(),
            bundle_target: "/test/Meow.app".to_string(),
            checks: vec![DoctorCheck {
                name: "Test".to_string(),
                level: CheckLevel::Pass,
                detail: "ok".to_string(),
                remediation: None,
            }],
            overall: CheckLevel::Pass,
            daemon_status: None,
        };

        let term = format_doctor_terminal(&report);
        assert!(term.contains("MEOW DOCTOR REPORT"));
        assert!(term.contains("✅ Test: ok"));

        let md = format_doctor_markdown(&report);
        assert!(md.contains("Meow Doctor Diagnostic Summary"));
        assert!(md.contains("| Test | ✅ | ok |"));
    }
}
