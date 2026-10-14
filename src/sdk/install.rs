//! Extracting SDK archives into the `sdk\` layout the emulator expects.
//!
//! Each archive has one top-level folder (`emulator/`, `platform-tools/`, `x86_64/`). It is
//! extracted into `sdk\.staging\<key>` with that folder stripped, then renamed into place, so an
//! interrupted extract leaves only staging behind and is simply redone.
//! No `package.xml`, `source.properties` or `platforms\` files are written: the emulator runs
//! from this bare layout (tested with emulator 37.2.12; the archives carry their own
//! `source.properties`).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use super::download::Cancelled;

const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_LOCK_VIOLATION: i32 = 33;
const ERROR_HANDLE_DISK_FULL: i32 = 39;
const ERROR_DISK_FULL: i32 = 112;

/// Turns disk-full errors into a message a user can act on.
pub fn friendly_io(e: std::io::Error) -> anyhow::Error {
    match e.raw_os_error() {
        Some(ERROR_DISK_FULL) | Some(ERROR_HANDLE_DISK_FULL) => anyhow!(
            "the disk is full. Free some space (Shard needs about 15 GB) and run setup again"
        ),
        _ => anyhow::Error::new(e),
    }
}

fn is_lock_error(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(ERROR_ACCESS_DENIED) | Some(ERROR_SHARING_VIOLATION) | Some(ERROR_LOCK_VIOLATION)
    )
}

/// Retries `op` while it fails with access-denied/sharing violations (typically an antivirus
/// scanning a freshly written file), backing off 0.5, 1, 2, 4, 8 s.
pub fn retry_locked<T>(what: &str, mut op: impl FnMut() -> std::io::Result<T>) -> Result<T> {
    let mut wait = Duration::from_millis(500);
    for _ in 0..5 {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if is_lock_error(&e) => {
                std::thread::sleep(wait);
                wait *= 2;
            }
            Err(e) => return Err(friendly_io(e).context(what.to_string())),
        }
    }
    op().map_err(|e| {
        if is_lock_error(&e) {
            anyhow!(
                "{what}: the file stays locked by another program (often an antivirus scan). \
                 Wait a minute or exclude the Shard folder, then retry ({e})"
            )
        } else {
            friendly_io(e).context(what.to_string())
        }
    })
}

pub fn remove_dir_all_retry(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    retry_locked(&format!("removing {}", dir.display()), || {
        std::fs::remove_dir_all(dir)
    })
}

/// Drops the first component of an archive path ("emulator/lib/x.dll" -> "lib/x.dll").
pub fn strip_top(name: &Path) -> Option<PathBuf> {
    let mut comps = name.components();
    match comps.next()? {
        Component::Normal(_) => {}
        _ => return None,
    }
    let rest: PathBuf = comps.collect();
    (!rest.as_os_str().is_empty()).then_some(rest)
}

/// Extracts `archive` into `target` via `staging`, stripping the single top-level folder.
/// `progress` gets (entries done, entries total).
pub fn extract(
    archive: &Path,
    staging: &Path,
    target: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    let file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))
        .with_context(|| format!("{} is not a valid zip archive", archive.display()))?;
    remove_dir_all_retry(staging)?;
    std::fs::create_dir_all(staging).map_err(friendly_io)?;
    let total = zip.len() as u64;
    for i in 0..zip.len() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let mut entry = zip.by_index(i)?;
        let Some(name) = entry.enclosed_name() else {
            bail!(
                "{} contains an unsafe path ({}); refusing to extract",
                archive.display(),
                String::from_utf8_lossy(entry.name_raw())
            );
        };
        let Some(rel) = strip_top(&name) else {
            continue;
        };
        let out = staging.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(friendly_io)?;
        } else {
            if let Some(dir) = out.parent() {
                std::fs::create_dir_all(dir).map_err(friendly_io)?;
            }
            let f = retry_locked(&format!("creating {}", out.display()), || {
                File::create(&out)
            })?;
            let mut w = BufWriter::with_capacity(1 << 20, f);
            std::io::copy(&mut entry, &mut w)
                .map_err(friendly_io)
                .with_context(|| format!("extracting {}", out.display()))?;
            w.flush().map_err(friendly_io)?;
        }
        progress(i as u64 + 1, total);
    }
    remove_dir_all_retry(target)?;
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(friendly_io)?;
    }
    retry_locked(&format!("moving {} into place", target.display()), || {
        std::fs::rename(staging, target)
    })?;
    Ok(())
}
