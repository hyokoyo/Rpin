use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::core::PCWSTR;

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const TOTAL_LOG_BYTES: u64 = 20 * 1024 * 1024;
const MAX_MESSAGE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
        }
    }
}

struct Logger {
    file: File,
    bytes: u64,
    path: PathBuf,
    day: (u16, u16, u16),
}

static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();
static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

pub fn parse_level(value: &str) -> Level {
    match value.trim().to_ascii_lowercase().as_str() {
        "error" => Level::Error,
        "warn" | "warning" => Level::Warn,
        "debug" => Level::Debug,
        _ => Level::Info,
    }
}

pub fn init(level: &str) -> anyhow::Result<()> {
    init_in(&crate::config::logs_dir(), level)
}

pub fn init_in(dir: &Path, level: &str) -> anyhow::Result<()> {
    set_level(level);
    fs::create_dir_all(dir)?;
    // SAFETY: GetLocalTime returns a SYSTEMTIME by value.
    let time = unsafe { GetLocalTime() };
    let _ = LOGGER.set(Mutex::new(Logger::open(dir, time)?));
    Ok(())
}

impl Logger {
    fn open(dir: &Path, time: SYSTEMTIME) -> anyhow::Result<Self> {
        let path = dir.join(format!(
            "rpin-{:04}{:02}{:02}.log",
            time.wYear, time.wMonth, time.wDay
        ));
        if fs::metadata(&path).is_ok_and(|meta| meta.len() >= MAX_LOG_BYTES) {
            rotate(&path)?;
        }
        Self::open_path(path, day(time))
    }

    fn open_path(path: PathBuf, day: (u16, u16, u16)) -> anyhow::Result<Self> {
        prune_archives(&path, TOTAL_LOG_BYTES - MAX_LOG_BYTES)?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes = file.metadata()?.len();
        Ok(Self {
            file,
            bytes,
            path,
            day,
        })
    }

    fn append(&mut self, line: &[u8], time: SYSTEMTIME) -> anyhow::Result<()> {
        if self.day != day(time) {
            if let Some(dir) = self.path.parent() {
                *self = Self::open(dir, time)?;
            }
        }
        if self.bytes + line.len() as u64 > MAX_LOG_BYTES {
            rotate(&self.path)?;
            *self = Self::open_path(self.path.clone(), self.day)?;
        }
        self.file.write_all(line)?;
        self.bytes += line.len() as u64;
        Ok(())
    }
}

pub fn set_level(level: &str) {
    LEVEL.store(parse_level(level) as u8, Ordering::Relaxed);
}

pub fn current_level() -> Level {
    match LEVEL.load(Ordering::Relaxed) {
        0 => Level::Error,
        1 => Level::Warn,
        3 => Level::Debug,
        _ => Level::Info,
    }
}

pub fn info(message: impl AsRef<str>) {
    write(Level::Info, message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    write(Level::Warn, message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    write(Level::Error, message.as_ref());
}

pub fn debug(message: impl AsRef<str>) {
    write(Level::Debug, message.as_ref());
}

pub fn write(level: Level, message: &str) {
    if level > current_level() {
        return;
    }

    // SAFETY: GetLocalTime returns a SYSTEMTIME by value.
    let time = unsafe { GetLocalTime() };
    let mut end = message.len().min(MAX_MESSAGE_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let suffix = if end < message.len() { " …" } else { "" };
    let line = format!(
        "{} [{}] {}{suffix}\n",
        timestamp(time),
        level.as_str(),
        &message[..end]
    );

    if let Some(logger) = LOGGER.get() {
        let guard = if std::thread::panicking() {
            logger.try_lock().ok()
        } else {
            logger.lock().ok()
        };
        if let Some(mut logger) = guard {
            if let Err(err) = logger.append(line.as_bytes(), time) {
                eprintln!("RPin log write failed: {err}");
            }
        } else {
            eprint!("{line}");
        }
    } else {
        eprint!("{line}");
    }

    if cfg!(debug_assertions) {
        let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and valid for the duration of the call.
        unsafe { OutputDebugStringW(PCWSTR(wide.as_ptr())) };
    }
}

pub fn log_file_path() -> Option<PathBuf> {
    LOGGER.get()?.lock().ok().map(|logger| logger.path.clone())
}

fn timestamp(st: SYSTEMTIME) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
    )
}

fn day(time: SYSTEMTIME) -> (u16, u16, u16) {
    (time.wYear, time.wMonth, time.wDay)
}

fn rotate(path: &Path) -> std::io::Result<()> {
    // Move older files first. Results other than a missing source matter.
    for (from, to) in [
        (path.with_extension("log.2"), path.with_extension("log.3")),
        (path.with_extension("log.1"), path.with_extension("log.2")),
        (path.to_owned(), path.with_extension("log.1")),
    ] {
        match fs::rename(from, to) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn prune_archives(active: &Path, budget: u64) -> std::io::Result<()> {
    let Some(dir) = active.parent() else {
        return Ok(());
    };
    let mut archives = Vec::new();
    let mut bytes = 0u64;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(order) = name.to_str().and_then(archive_order) else {
            continue;
        };
        let path = entry.path();
        if path == active {
            continue;
        }
        let metadata = entry.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            continue;
        }
        bytes = bytes.saturating_add(metadata.len());
        archives.push((order, path, metadata.len()));
    }
    if bytes <= budget {
        return Ok(());
    }
    archives.sort_unstable_by_key(|archive| archive.0);
    let mut failure = None;
    for (_, path, size) in archives {
        match fs::remove_file(path) {
            Ok(()) => bytes = bytes.saturating_sub(size),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bytes = bytes.saturating_sub(size)
            }
            Err(err) => {
                failure = Some(err);
            }
        }
        if bytes <= budget {
            return Ok(());
        }
    }
    Err(failure.unwrap_or_else(|| std::io::Error::other("RPin archive budget exceeded")))
}

fn archive_order(name: &str) -> Option<(u32, u8)> {
    let name = name.strip_prefix("rpin-")?;
    let (date, suffix) = name.split_once(".log")?;
    if date.len() != 8 || !date.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let age = match suffix {
        ".3" => 0,
        ".2" => 1,
        ".1" => 2,
        "" => 3,
        _ => return None,
    };
    Some((date.parse().ok()?, age))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestFolder(PathBuf);

    impl TestFolder {
        fn new(name: &str) -> std::io::Result<Self> {
            let path = std::env::temp_dir().join(format!("rpin-log-{name}-{}", std::process::id()));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }
    }

    impl Drop for TestFolder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rotation_bounds_active_file_before_writing() -> anyhow::Result<()> {
        let folder = TestFolder::new("rotate")?;
        let time = SYSTEMTIME {
            wYear: 2026,
            wMonth: 11,
            wDay: 23,
            ..Default::default()
        };
        let active = folder.0.join("rpin-20261123.log");
        File::create_new(&active)?.set_len(MAX_LOG_BYTES - 1)?;
        let mut logger = Logger::open(&folder.0, time)?;
        logger.append(b"new record\n", time)?;
        assert_eq!(fs::read(&active)?, b"new record\n");
        assert_eq!(
            fs::metadata(active.with_extension("log.1"))?.len(),
            MAX_LOG_BYTES - 1
        );
        Ok(())
    }

    #[test]
    fn total_budget_prunes_oldest_rpin_logs_only() -> anyhow::Result<()> {
        let folder = TestFolder::new("budget")?;
        let active = folder.0.join("rpin-20261123.log");
        for name in [
            "rpin-20261121.log",
            "rpin-20261122.log.3",
            "rpin-20261122.log.1",
            "rpin-20261123.log",
            "rpin-20261121.png",
            "rpin-notmine.log",
            "notes.log",
        ] {
            fs::write(folder.0.join(name), b"log")?;
        }
        prune_archives(&active, 3)?;
        assert!(!folder.0.join("rpin-20261121.log").exists());
        assert!(!folder.0.join("rpin-20261122.log.3").exists());
        for name in [
            "rpin-20261122.log.1",
            "rpin-20261123.log",
            "rpin-20261121.png",
            "rpin-notmine.log",
            "notes.log",
        ] {
            assert!(folder.0.join(name).is_file());
        }
        Ok(())
    }
}
