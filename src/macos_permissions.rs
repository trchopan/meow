use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PermissionStatus {
    pub(crate) accessibility: bool,
    pub(crate) input_monitoring: bool,
}

pub(crate) fn permission_target_for_executable(path: &Path) -> PathBuf {
    let Some(macos_dir) = path.parent() else {
        return path.to_path_buf();
    };
    if macos_dir.file_name().is_none_or(|name| name != "MacOS") {
        return path.to_path_buf();
    }
    let Some(contents_dir) = macos_dir.parent() else {
        return path.to_path_buf();
    };
    if contents_dir
        .file_name()
        .is_none_or(|name| name != "Contents")
    {
        return path.to_path_buf();
    }
    contents_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.to_path_buf())
}

impl PermissionStatus {
    pub(crate) fn missing_host_permissions(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.accessibility {
            missing.push("Accessibility");
        }
        if !self.input_monitoring {
            missing.push("Input Monitoring");
        }
        missing
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use anyhow::{Result, anyhow, bail};
    use core_foundation::{
        base::TCFType,
        boolean::CFBoolean,
        dictionary::{CFDictionary, CFDictionaryRef},
        string::CFString,
    };
    use std::{ffi::c_int, process::Command};

    use super::{PermissionStatus, permission_target_for_executable};

    type Boolean = c_int;
    type IOHIDRequestType = c_int;
    type IOHIDAccessType = c_int;

    const K_IO_HID_REQUEST_TYPE_LISTEN_EVENT: IOHIDRequestType = 1;
    const K_IO_HID_ACCESS_TYPE_GRANTED: IOHIDAccessType = 0;
    const K_IO_HID_ACCESS_TYPE_DENIED: IOHIDAccessType = 1;
    const K_IO_HID_ACCESS_TYPE_UNKNOWN: IOHIDAccessType = 2;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        static kAXTrustedCheckOptionPrompt: core_foundation::string::CFStringRef;
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> Boolean;
        fn IOHIDCheckAccess(request_type: IOHIDRequestType) -> IOHIDAccessType;
        fn IOHIDRequestAccess(request_type: IOHIDRequestType) -> bool;
    }

    pub(crate) fn ensure_host_permissions_on_startup() -> Result<()> {
        let mut missing = Vec::new();

        let status = check_host_permissions();
        if !status.accessibility {
            let _ = accessibility_granted(true);
            missing.push("Accessibility");
        }

        if !status.input_monitoring {
            let _ = input_monitoring_granted(true);
            missing.push("Input Monitoring");
        }

        if missing.is_empty() {
            return Ok(());
        }

        if missing.contains(&"Accessibility") {
            let _ = open_settings_url(
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
            );
        }
        if missing.contains(&"Input Monitoring") {
            let _ = open_settings_url(
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent",
            );
        }

        eprintln!("meow needs macOS permissions before host mode can run.");
        eprintln!("missing: {}", missing.join(", "));
        eprintln!(
            "grant {} to {}, then re-run `meow host`.",
            missing.join(" and "),
            std::env::current_exe()
                .map(|path| permission_target_for_executable(&path)
                    .display()
                    .to_string())
                .unwrap_or_else(|_| "the meow executable".to_string())
        );
        bail!("missing macOS permissions: {}", missing.join(", "))
    }

    pub(crate) fn check_host_permissions() -> PermissionStatus {
        PermissionStatus {
            accessibility: accessibility_granted(false),
            input_monitoring: input_monitoring_granted(false),
        }
    }

    pub(crate) fn check_client_permissions() -> PermissionStatus {
        PermissionStatus {
            accessibility: accessibility_granted(false),
            input_monitoring: true,
        }
    }

    pub(crate) fn open_host_system_settings() -> Result<()> {
        open_settings_url(
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
        )?;
        open_settings_url(
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent",
        )?;
        Ok(())
    }

    fn accessibility_granted(prompt: bool) -> bool {
        let prompt_value = if prompt {
            CFBoolean::true_value()
        } else {
            CFBoolean::false_value()
        };
        let option_key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
        let options = CFDictionary::from_CFType_pairs(&[(option_key, prompt_value)]);
        unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) != 0 }
    }

    fn input_monitoring_granted(prompt: bool) -> bool {
        let access = unsafe { IOHIDCheckAccess(K_IO_HID_REQUEST_TYPE_LISTEN_EVENT) };
        match access {
            K_IO_HID_ACCESS_TYPE_GRANTED => true,
            K_IO_HID_ACCESS_TYPE_UNKNOWN => {
                if prompt {
                    unsafe { IOHIDRequestAccess(K_IO_HID_REQUEST_TYPE_LISTEN_EVENT) }
                } else {
                    false
                }
            }
            K_IO_HID_ACCESS_TYPE_DENIED => {
                if prompt {
                    let _ = open_settings_url(
                        "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent",
                    );
                }
                false
            }
            _ => false,
        }
    }

    fn open_settings_url(url: &str) -> Result<()> {
        let status = Command::new("/usr/bin/open")
            .arg(url)
            .status()
            .map_err(|err| anyhow!("failed to launch System Settings: {err}"))?;
        if !status.success() {
            bail!("failed to open System Settings URL: {url}");
        }
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use anyhow::Result;

    use super::PermissionStatus;

    pub(crate) fn ensure_host_permissions_on_startup() -> Result<()> {
        Ok(())
    }

    pub(crate) fn check_host_permissions() -> PermissionStatus {
        PermissionStatus {
            accessibility: true,
            input_monitoring: true,
        }
    }

    pub(crate) fn check_client_permissions() -> PermissionStatus {
        check_host_permissions()
    }

    pub(crate) fn open_host_system_settings() -> Result<()> {
        Ok(())
    }
}

pub(crate) fn ensure_host_permissions_on_startup() -> Result<()> {
    imp::ensure_host_permissions_on_startup()
}

pub(crate) fn check_host_permissions() -> PermissionStatus {
    imp::check_host_permissions()
}

pub(crate) fn check_client_permissions() -> PermissionStatus {
    imp::check_client_permissions()
}

pub(crate) fn open_host_system_settings() -> Result<()> {
    imp::open_host_system_settings()
}

pub(crate) fn print_permission_status(role: &str) -> Result<()> {
    let status = match role {
        "host" => check_host_permissions(),
        "client" => check_client_permissions(),
        _ => bail!("unknown permission role"),
    };
    println!("{}", serde_json::to_string(&status)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{PermissionStatus, permission_target_for_executable};

    #[test]
    fn permission_target_maps_bundle_executables_to_the_app() {
        assert_eq!(
            permission_target_for_executable(std::path::Path::new(
                "/Applications/Meow.app/Contents/MacOS/meow",
            )),
            std::path::Path::new("/Applications/Meow.app")
        );
        assert_eq!(
            permission_target_for_executable(std::path::Path::new("/tmp/meow")),
            std::path::Path::new("/tmp/meow")
        );
    }

    #[test]
    fn missing_host_permissions_reports_each_missing_capability() {
        assert_eq!(
            (PermissionStatus {
                accessibility: false,
                input_monitoring: false,
            })
            .missing_host_permissions(),
            vec!["Accessibility", "Input Monitoring"]
        );
        assert_eq!(
            (PermissionStatus {
                accessibility: true,
                input_monitoring: false,
            })
            .missing_host_permissions(),
            vec!["Input Monitoring"]
        );
        assert!(
            (PermissionStatus {
                accessibility: true,
                input_monitoring: true,
            })
            .missing_host_permissions()
            .is_empty()
        );
    }
}
