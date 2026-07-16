use anyhow::Result;

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

        if !accessibility_granted(false) {
            let _ = accessibility_granted(true);
            missing.push("Accessibility");
        }

        if !input_monitoring_granted(false) {
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
            "grant access for the app launching meow (Ghostty/Terminal/iTerm/Warp) or the meow binary, then re-run `meow host`."
        );
        eprintln!(
            "if Input Monitoring is empty, add and enable the app launching meow (or the meow binary) first."
        );
        bail!("missing macOS permissions: {}", missing.join(", "))
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

    pub(crate) fn ensure_host_permissions_on_startup() -> Result<()> {
        Ok(())
    }
}

pub(crate) fn ensure_host_permissions_on_startup() -> Result<()> {
    imp::ensure_host_permissions_on_startup()
}
