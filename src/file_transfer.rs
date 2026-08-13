use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub(crate) const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

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

pub(crate) struct ReceiveTarget {
    temporary_path: PathBuf,
    destination: PathBuf,
}

impl ReceiveTarget {
    pub(crate) fn new(destination: &Path) -> Result<Self> {
        let destination = if destination.is_absolute() {
            destination.to_path_buf()
        } else {
            std::env::current_dir()?.join(destination)
        };
        let directory = destination
            .parent()
            .context("destination has no parent directory")?;
        std::fs::create_dir_all(directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;
        let filename = destination
            .file_name()
            .and_then(|name| name.to_str())
            .context("destination filename is not valid UTF-8")?;
        let (temporary_path, _) = create_temporary_file(directory, filename)?;
        Ok(Self {
            temporary_path,
            destination,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.temporary_path
    }

    pub(crate) fn finish(self) -> Result<PathBuf> {
        match std::fs::hard_link(&self.temporary_path, &self.destination) {
            Ok(()) => std::fs::remove_file(&self.temporary_path),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("destination already exists: {}", self.destination.display())
            }
            Err(err) => Err(err),
        }
        .with_context(|| format!("failed to finalize {}", self.destination.display()))?;
        Ok(self.destination.clone())
    }
}

impl Drop for ReceiveTarget {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temporary_path);
    }
}

fn create_temporary_file(directory: &Path, filename: &str) -> Result<(PathBuf, File)> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    for index in 0..1000 {
        let path = directory.join(format!(
            ".{}.part-{}-{index}",
            sanitize_filename(filename),
            std::process::id()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path) {
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
    fn refuses_existing_destination_without_replacing_it() {
        let directory = std::env::temp_dir().join(format!("meow-transfer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let destination = directory.join("file.txt");
        std::fs::write(&destination, b"original").unwrap();
        let target = ReceiveTarget::new(&destination).unwrap();
        std::fs::write(target.path(), b"replacement").unwrap();
        assert!(target.finish().is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"original");
        let _ = std::fs::remove_dir_all(directory);
    }
}
