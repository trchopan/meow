use std::path::{Path, PathBuf};
use std::{fs::File, io::Write};

use anyhow::{Context, Result, bail};

use crate::protocol::FILE_CHUNK_SIZE;

pub(crate) const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

pub(crate) fn format_size(size: u64) -> String {
    if size >= 1024 * 1024 {
        format!("{:.1} MiB", size as f64 / (1024.0 * 1024.0))
    } else if size >= 1024 {
        format!("{:.1} KiB", size as f64 / 1024.0)
    } else {
        format!("{size} bytes")
    }
}

pub(crate) fn sanitize_filename(name: &str) -> String {
    let name = Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("meow-file");
    let name = name
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '/' | '\\') {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>();
    if name.is_empty() || name == "." || name == ".." {
        "meow-file".to_string()
    } else {
        name
    }
}

pub(crate) fn collision_path(directory: &Path, filename: &str) -> PathBuf {
    let filename = sanitize_filename(filename);
    let path = directory.join(&filename);
    if !path.exists() {
        return path;
    }

    let path = Path::new(&filename);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("meow-file");
    let extension = path.extension().and_then(|value| value.to_str());
    for index in 1.. {
        let candidate = match extension {
            Some(extension) => format!("{stem}-{index}.{extension}"),
            None => format!("{stem}-{index}"),
        };
        let candidate = directory.join(candidate);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

pub(crate) struct IncomingFile {
    temporary_path: PathBuf,
    temporary: File,
    directory: PathBuf,
    filename: String,
    expected_size: u64,
    expected_digest: [u8; 32],
    received: u64,
    hasher: blake3::Hasher,
}

impl IncomingFile {
    pub(crate) fn new(filename: &str, size: u64, digest: [u8; 32]) -> Result<Self> {
        if size > MAX_FILE_SIZE {
            bail!(
                "file exceeds the {} MiB limit",
                MAX_FILE_SIZE / (1024 * 1024)
            );
        }
        let home = dirs::home_dir().context("home directory is unavailable")?;
        let directory = home.join("Downloads").join("meow");
        std::fs::create_dir_all(&directory).context("failed to create ~/Downloads/meow")?;
        let (temporary_path, temporary) = create_temporary_file(&directory, filename)?;
        Ok(Self {
            temporary_path,
            temporary,
            directory,
            filename: sanitize_filename(filename),
            expected_size: size,
            expected_digest: digest,
            received: 0,
            hasher: blake3::Hasher::new(),
        })
    }

    pub(crate) fn write_chunk(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let next = validate_chunk(self.received, self.expected_size, offset, data.len())?;
        self.temporary.write_all(data)?;
        self.hasher.update(data);
        self.received = next;
        Ok(())
    }

    pub(crate) fn finish(mut self, digest: [u8; 32]) -> Result<PathBuf> {
        if self.received != self.expected_size
            || digest != self.expected_digest
            || self.hasher.finalize().as_bytes() != &digest
        {
            bail!("clipboard file transfer failed integrity checks");
        }
        self.temporary.flush()?;
        self.temporary.sync_all()?;
        loop {
            let destination = collision_path(&self.directory, &self.filename);
            match std::fs::hard_link(&self.temporary_path, &destination) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&self.temporary_path);
                    return Ok(destination);
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err).context("failed to finalize transferred file"),
            }
        }
    }
}

fn validate_chunk(received: u64, expected_size: u64, offset: u64, length: usize) -> Result<u64> {
    if length > FILE_CHUNK_SIZE || offset != received {
        bail!("invalid clipboard file chunk");
    }
    let next = received.saturating_add(length as u64);
    if next > expected_size {
        bail!("clipboard file exceeds offered size");
    }
    Ok(next)
}

impl Drop for IncomingFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temporary_path);
    }
}

fn create_temporary_file(directory: &Path, filename: &str) -> Result<(PathBuf, File)> {
    for index in 0..1000 {
        let path = directory.join(format!(
            ".{}.part-{}-{index}",
            sanitize_filename(filename),
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err).context("failed to create temporary file"),
        }
    }
    bail!("failed to find an unused temporary filename")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_paths_and_control_characters() {
        assert_eq!(sanitize_filename("../../hello.txt"), "hello.txt");
        assert_eq!(sanitize_filename("bad\nname.txt"), "bad_name.txt");
        assert_eq!(sanitize_filename(".."), "meow-file");
    }

    #[test]
    fn preserves_extension_when_renaming() {
        let directory = tempfile_dir();
        std::fs::write(directory.join("hello.txt"), b"existing").unwrap();
        assert_eq!(
            collision_path(&directory, "hello.txt"),
            directory.join("hello-1.txt")
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn chunk_validation_requires_order_and_offered_size() {
        assert!(validate_chunk(0, 10, 0, 4).is_ok());
        assert!(validate_chunk(4, 10, 0, 4).is_err());
        assert!(validate_chunk(8, 10, 8, 4).is_err());
        assert!(validate_chunk(0, 10, 0, FILE_CHUNK_SIZE + 1).is_err());
    }

    fn tempfile_dir() -> PathBuf {
        let directory = std::env::temp_dir().join(format!("meow-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }
}
