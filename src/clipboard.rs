use anyhow::{Result, bail};

use crate::file_transfer::MAX_FILE_SIZE;

const MAX_CLIPBOARD_TEXT_BYTES: usize = 900 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct ClipboardFile {
    pub(crate) path: std::path::PathBuf,
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) digest: [u8; 32],
}

pub(crate) fn read_file() -> Result<Option<ClipboardFile>> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/bin/pbpaste")
            .args(["-Prefer", "public.file-url"])
            .output()?;
        if !output.status.success() || output.stdout.is_empty() {
            return Ok(None);
        }
        let output_text = String::from_utf8_lossy(&output.stdout);
        let urls = output_text
            .lines()
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        if urls.len() > 1 {
            bail!("clipboard contains multiple files; only one file is supported");
        }
        let Some(url) = urls.first() else {
            return Ok(None);
        };
        let Some(path) = file_url_to_path(url) else {
            return Ok(None);
        };
        let metadata = std::fs::metadata(&path)?;
        if !metadata.is_file() {
            return Ok(None);
        }
        if metadata.len() > MAX_FILE_SIZE {
            bail!(
                "clipboard file exceeds the {} MiB limit",
                MAX_FILE_SIZE / (1024 * 1024)
            );
        }
        let digest = digest_file(&path)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("meow-file")
            .to_string();
        Ok(Some(ClipboardFile {
            path,
            name,
            size: metadata.len(),
            digest,
        }))
    }

    #[cfg(not(target_os = "macos"))]
    {
        Ok(None)
    }
}

pub(crate) fn read_file_chunk(file: &ClipboardFile, offset: u64) -> Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};

    if offset > file.size {
        bail!("clipboard file offset is out of bounds");
    }
    let mut source = std::fs::File::open(&file.path)?;
    source.seek(SeekFrom::Start(offset))?;
    let remaining = file.size - offset;
    let mut chunk = vec![0u8; remaining.min(crate::protocol::FILE_CHUNK_SIZE as u64) as usize];
    let read = source.read(&mut chunk)?;
    chunk.truncate(read);
    Ok(chunk)
}

fn digest_file(path: &std::path::Path) -> Result<[u8; 32]> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(*hasher.finalize().as_bytes())
}

#[cfg(target_os = "macos")]
fn file_url_to_path(url: &str) -> Option<std::path::PathBuf> {
    let value = url.strip_prefix("file://")?;
    let path = if let Some(path) = value.strip_prefix("localhost") {
        path
    } else if value.starts_with('/') {
        value
    } else {
        return None;
    };
    let mut bytes = Vec::with_capacity(path.len());
    let chars = path.as_bytes();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == b'%' && index + 2 < chars.len() {
            let high = (chars[index + 1] as char).to_digit(16)?;
            let low = (chars[index + 2] as char).to_digit(16)?;
            bytes.push((high * 16 + low) as u8);
            index += 3;
        } else {
            bytes.push(chars[index]);
            index += 1;
        }
    }
    let path = String::from_utf8(bytes).ok()?;
    let path = std::path::PathBuf::from(path);
    path.is_absolute().then_some(path)
}

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
