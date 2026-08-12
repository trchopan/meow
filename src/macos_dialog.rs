use anyhow::Result;

#[cfg(target_os = "macos")]
pub(crate) fn confirm_file_transfer(name: &str, size: u64) -> Result<bool> {
    use std::process::Command;

    let message = format!(
        "Allow meow to copy '{name}' ({}) to ~/Downloads/meow/?",
        crate::file_transfer::format_size(size)
    );
    let escape = |value: &str| {
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace(['\n', '\r'], " ")
    };
    let script = format!(
        "with timeout of 60 seconds\n display dialog \"{}\" with title \"meow file transfer\" buttons {{\"Cancel\", \"Allow\"}} default button \"Allow\" cancel button \"Cancel\"\nend timeout",
        escape(&message)
    );
    let status = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .status()?;
    Ok(status.success())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn confirm_file_transfer(_name: &str, _size: u64) -> Result<bool> {
    Ok(false)
}
