//! Small Win32 helpers: console attach, disk space, profile folder, HKCU registry trees, window
//! focus, opening a file or folder, and the read-only firewall and socket checks that verify uses.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{
    SetHandleInformation, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS,
    ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, HANDLE_FLAG_INHERIT, HWND, INVALID_HANDLE_VALUE,
    LPARAM, NO_ERROR,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetExtendedUdpTable, TCP_TABLE_OWNER_PID_LISTENER, UDP_TABLE_OWNER_PID,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetDiskFreeSpaceExW, GetFileType, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_TYPE_UNKNOWN, OPEN_EXISTING,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Console::{
    AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_HANDLE,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW,
    RegQueryInfoKeyW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ,
    KEY_WRITE, REG_OPTION_NON_VOLATILE,
};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Profile, SHGetKnownFolderPath, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow,
    ShowWindow, SW_RESTORE, SW_SHOWNORMAL,
};

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..end])
        .to_string_lossy()
        .into_owned()
}

/// For `--headless`: keep inherited stdout/stderr when they are real (pipe, file, console);
/// otherwise attach to the parent's console so output shows up in the terminal.
pub fn attach_console() {
    fn usable(which: STD_HANDLE) -> bool {
        // SAFETY: plain Win32 calls on this process's own standard handles.
        unsafe {
            let h = GetStdHandle(which);
            !h.is_null() && h != INVALID_HANDLE_VALUE && GetFileType(h) != FILE_TYPE_UNKNOWN
        }
    }
    let (out_ok, err_ok) = (usable(STD_OUTPUT_HANDLE), usable(STD_ERROR_HANDLE));
    if out_ok && err_ok {
        return;
    }
    // SAFETY: attaching to the parent console and opening CONOUT$ have no preconditions.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        let name = wide("CONOUT$");
        let h = CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return;
        }
        if !out_ok {
            SetStdHandle(STD_OUTPUT_HANDLE, h);
        }
        if !err_ok {
            SetStdHandle(STD_ERROR_HANDLE, h);
        }
    }
}

/// Stops children from inheriting our stdout/stderr. Without this, the emulator (which outlives
/// `--headless boot`) would hold the caller's pipe open and `Shard.exe --headless boot | ...`
/// would not return until the emulator exits. Children that need output get their own pipes.
pub fn std_handles_not_inherited() {
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: plain Win32 calls on this process's own standard handles; failure is harmless.
        unsafe {
            let h = GetStdHandle(which);
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Free bytes available to this user on the volume holding `path`.
pub fn disk_free(path: &Path) -> Option<u64> {
    let p = wide(path);
    let mut free = 0u64;
    // SAFETY: `p` is NUL-terminated and outlives the call; null out-params are allowed.
    let ok = unsafe { GetDiskFreeSpaceExW(p.as_ptr(), &mut free, null_mut(), null_mut()) };
    (ok != 0).then_some(free)
}

/// The user profile folder as the shell reports it (what adb and the emulator use), falling
/// back to %USERPROFILE%.
pub fn profile_dir() -> PathBuf {
    known_folder(&FOLDERID_Profile)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(r"C:\Users\Default"))
}

/// The Desktop folder (may be redirected, e.g. into OneDrive).
pub fn desktop_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_Desktop)
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    let mut out: *mut u16 = null_mut();
    // SAFETY: `id` is a valid known-folder GUID; the returned buffer is freed with CoTaskMemFree.
    unsafe {
        let hr = SHGetKnownFolderPath(id, 0, null_mut(), &mut out);
        let path = (hr >= 0 && !out.is_null()).then(|| {
            let mut len = 0;
            while *out.add(len) != 0 {
                len += 1;
            }
            PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(out, len)))
        });
        if !out.is_null() {
            CoTaskMemFree(out as *const _);
        }
        path
    }
}

// ---------------------------------------------------------------------------------------------
// HKCU registry trees (only used for the emulator's own settings key; see profile.rs).

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegValue {
    pub name: String,
    pub kind: u32,
    /// Raw value bytes as lowercase hex.
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegKeyDump {
    /// Path relative to the dumped root ("" is the root itself).
    pub path: String,
    pub values: Vec<RegValue>,
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the handle came from RegOpenKeyExW/RegCreateKeyExW and is closed once.
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn open_hkcu(subkey: &str, access: u32) -> Result<Option<Key>> {
    let name = wide(subkey);
    let mut h: HKEY = null_mut();
    // SAFETY: `name` is NUL-terminated; `h` receives the opened handle.
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, name.as_ptr(), 0, access, &mut h) };
    match rc {
        ERROR_SUCCESS => Ok(Some(Key(h))),
        ERROR_FILE_NOT_FOUND => Ok(None),
        rc => bail!("opening HKCU\\{subkey} failed (error {rc})"),
    }
}

pub fn hkcu_key_exists(subkey: &str) -> bool {
    matches!(open_hkcu(subkey, KEY_READ), Ok(Some(_)))
}

/// Names of the direct subkeys of HKCU\`subkey` (empty if it does not exist).
pub fn hkcu_subkeys(subkey: &str) -> Result<Vec<String>> {
    let Some(key) = open_hkcu(subkey, KEY_READ)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for i in 0.. {
        let mut name = vec![0u16; 256];
        let mut name_len = name.len() as u32;
        // SAFETY: valid key handle; the name buffer length is passed alongside.
        let rc = unsafe {
            RegEnumKeyExW(
                key.0,
                i,
                name.as_mut_ptr(),
                &mut name_len,
                null(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        if rc != ERROR_SUCCESS {
            bail!("listing subkeys of HKCU\\{subkey} failed (error {rc})");
        }
        out.push(from_wide(&name[..name_len as usize]));
    }
    Ok(out)
}

/// Every key and value under HKCU\`subkey`, or None if the key does not exist.
pub fn hkcu_dump(subkey: &str) -> Result<Option<Vec<RegKeyDump>>> {
    if !hkcu_key_exists(subkey) {
        return Ok(None);
    }
    let mut out = Vec::new();
    dump_into(subkey, "", &mut out)?;
    Ok(Some(out))
}

fn dump_into(root: &str, rel: &str, out: &mut Vec<RegKeyDump>) -> Result<()> {
    let full = if rel.is_empty() {
        root.to_string()
    } else {
        format!("{root}\\{rel}")
    };
    let Some(key) = open_hkcu(&full, KEY_READ)? else {
        return Ok(());
    };
    let (mut subkeys, mut max_sub, mut values, mut max_name, mut max_data) = (0, 0, 0, 0, 0);
    // SAFETY: valid key handle; unused out-params are null.
    let rc = unsafe {
        RegQueryInfoKeyW(
            key.0,
            null_mut(),
            null_mut(),
            null(),
            &mut subkeys,
            &mut max_sub,
            null_mut(),
            &mut values,
            &mut max_name,
            &mut max_data,
            null_mut(),
            null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        bail!("reading HKCU\\{full} failed (error {rc})");
    }
    let mut vals = Vec::new();
    for i in 0..values {
        let mut name = vec![0u16; max_name as usize + 1];
        let mut name_len = name.len() as u32;
        let mut data = vec![0u8; max_data as usize];
        let mut data_len = data.len() as u32;
        let mut kind = 0u32;
        // SAFETY: buffers are sized from RegQueryInfoKeyW and their lengths passed alongside.
        let rc = unsafe {
            RegEnumValueW(
                key.0,
                i,
                name.as_mut_ptr(),
                &mut name_len,
                null(),
                &mut kind,
                data.as_mut_ptr(),
                &mut data_len,
            )
        };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        if rc != ERROR_SUCCESS {
            bail!("reading a value of HKCU\\{full} failed (error {rc})");
        }
        data.truncate(data_len as usize);
        vals.push(RegValue {
            name: from_wide(&name[..name_len as usize]),
            kind,
            data: crate::sdk::download::hex(&data),
        });
    }
    out.push(RegKeyDump {
        path: rel.to_string(),
        values: vals,
    });
    let mut children = Vec::new();
    for i in 0..subkeys {
        let mut name = vec![0u16; max_sub as usize + 1];
        let mut name_len = name.len() as u32;
        // SAFETY: as above.
        let rc = unsafe {
            RegEnumKeyExW(
                key.0,
                i,
                name.as_mut_ptr(),
                &mut name_len,
                null(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        if rc != ERROR_SUCCESS {
            bail!("listing subkeys of HKCU\\{full} failed (error {rc})");
        }
        children.push(from_wide(&name[..name_len as usize]));
    }
    drop(key);
    for child in children {
        let child_rel = if rel.is_empty() {
            child
        } else {
            format!("{rel}\\{child}")
        };
        dump_into(root, &child_rel, out)?;
    }
    Ok(())
}

/// Recreates a dump produced by [`hkcu_dump`] under HKCU\`subkey`.
pub fn hkcu_restore(subkey: &str, dump: &[RegKeyDump]) -> Result<()> {
    for entry in dump {
        let full = if entry.path.is_empty() {
            subkey.to_string()
        } else {
            format!("{subkey}\\{}", entry.path)
        };
        let name = wide(&full);
        let mut h: HKEY = null_mut();
        // SAFETY: `name` is NUL-terminated; disposition is not needed.
        let rc = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                name.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                null(),
                &mut h,
                null_mut(),
            )
        };
        if rc != ERROR_SUCCESS {
            bail!("creating HKCU\\{full} failed (error {rc})");
        }
        let key = Key(h);
        for v in &entry.values {
            let data = crate::sdk::download::unhex(&v.data)?;
            let vname = wide(&v.name);
            // SAFETY: data pointer/length describe `data`; value name is NUL-terminated.
            let rc = unsafe {
                RegSetValueExW(
                    key.0,
                    vname.as_ptr(),
                    0,
                    v.kind,
                    data.as_ptr(),
                    data.len() as u32,
                )
            };
            if rc != ERROR_SUCCESS {
                bail!("writing HKCU\\{full}\\{} failed (error {rc})", v.name);
            }
        }
    }
    Ok(())
}

/// Deletes HKCU\`subkey` and everything below it. Missing key is not an error.
pub fn hkcu_delete_tree(subkey: &str) -> Result<()> {
    let name = wide(subkey);
    // SAFETY: `name` is NUL-terminated; RegDeleteTreeW with a subkey name removes that key too.
    let rc = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, name.as_ptr()) };
    match rc {
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => {}
        rc => bail!("deleting HKCU\\{subkey} failed (error {rc})"),
    }
    if hkcu_key_exists(subkey) {
        bail!("HKCU\\{subkey} still exists after delete");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------

/// Brings the first visible top-level window of `pid` to the front. Returns whether one was found.
pub fn focus_window_of(pid: u32) -> bool {
    struct Search {
        pid: u32,
        found: HWND,
    }
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> i32 {
        // SAFETY: lparam is the &mut Search passed to EnumWindows below, alive for the call.
        let search = unsafe { &mut *(lparam as *mut Search) };
        let mut owner = 0u32;
        // SAFETY: hwnd comes from EnumWindows.
        unsafe { GetWindowThreadProcessId(hwnd, &mut owner) };
        if owner == search.pid && unsafe { IsWindowVisible(hwnd) } != 0 {
            search.found = hwnd;
            return 0;
        }
        1
    }
    let mut search = Search {
        pid,
        found: null_mut(),
    };
    // SAFETY: the callback only touches `search`, which outlives EnumWindows.
    unsafe {
        EnumWindows(Some(each), &mut search as *mut Search as LPARAM);
        if search.found.is_null() {
            return false;
        }
        if IsIconic(search.found) != 0 {
            ShowWindow(search.found, SW_RESTORE);
        }
        SetForegroundWindow(search.found);
    }
    true
}

// ---------------------------------------------------------------------------------------------
// Read-only network checks used by verify: Windows Firewall rules and bound sockets.

const FIREWALL_RULES: &str =
    "SYSTEM\\CurrentControlSet\\Services\\SharedAccess\\Parameters\\FirewallPolicy\\FirewallRules";

/// Windows Firewall rules whose program lies inside `dir`, e.g.
/// "adb: Block In UDP (d:\shard\sdk\platform-tools\adb.exe)". Windows adds such rules by itself
/// ("Query User" rules) when a program listens on the network.
pub fn firewall_rules_for(dir: &Path) -> Result<Vec<String>> {
    let name = wide(FIREWALL_RULES);
    let mut h: HKEY = null_mut();
    // SAFETY: `name` is NUL-terminated; `h` receives the opened handle (read-only).
    let rc = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, name.as_ptr(), 0, KEY_READ, &mut h) };
    if rc != ERROR_SUCCESS {
        bail!("reading the Windows Firewall rules failed (error {rc})");
    }
    let key = Key(h);
    let mut out = Vec::new();
    for i in 0.. {
        let mut vname = vec![0u16; 512];
        let mut vname_len = vname.len() as u32;
        let mut data = vec![0u8; 8192];
        let mut data_len = data.len() as u32;
        let mut kind = 0u32;
        // SAFETY: buffers are passed with their lengths.
        let rc = unsafe {
            RegEnumValueW(
                key.0,
                i,
                vname.as_mut_ptr(),
                &mut vname_len,
                null(),
                &mut kind,
                data.as_mut_ptr(),
                &mut data_len,
            )
        };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        if rc != ERROR_SUCCESS {
            // ERROR_MORE_DATA for an unusually long rule: not one of ours.
            continue;
        }
        let words: Vec<u16> = data[..data_len as usize]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        if let Some(rule) = describe_firewall_rule(&from_wide(&words), dir) {
            out.push(rule);
        }
    }
    Ok(out)
}

/// Describes one rule string ("v2.33|Action=Block|Dir=In|Protocol=17|App=...|Name=adb|...") if
/// its program lies inside `dir`.
pub fn describe_firewall_rule(rule: &str, dir: &Path) -> Option<String> {
    let field = |k: &str| {
        rule.split('|')
            .find_map(|f| f.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
    };
    let app = field("App")?;
    if !crate::paths::path_starts_with(Path::new(app), dir) {
        return None;
    }
    let proto = match field("Protocol") {
        Some("6") => "TCP",
        Some("17") => "UDP",
        Some(p) => p,
        None => "any",
    };
    Some(format!(
        "{}: {} {} {proto} ({app})",
        field("Name").unwrap_or("?"),
        field("Action").unwrap_or("?"),
        field("Dir").unwrap_or("?"),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Socket {
    pub pid: u32,
    pub udp: bool,
    pub addr: std::net::SocketAddr,
}

/// TCP listeners and UDP sockets of every process, IPv4 and IPv6.
pub fn bound_sockets() -> Vec<Socket> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    // (family, udp, row size, address offset, port offset, pid offset) for MIB_TCPROW_OWNER_PID,
    // MIB_UDPROW_OWNER_PID, MIB_TCP6ROW_OWNER_PID and MIB_UDP6ROW_OWNER_PID.
    let tables: [(u32, bool, usize, usize, usize, usize); 4] = [
        (AF_INET as u32, false, 24, 4, 8, 20),
        (AF_INET as u32, true, 12, 0, 4, 8),
        (AF_INET6 as u32, false, 56, 0, 20, 52),
        (AF_INET6 as u32, true, 28, 0, 20, 24),
    ];
    let mut out = Vec::new();
    for (af, udp, row, addr_off, port_off, pid_off) in tables {
        let mut buf = vec![0u8; 64 * 1024];
        let mut rc = NO_ERROR;
        for _ in 0..3 {
            let mut size = buf.len() as u32;
            // SAFETY: `buf` is writable for `size` bytes; the table classes match the layouts.
            rc = unsafe {
                if udp {
                    GetExtendedUdpTable(
                        buf.as_mut_ptr().cast(),
                        &mut size,
                        0,
                        af,
                        UDP_TABLE_OWNER_PID,
                        0,
                    )
                } else {
                    GetExtendedTcpTable(
                        buf.as_mut_ptr().cast(),
                        &mut size,
                        0,
                        af,
                        TCP_TABLE_OWNER_PID_LISTENER,
                        0,
                    )
                }
            };
            if rc == ERROR_INSUFFICIENT_BUFFER {
                buf = vec![0u8; size as usize + 4096];
                continue;
            }
            break;
        }
        if rc != NO_ERROR {
            continue;
        }
        let u32_at = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
        let count = u32_at(0) as usize;
        for i in 0..count {
            let base = 4 + i * row;
            if base + row > buf.len() {
                break;
            }
            let ip = if af == AF_INET as u32 {
                IpAddr::V4(Ipv4Addr::from(u32_at(base + addr_off).to_le_bytes()))
            } else {
                let mut b = [0u8; 16];
                b.copy_from_slice(&buf[base + addr_off..base + addr_off + 16]);
                IpAddr::V6(Ipv6Addr::from(b))
            };
            // Ports are stored in network byte order in the low 16 bits.
            let p = u32_at(base + port_off);
            let port = u16::from_be_bytes([p as u8, (p >> 8) as u8]);
            out.push(Socket {
                pid: u32_at(base + pid_off),
                udp,
                addr: SocketAddr::new(ip, port),
            });
        }
    }
    out
}

/// Opens a file or folder the way Explorer would (a log in its text editor, a folder in a new
/// Explorer window). Only paths inside the Shard folder are passed here.
pub fn open_path(path: &Path) -> bool {
    let verb = wide("open");
    let file = wide(path);
    // SAFETY: both strings are NUL-terminated and outlive the call.
    let r = unsafe {
        ShellExecuteW(
            null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    r as usize > 32
}
