//! `logs\shard.log` through tracing, with a file handle that can be closed and reopened (the
//! folder-move check must not hold files open inside the folder while it renames it).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};

use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

static SINK: Mutex<Option<File>> = Mutex::new(None);
static INIT: Once = Once::new();
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

fn now() -> SYSTEMTIME {
    // SAFETY: GetLocalTime only writes the struct it is given.
    unsafe {
        let mut t: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    }
}

/// Local time as `YYYYMMDD-HHMMSS`, for file names.
pub fn stamp() -> String {
    let t = now();
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

struct LocalTime;

impl FormatTime for LocalTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        let t = now();
        write!(
            w,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
        )
    }
}

struct SinkWriter;

impl Write for SinkWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut guard) = SINK.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.write_all(buf);
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if let Ok(mut guard) = SINK.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.flush();
            }
        }
        Ok(())
    }
}

/// Installs the subscriber (once per process) and opens `logs\shard.log`.
pub fn init(logs: &Path) {
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_writer(|| SinkWriter)
            .with_ansi(false)
            .with_target(false)
            .with_timer(LocalTime)
            .try_init();
    });
    open(&logs.join("shard.log"));
}

pub fn open(path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_LOG_BYTES {
            let mut old = path.as_os_str().to_os_string();
            old.push(".1");
            let _ = std::fs::rename(path, PathBuf::from(old));
        }
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = OpenOptions::new().create(true).append(true).open(path).ok();
    if let Ok(mut guard) = SINK.lock() {
        *guard = file;
    }
}

/// Closes the log file (logging continues to nowhere until [`open`]).
pub fn close() {
    if let Ok(mut guard) = SINK.lock() {
        *guard = None;
    }
}

/// Keeps the newest `keep` files in `dir` whose names start with `prefix`.
pub fn prune(dir: &Path, prefix: &str, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
        })
        .collect();
    // Names embed a sortable timestamp.
    files.sort();
    let excess = files.len().saturating_sub(keep);
    for f in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(f);
    }
}
