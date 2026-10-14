//! First-run checks on the Shard folder itself: writability, location, path length, free space.

use crate::paths::{path_starts_with, Paths};

/// Longest path under the Shard root after setup (measured on emulator 37.2.12 + API 33 r9:
/// `sdk\system-images\android-33\google_apis_playstore\x86_64\data\misc\modem_simulator\...`).
pub const LONGEST_RELATIVE_PATH: usize = 132;
/// Classic MAX_PATH minus the terminating NUL.
pub const MAX_PATH_CHARS: usize = 259;
pub const MIN_FREE_BYTES: u64 = 15 * 1024 * 1024 * 1024;

/// Warnings about where the folder lives. Empty = all good.
pub fn path_warnings(paths: &Paths) -> Vec<String> {
    let mut out = Vec::new();
    let root = &paths.root;
    let probe = paths.state.join(".write-test");
    let writable = std::fs::create_dir_all(&paths.state).is_ok()
        && std::fs::write(&probe, b"ok").is_ok()
        && std::fs::remove_file(&probe).is_ok();
    if !writable {
        out.push(format!(
            "The Shard folder is not writable ({}). Move it somewhere you can write to.",
            root.display()
        ));
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Some(dir) = std::env::var_os(var) {
            if path_starts_with(root, std::path::Path::new(&dir)) {
                out.push(format!(
                    "Shard is inside {} ({var}). Windows protects that folder; move Shard to e.g. D:\\Shard.",
                    std::path::Path::new(&dir).display()
                ));
                break;
            }
        }
    }
    for var in ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"] {
        if let Some(dir) = std::env::var_os(var) {
            if path_starts_with(root, std::path::Path::new(&dir)) {
                out.push(format!(
                    "Shard is inside a OneDrive-synced folder ({}). OneDrive would upload ~15 GB of emulator files; move Shard outside it.",
                    std::path::Path::new(&dir).display()
                ));
                break;
            }
        }
    }
    let len = root.as_os_str().len() + 1 + LONGEST_RELATIVE_PATH;
    if len > MAX_PATH_CHARS {
        out.push(format!(
            "The folder path is long ({} characters); the deepest emulator files would reach {len} characters, over the 260-character Windows limit. Use a shorter path such as D:\\Shard.",
            root.as_os_str().len()
        ));
    }
    if !root.to_string_lossy().is_ascii() {
        out.push(
            "The folder path has non-ASCII characters. Shard works there, but the emulator can't load its own Vulkan loader from such a path (it falls back to Windows' copy) and this setup is less tested. A plain ASCII path such as D:\\Shard is safer."
                .into(),
        );
    }
    out
}

pub fn disk_warning(paths: &Paths) -> Option<String> {
    let free = crate::winutil::disk_free(&paths.root)?;
    (free < MIN_FREE_BYTES).then(|| {
        format!(
            "Only {:.1} GB free on this drive; Shard needs about 15 GB (SDK, image, user data). Setup may fail.",
            free as f64 / (1u64 << 30) as f64
        )
    })
}
