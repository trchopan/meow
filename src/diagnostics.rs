use std::{
    fs,
    path::{Path, PathBuf},
    process::Command as StdCommand,
};

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    doctor::{DoctorReport, format_doctor_markdown, format_doctor_terminal, run_doctor_checks},
    logging::{read_file_tail, redact_sensitive_text},
    state::{crash_log_path, diagnostics_dir, log_file_path},
};

const MAX_LOG_BUNDLE_BYTES: u64 = 500 * 1024; // 500 KB tail

pub async fn export_diagnostic_bundle() -> Result<PathBuf> {
    let diag_dir = diagnostics_dir()?;
    let timestamp = get_timestamp();
    let staging_name = format!("meow-diagnostics-{timestamp}");
    let staging_path = diag_dir.join(&staging_name);
    fs::create_dir_all(&staging_path)?;

    let zip_path = diag_dir.join(format!("{staging_name}.zip"));

    // 1. Run Doctor Checks
    let report = run_doctor_checks().await;
    let json_report = serde_json::to_string_pretty(&report)?;
    fs::write(
        staging_path.join("doctor_report.json"),
        redact_sensitive_text(&json_report),
    )?;
    fs::write(
        staging_path.join("doctor_report.txt"),
        format_doctor_terminal(&report),
    )?;

    // 2. Collect System Info
    let sys_info = collect_system_info();
    fs::write(staging_path.join("system_info.txt"), sys_info)?;

    // 3. Tail active logs (redacted)
    let log_path = log_file_path()?;
    if log_path.exists() {
        let log_content = read_file_tail(&log_path, MAX_LOG_BUNDLE_BYTES)?;
        fs::write(staging_path.join("meow.log"), log_content)?;
    }

    // 4. Copy crash logs if present (redacted)
    let crash_path = crash_log_path()?;
    if crash_path.exists() {
        let crash_content = read_file_tail(&crash_path, MAX_LOG_BUNDLE_BYTES)?;
        fs::write(staging_path.join("crash.log"), crash_content)?;
    }

    // 5. Create ZIP archive using macOS system /usr/bin/zip
    let zip_status = StdCommand::new("/usr/bin/zip")
        .args(["-r", "-q"])
        .arg(&zip_path)
        .arg(".")
        .current_dir(&staging_path)
        .status()
        .context("failed to execute /usr/bin/zip to create diagnostics bundle")?;

    // Cleanup staging directory
    let _ = fs::remove_dir_all(&staging_path);

    if !zip_status.success() {
        bail!("failed to create zip archive for diagnostics bundle");
    }

    tracing::info!("exported diagnostics bundle to {}", zip_path.display());
    Ok(zip_path)
}

fn collect_system_info() -> String {
    let mut out = String::new();
    out.push_str("=== MEOW SYSTEM PROFILE ===\n");

    let os_vers = StdCommand::new("/usr/bin/sw_vers").output();
    if let Ok(o) = os_vers {
        out.push_str(&String::from_utf8_lossy(&o.stdout));
    }

    let model = StdCommand::new("/usr/sbin/sysctl")
        .args(["-n", "hw.model"])
        .output();
    if let Ok(o) = model {
        out.push_str(&format!(
            "Hardware Model: {}\n",
            String::from_utf8_lossy(&o.stdout).trim()
        ));
    }

    let uname = StdCommand::new("/usr/bin/uname").arg("-a").output();
    if let Ok(o) = uname {
        out.push_str(&format!(
            "Kernel: {}\n",
            String::from_utf8_lossy(&o.stdout).trim()
        ));
    }

    out
}

pub fn generate_github_issue_body(report: &DoctorReport, zip_path: &Path) -> String {
    let mut md = String::new();
    md.push_str("<!-- Thank you for reporting an issue with Meow! -->\n\n");
    md.push_str("### Problem Description\n");
    md.push_str(
        "<!-- Please describe what happened, expected behavior, and reproduction steps -->\n\n",
    );

    md.push_str("### Environment & Diagnostics Summary\n");
    md.push_str(&format_doctor_markdown(report));
    md.push('\n');
    md.push_str(&format!(
        "📎 **Diagnostic bundle exported to**: `{}`\n*(Please attach this zip file to this issue if possible - all credentials and keys have been redacted)*\n",
        zip_path.display()
    ));

    md
}

/// Reveal the target file or directory in macOS Finder.
pub fn open_in_finder(path: &Path) -> Result<()> {
    let status = StdCommand::new("/usr/bin/open")
        .args(["-R"])
        .arg(path)
        .status()
        .map_err(|e| anyhow!("failed to open Finder: {e}"))?;

    if !status.success() {
        bail!("failed to reveal path in Finder: {}", path.display());
    }
    Ok(())
}

fn get_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{now}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_issue_body_contains_doctor_markdown_and_zip_path() {
        let report = DoctorReport {
            timestamp: "123".to_string(),
            os_version: "15.0".to_string(),
            arch: "aarch64".to_string(),
            executable_path: "/test".to_string(),
            bundle_target: "/test.app".to_string(),
            checks: vec![],
            overall: crate::doctor::CheckLevel::Pass,
            daemon_status: None,
        };
        let zip = Path::new("/tmp/test.zip");
        let body = generate_github_issue_body(&report, zip);
        assert!(body.contains("Diagnostic bundle exported to"));
        assert!(body.contains("/tmp/test.zip"));
        assert!(body.contains("Problem Description"));
    }
}
