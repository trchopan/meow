use anyhow::{Result, bail};

const MAX_CLIPBOARD_TEXT_BYTES: usize = 900 * 1024;

pub(crate) fn read_text() -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/bin/pbpaste")
            .output()
            .map_err(|err| anyhow::anyhow!("failed to read macOS clipboard: {err}"))?;
        if !output.status.success() {
            bail!("pbpaste exited with status {}", output.status);
        }
        if output.stdout.len() > MAX_CLIPBOARD_TEXT_BYTES {
            bail!("clipboard text exceeds {} bytes", MAX_CLIPBOARD_TEXT_BYTES);
        }
        String::from_utf8(output.stdout)
            .map_err(|err| anyhow::anyhow!("clipboard is not valid UTF-8: {err}"))
    }

    #[cfg(not(target_os = "macos"))]
    {
        bail!("clipboard transfer is only supported on macOS")
    }
}

pub(crate) fn write_text(text: &str) -> Result<()> {
    if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
        bail!("clipboard text exceeds {} bytes", MAX_CLIPBOARD_TEXT_BYTES);
    }

    #[cfg(target_os = "macos")]
    {
        use std::io::Write;
        let mut child = std::process::Command::new("/usr/bin/pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|err| anyhow::anyhow!("failed to write macOS clipboard: {err}"))?;
        child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("pbcopy stdin was unavailable"))?
            .write_all(text.as_bytes())?;
        let status = child.wait()?;
        if !status.success() {
            bail!("pbcopy exited with status {status}");
        }
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = text;
        bail!("clipboard transfer is only supported on macOS")
    }
}
