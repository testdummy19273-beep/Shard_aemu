//! HTTP download with resume (Range), retries with backoff, progress, cancel and checksum check.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::header::RANGE;
use reqwest::{StatusCode, Url};
use sha1::Digest;

use super::manifest::{Checksum, ChecksumKind};

#[derive(Debug, thiserror::Error)]
#[error("cancelled")]
pub struct Cancelled;

pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Cancelled>().is_some()
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

pub fn unhex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        bail!("odd-length hex string");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| anyhow!("bad hex: {e}")))
        .collect()
}

enum Hasher {
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
}

impl Hasher {
    fn new(kind: ChecksumKind) -> Self {
        match kind {
            ChecksumKind::Sha1 => Self::Sha1(sha1::Sha1::new()),
            ChecksumKind::Sha256 => Self::Sha256(sha2::Sha256::new()),
        }
    }
    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha1(h) => h.update(data),
            Self::Sha256(h) => h.update(data),
        }
    }
    fn finish(self) -> String {
        match self {
            Self::Sha1(h) => hex(&h.finalize()),
            Self::Sha256(h) => hex(&h.finalize()),
        }
    }
}

/// Hex digest of `reader`'s contents.
pub fn digest(kind: ChecksumKind, mut reader: impl Read) -> std::io::Result<String> {
    let mut h = Hasher::new(kind);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finish())
}

pub fn matches(checksum: &Checksum, data: impl Read) -> std::io::Result<bool> {
    Ok(digest(checksum.kind, data)?.eq_ignore_ascii_case(checksum.hex.trim()))
}

/// True if `path` exists with the expected size and checksum.
pub fn file_ok(path: &Path, size: u64, checksum: &Checksum) -> Result<bool> {
    match std::fs::metadata(path) {
        Ok(m) if m.len() == size || size == 0 => {}
        _ => return Ok(false),
    }
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    Ok(matches(checksum, std::io::BufReader::new(file))?)
}

#[derive(Debug, Clone)]
pub struct Spec {
    pub url: Url,
    pub dest: PathBuf,
    pub size: u64,
    pub checksum: Checksum,
}

pub struct Downloader<'a> {
    pub client: &'a reqwest::blocking::Client,
    pub cancel: &'a AtomicBool,
    /// Called with (bytes of this file done, file size).
    pub progress: &'a mut dyn FnMut(u64, u64),
    pub log: &'a mut dyn FnMut(String),
    /// Backoff before network retries 1..=3 (2/4/8 s in production).
    pub backoff: [Duration; 3],
}

impl Downloader<'_> {
    /// Downloads `spec.dest` (resuming a `.part` file) and verifies it. On checksum mismatch the
    /// file is deleted and downloaded once more before giving up.
    pub fn fetch(&mut self, spec: &Spec) -> Result<()> {
        if file_ok(&spec.dest, spec.size, &spec.checksum)? {
            (self.log)(format!(
                "{} is already downloaded and its checksum matches",
                file_name(&spec.dest)
            ));
            (self.progress)(spec.size, spec.size);
            return Ok(());
        }
        let _ = std::fs::remove_file(&spec.dest);
        for round in 0..2 {
            self.with_retries(spec)?;
            if file_ok(&spec.dest, spec.size, &spec.checksum)? {
                return Ok(());
            }
            let _ = std::fs::remove_file(&spec.dest);
            let _ = std::fs::remove_file(part_path(&spec.dest));
            if round == 0 {
                (self.log)(format!(
                    "checksum mismatch for {}, downloading it again",
                    file_name(&spec.dest)
                ));
            }
        }
        bail!(
            "{} failed its checksum twice; the download is corrupt (proxy, antivirus or disk problem?)",
            file_name(&spec.dest)
        )
    }

    fn with_retries(&mut self, spec: &Spec) -> Result<()> {
        let mut attempt = 0;
        loop {
            match self.attempt(spec) {
                Ok(()) => return Ok(()),
                Err(e) if is_cancelled(&e) => return Err(e),
                Err(e) if attempt < self.backoff.len() => {
                    let wait = self.backoff[attempt];
                    attempt += 1;
                    (self.log)(format!(
                        "download of {} failed ({e:#}); retry {attempt}/3 in {}s",
                        file_name(&spec.dest),
                        wait.as_secs()
                    ));
                    self.sleep(wait)?;
                }
                Err(e) => {
                    return Err(e.context(format!(
                        "downloading {} failed after 3 retries",
                        file_name(&spec.dest)
                    )))
                }
            }
        }
    }

    fn sleep(&self, d: Duration) -> Result<()> {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }

    fn attempt(&mut self, spec: &Spec) -> Result<()> {
        let part = part_path(&spec.dest);
        if let Some(dir) = part.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if spec.size > 0 && have > spec.size {
            have = 0;
        }
        if spec.size == 0 || have < spec.size {
            let mut req = self.client.get(spec.url.clone());
            if have > 0 {
                req = req.header(RANGE, format!("bytes={have}-"));
            }
            let mut resp = req
                .send()
                .with_context(|| format!("connecting to {}", spec.url))?;
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&part)
                .with_context(|| format!("opening {}", part.display()))?;
            match resp.status() {
                StatusCode::PARTIAL_CONTENT if have > 0 => {
                    file.seek(SeekFrom::Start(have))?;
                }
                StatusCode::OK => {
                    have = 0;
                    file.set_len(0)?;
                }
                StatusCode::RANGE_NOT_SATISFIABLE => {
                    file.set_len(0)?;
                    bail!("server rejected the resume offset; restarting");
                }
                s => bail!("{} returned HTTP {s}", spec.url),
            }
            let mut buf = vec![0u8; 256 * 1024];
            let mut last = Instant::now();
            loop {
                if self.cancel.load(Ordering::Relaxed) {
                    return Err(Cancelled.into());
                }
                let n = resp.read(&mut buf).context("reading from the network")?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n])
                    .map_err(super::install::friendly_io)?;
                have += n as u64;
                if last.elapsed() >= Duration::from_millis(200) {
                    (self.progress)(have, spec.size);
                    last = Instant::now();
                }
            }
            file.flush()?;
            drop(file);
            if spec.size > 0 && have != spec.size {
                bail!("connection closed early ({have} of {} bytes)", spec.size);
            }
        }
        (self.progress)(have, spec.size);
        let _ = std::fs::remove_file(&spec.dest);
        std::fs::rename(&part, &spec.dest)
            .with_context(|| format!("renaming {} into place", part.display()))?;
        Ok(())
    }
}

pub fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("Shard/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        // In reqwest's blocking client this bounds the wait for response headers and each
        // individual body read, not the whole transfer: a stalled connection fails (and is
        // retried with resume) instead of hanging, while a slow 1.7 GB download still finishes.
        .timeout(Duration::from_secs(60))
        .build()
        .context("creating the HTTP client")
}
