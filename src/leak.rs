//! Fingerprints of the places outside the Shard folder that adb or the emulator could write to.
//! Setup records one before anything else runs (`state\leak-baseline.toml`); verify check b takes
//! another at its start and one at the end, and compares.
//!
//! Two kinds of entries:
//! - explicit targets (the profile items from `profile.rs`, the emulator's registry key, the
//!   Desktop folder): any difference is a leak, including a pre-existing item that changed;
//! - a name scan of `%USERPROFILE%`, `%LOCALAPPDATA%`, `%APPDATA%`, `%TEMP%` and `HKCU\Software`
//!   for Android/emulator-looking names: a new name is a leak.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha1::Digest;

use crate::paths::Paths;
use crate::profile;
use crate::winutil;

/// Name tokens that mark something as ours (adb, emulator, qemu and their helpers).
const KEYWORDS: &[&str] = &[
    "android", "emulator", "adb", "qemu", "netsim", "crashpad", "avd",
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Explicit target -> fingerprint ("absent", "file ...", "dir ...", "key ...").
    pub targets: BTreeMap<String, String>,
    /// Location -> matching names found there.
    pub scan: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Diff {
    /// (target, before, after)
    pub changed: Vec<(String, String, String)>,
    /// "location\name"
    pub added: Vec<String>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.added.is_empty()
    }
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = self
            .changed
            .iter()
            .map(|(t, a, b)| format!("{t}: {a} -> {b}"))
            .collect();
        parts.extend(self.added.iter().map(|a| format!("new: {a}")));
        parts.join("; ")
    }
}

pub fn baseline_file(paths: &Paths) -> PathBuf {
    paths.state.join("leak-baseline.toml")
}

pub fn load_baseline(paths: &Paths) -> Option<Snapshot> {
    toml::from_str(&std::fs::read_to_string(baseline_file(paths)).ok()?).ok()
}

/// Records the baseline once (first setup). Later setups keep the first one.
pub fn record_baseline(paths: &Paths) -> Result<()> {
    let file = baseline_file(paths);
    if file.is_file() {
        return Ok(());
    }
    let snap = take();
    std::fs::write(&file, toml::to_string(&snap)?).context("writing state\\leak-baseline.toml")
}

pub fn take() -> Snapshot {
    take_with(&winutil::profile_dir())
}

pub fn take_with(profile_dir: &Path) -> Snapshot {
    let mut snap = Snapshot::default();
    for d in profile::DIRS {
        let p = profile_dir.join(d);
        snap.targets.insert(display(&p), fingerprint(&p));
    }
    for f in profile::FILES {
        let p = profile_dir.join(f);
        snap.targets.insert(display(&p), fingerprint(&p));
    }
    snap.targets.insert(
        format!("HKCU\\{}", profile::REG_PARENT),
        reg_fingerprint(profile::REG_PARENT),
    );
    if let Some(desktop) = winutil::desktop_dir() {
        snap.targets.insert(
            format!("Desktop ({})", desktop.display()),
            fingerprint(&desktop),
        );
    }

    let mut dirs: Vec<(String, PathBuf)> = vec![("%USERPROFILE%".into(), profile_dir.into())];
    for var in ["LOCALAPPDATA", "APPDATA", "TEMP"] {
        if let Some(v) = std::env::var_os(var) {
            dirs.push((format!("%{var}%"), PathBuf::from(v)));
        }
    }
    for (label, dir) in dirs {
        let names = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| looks_ours(n))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        snap.scan.insert(label, sorted(names));
    }
    let keys = winutil::hkcu_subkeys("Software").unwrap_or_default();
    snap.scan.insert(
        "HKCU\\Software".into(),
        sorted(keys.into_iter().filter(|k| looks_ours(k)).collect()),
    );
    snap
}

/// Differences from `before` to `after` that count as leaks.
pub fn diff(before: &Snapshot, after: &Snapshot) -> Diff {
    let mut d = Diff::default();
    for (target, now) in &after.targets {
        let was = before.targets.get(target).map(String::as_str);
        if was != Some(now.as_str()) {
            d.changed.push((
                target.clone(),
                was.unwrap_or("(not recorded)").to_string(),
                now.clone(),
            ));
        }
    }
    for (loc, names) in &after.scan {
        let old = before.scan.get(loc);
        for n in names {
            if !old.is_some_and(|o| o.contains(n)) {
                d.added.push(format!("{loc}\\{n}"));
            }
        }
    }
    d
}

/// Whether a file or key name contains one of [`KEYWORDS`] at the start of a word
/// ("adbkey", ".android", "AndroidStudio", "Android Open Source Project"). Word starts are
/// alphanumeric runs, so GUID-like temp names ("3adb...") do not match.
pub fn looks_ours(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| KEYWORDS.iter().any(|k| word_matches(w, k)))
}

fn word_matches(word: &str, keyword: &str) -> bool {
    let Some(rest) = word.strip_prefix(keyword) else {
        return false;
    };
    // A short keyword followed by a hex digit is most likely part of a GUID or hash ("adb3f2…").
    keyword.len() > 3 || !rest.starts_with(|c: char| c.is_ascii_hexdigit())
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn display(p: &Path) -> String {
    p.display().to_string()
}

fn mtime(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn fingerprint(p: &Path) -> String {
    let Ok(meta) = std::fs::metadata(p) else {
        return "absent".into();
    };
    if meta.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(p)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        let listing = crate::sdk::download::hex(&sha1::Sha1::digest(names.join("\n").as_bytes()));
        format!(
            "dir modified={} entries={} listing={}",
            mtime(&meta),
            names.len(),
            &listing[..12]
        )
    } else {
        let hash = std::fs::read(p)
            .map(|b| crate::sdk::download::hex(&sha1::Sha1::digest(&b)))
            .unwrap_or_default();
        format!(
            "file size={} modified={} sha1={}",
            meta.len(),
            mtime(&meta),
            hash.get(..12).unwrap_or("")
        )
    }
}

fn reg_fingerprint(subkey: &str) -> String {
    match winutil::hkcu_dump(subkey) {
        Ok(None) => "absent".into(),
        Ok(Some(dump)) => {
            let text = format!("{dump:?}");
            let hash = crate::sdk::download::hex(&sha1::Sha1::digest(text.as_bytes()));
            format!("key keys={} sha1={}", dump.len(), &hash[..12])
        }
        Err(e) => format!("unreadable ({e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_matching() {
        for n in [
            ".android",
            "adbkey",
            "AndroidStudio2024",
            "Android Open Source Project",
            ".emulator_console_auth_token",
            "qemu-img",
            "netsimd",
        ] {
            assert!(looks_ours(n), "{n}");
        }
        for n in [
            "3adb4f2e-1111-2222-3333-444455556666",
            "Downloads",
            "badboy",
            "adb3f2a1",
            "NVIDIA",
        ] {
            assert!(!looks_ours(n), "{n}");
        }
    }

    #[test]
    fn diff_reports_changes_and_new_names() {
        let mut a = Snapshot::default();
        a.targets.insert("x".into(), "absent".into());
        a.scan.insert("%TEMP%".into(), vec!["old-adb".into()]);
        let mut b = a.clone();
        assert!(diff(&a, &b).is_empty());
        b.targets.insert("x".into(), "file size=1".into());
        b.scan
            .insert("%TEMP%".into(), vec!["old-adb".into(), "adb.log".into()]);
        let d = diff(&a, &b);
        assert_eq!(d.changed.len(), 1);
        assert_eq!(d.added, vec!["%TEMP%\\adb.log".to_string()]);
    }
}
