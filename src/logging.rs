use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::state::{crash_log_path, log_file_path};

const DEFAULT_MAX_LOG_BYTES: u64 = 5 * 1024 * 1024; // 5 MB
const DEFAULT_MAX_BACKUP_FILES: usize = 3;

/// Redacts sensitive credential patterns from log strings and diagnostic outputs.
pub fn redact_sensitive_text(text: &str) -> String {
    let mut result = text.to_string();

    // Redact "attach_secret": "..." or "secret": "..."
    let patterns = [
        ("attach_secret", "\"attach_secret\""),
        ("secret", "\"secret\""),
        ("secret_key", "\"secret_key\""),
    ];

    for (key, json_key) in patterns {
        // Redact JSON patterns
        let search = format!("{json_key}:");
        let mut start_idx = 0;
        while let Some(found) = result[start_idx..].find(&search) {
            let abs_found = start_idx + found;
            let after = abs_found + search.len();
            // Find opening quote of value
            if let Some(quote_start) = result[after..].find('"') {
                let val_start = after + quote_start + 1;
                if let Some(quote_end) = result[val_start..].find('"') {
                    let val_end = val_start + quote_end;
                    result.replace_range(val_start..val_end, "[REDACTED]");
                    start_idx = val_start + 10;
                    continue;
                }
            }
            start_idx = after;
        }

        // Redact key-value CLI/argument patterns: key=val or --secret <val>
        let cli_search = format!("{key}=");
        let mut start_idx = 0;
        while let Some(found) = result[start_idx..].find(&cli_search) {
            let val_start = start_idx + found + cli_search.len();
            let val_len = result[val_start..]
                .find(|c: char| c.is_whitespace() || c == ',' || c == '&' || c == '"')
                .unwrap_or(result[val_start..].len());
            let val_end = val_start + val_len;
            if val_len > 0 {
                result.replace_range(val_start..val_end, "[REDACTED]");
            }
            start_idx = val_start + 10;
        }
    }

    result
}

/// A thread-safe rolling file writer that limits file size and keeps historical backups.
pub struct RollingFileWriter {
    path: PathBuf,
    max_bytes: u64,
    max_backups: usize,
    file: Option<File>,
    current_size: u64,
}

impl RollingFileWriter {
    pub fn new(path: PathBuf, max_bytes: u64, max_backups: usize) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let (file, current_size) = Self::open_or_create(&path)?;

        Ok(Self {
            path,
            max_bytes,
            max_backups,
            file: Some(file),
            current_size,
        })
    }

    fn open_or_create(path: &Path) -> Result<(File, u64)> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open log file at {}", path.display()))?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok((file, size))
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;

        for i in (1..self.max_backups).rev() {
            let src = backup_path(&self.path, i);
            let dst = backup_path(&self.path, i + 1);
            if src.exists() {
                let _ = std::fs::rename(&src, &dst);
            }
        }

        let first_backup = backup_path(&self.path, 1);
        if self.path.exists() {
            let _ = std::fs::rename(&self.path, &first_backup);
        }

        match Self::open_or_create(&self.path) {
            Ok((file, size)) => {
                self.file = Some(file);
                self.current_size = size;
                Ok(())
            }
            Err(err) => Err(io::Error::other(err.to_string())),
        }
    }
}

fn backup_path(base: &Path, index: usize) -> PathBuf {
    PathBuf::from(format!("{}.{}", base.display(), index))
}

impl io::Write for RollingFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.current_size + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }

        let file = match self.file.as_mut() {
            Some(f) => f,
            None => {
                let (f, size) = Self::open_or_create(&self.path)
                    .map_err(|e| io::Error::other(e.to_string()))?;
                self.current_size = size;
                self.file = Some(f);
                self.file.as_mut().unwrap()
            }
        };

        let written = file.write(buf)?;
        self.current_size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct SharedRollingAppender(Arc<Mutex<RollingFileWriter>>);

impl io::Write for SharedRollingAppender {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?
            .flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedRollingAppender {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Initialize persistent structured logging for CLI or Menu Bar.
pub fn init_logging(role: &str, is_gui: bool) -> Result<()> {
    let filter_str =
        std::env::var("RUST_LOG").unwrap_or_else(|_| "meow=info,iroh=warn".to_string());

    let env_filter = EnvFilter::try_new(&filter_str)
        .or_else(|_| EnvFilter::try_new("meow=info,iroh=warn"))
        .unwrap_or_default();

    let log_path = log_file_path()?;
    let writer = RollingFileWriter::new(log_path, DEFAULT_MAX_LOG_BYTES, DEFAULT_MAX_BACKUP_FILES)?;
    let appender = SharedRollingAppender(Arc::new(Mutex::new(writer)));

    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_writer(appender);

    let registry = tracing_subscriber::registry().with(env_filter);

    if is_gui {
        registry.with(file_layer).try_init().ok();
    } else {
        let stderr_layer = fmt::layer().with_target(true).with_writer(io::stderr);
        registry.with(file_layer).with(stderr_layer).try_init().ok();
    }

    tracing::info!("initialized logging for {role} (is_gui={is_gui})");
    Ok(())
}

/// Read recent lines from the active log file.
pub fn read_recent_logs(max_lines: usize) -> Result<Vec<String>> {
    let path = log_file_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = File::open(&path)?;
    let reader = BufReader::new(file);
    let mut lines = Vec::new();

    for line in reader.lines().map_while(Result::ok) {
        lines.push(redact_sensitive_text(&line));
        if lines.len() > max_lines * 2 {
            lines.drain(0..max_lines);
        }
    }

    if lines.len() > max_lines {
        let skip = lines.len() - max_lines;
        Ok(lines.into_iter().skip(skip).collect())
    } else {
        Ok(lines)
    }
}

/// Read the tail of a file up to max_bytes.
pub fn read_file_tail(path: &Path, max_bytes: u64) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }

    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len > max_bytes {
        file.seek(SeekFrom::Start(len - max_bytes))?;
    }

    let mut reader = BufReader::new(file);
    let mut content = String::new();
    // discard potential partial first line if we seeked
    if len > max_bytes {
        let mut _partial = String::new();
        let _ = reader.read_line(&mut _partial);
    }
    reader.read_to_string(&mut content)?;
    Ok(redact_sensitive_text(&content))
}

/// Install global panic hook to capture crash stack traces.
pub fn install_panic_hook(is_gui: bool) {
    let default_hook = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |panic_info| {
        let backtrace = std::backtrace::Backtrace::capture();
        let location = panic_info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        let payload = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Box<dyn Any>".to_string()
        };

        let timestamp = chrono_fallback_timestamp();
        let crash_entry = format!(
            "=== MEOW CRASH REPORT ===\nTimestamp: {}\nLocation: {}\nMessage: {}\nBacktrace:\n{}\n=========================\n\n",
            timestamp,
            location,
            redact_sensitive_text(&payload),
            backtrace
        );

        if let Ok(path) = crash_log_path()
            && let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path)
        {
            let _ = file.write_all(crash_entry.as_bytes());
        }

        eprintln!("{crash_entry}");

        #[cfg(target_os = "macos")]
        if is_gui {
            notify_gui_crash(&location, &payload);
        }

        default_hook(panic_info);
    }));
}

fn chrono_fallback_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{now} (unix timestamp)")
}

#[cfg(target_os = "macos")]
fn notify_gui_crash(location: &str, message: &str) {
    use cocoa::base::{id, nil};
    use cocoa::foundation::NSString;
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let alert: id = msg_send![class!(NSAlert), alloc];
        let alert: id = msg_send![alert, init];
        let title = NSString::alloc(nil).init_str("Meow Unexpected Crash");
        let info = NSString::alloc(nil).init_str(&format!(
            "Meow encountered an unexpected crash.\nLocation: {}\nError: {}\n\nA crash report has been saved to ~/.local/share/meow/logs/crash.log",
            location, message
        ));
        let _: () = msg_send![alert, setMessageText:title];
        let _: () = msg_send![alert, setInformativeText:info];
        let button = NSString::alloc(nil).init_str("Quit");
        let _: id = msg_send![alert, addButtonWithTitle:button];
        let _: i64 = msg_send![alert, runModal];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_sensitive_json_fields() {
        let raw = r#"{"schema_version":1,"endpoint_id":"123","attach_secret":"secret12345","other":"ok"}"#;
        let sanitized = redact_sensitive_text(raw);
        assert!(!sanitized.contains("secret12345"));
        assert!(sanitized.contains("[REDACTED]"));
    }

    #[test]
    fn redact_sensitive_cli_args() {
        let raw = "run attach host123 secret=supersecretpassword --side right";
        let sanitized = redact_sensitive_text(raw);
        assert!(!sanitized.contains("supersecretpassword"));
        assert!(sanitized.contains("secret=[REDACTED]"));
    }

    #[test]
    fn rolling_file_writer_rotates_on_size_limit() {
        let temp_dir = std::env::temp_dir().join(format!("meow-test-log-{}", uuid::Uuid::new_v4()));
        let log_file = temp_dir.join("test.log");

        let mut writer = RollingFileWriter::new(log_file.clone(), 50, 3).expect("writer init");
        writer
            .write_all(b"123456789012345678901234567890")
            .expect("write 1"); // 30 bytes
        assert!(log_file.exists());

        // This next write pushes it over 50 bytes and triggers rotation
        writer
            .write_all(b"123456789012345678901234567890")
            .expect("write 2"); // 30 bytes

        let backup1 = backup_path(&log_file, 1);
        assert!(backup1.exists(), "backup .1 should exist after rotation");

        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
