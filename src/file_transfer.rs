use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

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

pub(crate) struct DownloadTarget {
    temporary_path: PathBuf,
    directory: PathBuf,
    filename: String,
}

impl DownloadTarget {
    pub(crate) fn new(filename: &str) -> Result<Self> {
        let home = dirs::home_dir().context("home directory is unavailable")?;
        let directory = home.join("Downloads").join("meow");
        std::fs::create_dir_all(&directory).context("failed to create ~/Downloads/meow")?;
        let (temporary_path, _) = create_temporary_file(&directory, filename)?;
        Ok(Self {
            temporary_path,
            directory,
            filename: sanitize_filename(filename),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.temporary_path
    }

    pub(crate) fn finish(self) -> Result<PathBuf> {
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

impl Drop for DownloadTarget {
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

    fn tempfile_dir() -> PathBuf {
        let directory = std::env::temp_dir().join(format!("meow-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }
}
