# Shard

A lightweight, portable launcher for Google's Android Emulator on Windows. Shard downloads the
official Android Emulator, platform-tools and the Android 13 (API 33) Google Play x86_64 system
image straight from Google, builds one Android device, and boots, controls and debloats it.

> Status: milestone 3 (UI). Setup with a checklist, home tiles, app launch, the side bar,
> debloat and wipe data work from the window. The full Settings tab comes in milestone 4, along
> with the rest of this README.

## Requirements

- Windows 10/11 x64 with **Windows Hypervisor Platform** turned on (Start → "Turn Windows
  features on or off" → Windows Hypervisor Platform, then restart). Shard checks this and tells
  you; it never changes Windows features itself.
- About **15 GB free** for the SDK, the system image and the device's data. Put the Shard folder
  on a **fast SSD**: boot time and in-game loading depend on it. The device's first start sets
  Android up (about a minute on a healthy SSD). A slow or failing drive can push it past the
  240 s boot limit, and Shard then reports a timeout with the end of the emulator's log.
- An internet connection for the first setup (about 2 GB from `dl.google.com`) and for Repair.
  Shard makes no other network calls.

## Use

Put `Shard.exe` in its own folder (e.g. `D:\Shard`) and run it. Everything Shard creates lives
next to it:

```
Shard.exe  shard.toml  debloat.txt
sdk\    emulator, platform-tools, system image (from Google)
avd\    the "shard" Android device and its data
home\   emulator/adb settings, Shard's copies of the adb key and console token
tmp\    temp files of the emulator and adb
cache\  downloads in progress
state\  what is installed, what is running, lock file
logs\   shard.log, emulator-*.log, screenshots
```

Moving or renaming the folder is fine; paths are rewritten on every start. A path with only
ASCII characters is safest (the emulator works elsewhere, but cannot load its bundled Vulkan
loader from a non-ASCII path and falls back to Windows' copy; Shard warns about this).

### The window

- **First run** shows a checklist: folder and disk checks, Windows Hypervisor Platform, the
  three downloads with progress bars, extract, `emulator -accel-check`, creating the device,
  the first start, debloat and the Play Store. Cancel stops at once and keeps what was
  downloaded; Resume (or Start setup after restarting Shard) continues a download where it
  stopped. When setup is done, Shard starts the device; "Open Home" switches to the home page.
- **Home** has a tile per app: the Play Store first, then every app you installed
  (`pm list packages -3`), read when the emulator starts and when you come back to Shard's
  window (at most every 5 s). Names come from the package name (`com.miHoYo.GenshinImpact` →
  "Genshin Impact"); Android has no shell command that gives an app's real name. Clicking a
  tile starts the emulator if needed and opens the app (its launcher activity, or `monkey` if
  that cannot be found).
- **The side bar** on the right sends Home, Recent apps and Back, plus volume up/down and
  rotate from the ☰ menu. Keys go through one adb shell kept open while the emulator runs
  (it is reopened if it dies), so a click takes 10–25 ms. Android shows its volume panel on
  the first volume press and changes the volume from the second (Android's own behavior; a
  plain `input keyevent` does the same). Rotate turns the emulator window; Android turns its screen only when the app in front
  allows it (Settings does, the launcher does not).
- **The gear** opens the device page: Debloat again, Wipe data (asks first), Open Shard folder
  and Repair (download again whatever is missing or damaged; this is the only other time
  Shard goes online).
- **Errors** show at the top with Retry and Open log. When the emulator dies, the last 30
  lines of its log are under Details.

### Headless

```
Shard.exe --headless setup      download and install, create the device
Shard.exe --headless boot       start the emulator and wait for Android (it keeps running)
Shard.exe --headless debloat    remove the apps in debloat.txt (starts the emulator if needed)
Shard.exe --headless wipe       reset the device's data on its next start (emulator stopped)
Shard.exe --headless stop       stop the emulator and adb
Shard.exe --headless verify     self-checks, one PASS/FAIL/SKIP line each; exit code 1 on FAIL
```

For tests there is also `Shard.exe --headless launch <package>` (not listed in `--help`): it
opens an app the way its tile does, starting the emulator if needed.

`Shard.exe` is a window app, so `cmd` and PowerShell do not wait for it by themselves. Use
`start /wait Shard.exe --headless verify` in cmd, or
`Start-Process .\Shard.exe '--headless','verify' -Wait -NoNewWindow` in PowerShell, or pipe the
output (`.\Shard.exe --headless verify | Out-Host`). Exit codes: 0 ok, 1 failed, 2 another Shard
is already running in this folder.

### The device's first boot

On the device's first start (and again after Wipe data) Shard waits for the home screen, then:

1. saves the full package list to `logs\packages-initial.txt`;
2. removes the apps listed in `debloat.txt` for user 0 (`pm uninstall --user 0`). Nothing is
   deleted from the system image, and every app can be brought back
   (`cmd package install-existing`; Wipe data restores all of them). Android saves a removal
   about 10 s later, so Shard waits up to 15 s before calling the debloat done;
3. sets the animation scales to 0.5x, keeps the screen on while "charging" and turns Android's
   Bluetooth off. The emulator runs without Bluetooth emulation (netsim is off), and with
   Bluetooth on, Android's Bluetooth service crashes several times on every cold boot before
   giving up. Turning it back on in Android's settings brings that back;
4. opens the Play Store, where you sign in with your own Google account.

Shard records this in `state\device.toml` and does not repeat it. If a step fails it warns and
tries again on the next start.

`debloat.txt` (next to `Shard.exe`, created on first setup, yours to edit: one package per line,
`#` for comments) lists YouTube, YouTube Music, Gmail, Maps, Photos, Drive, Calendar, Messages,
Camera, Digital Wellbeing and the Google app. Without the Google app the launcher works normally;
only its search bar no longer does anything. Shard never removes the Play Store, Google Play
services, Google Services Framework, System UI, Settings, Chrome, Android System WebView, the
launcher, the keyboard, the package installer or the permission controller, whatever the file
says: the launcher, keyboard, installer and permission controller are detected on the device.

**Stopping.** With Quick Boot on, Stop saves a snapshot of the running device and the next
start resumes from it. A change made in the last seconds before Stop (Android saves an app
removal about 10 s late) then lives only in that snapshot until the next start. So whenever the
next start could not use the snapshot, Shard makes sure Android saves everything first:

- With Quick Boot off, or when RAM, cores, resolution or GPU mode in `shard.toml` changed while
  the emulator was running, Stop shuts Android down cleanly (up to 30 s, about 10 s here)
  instead of saving a snapshot, and only then ends the emulator.
- When those settings changed while the emulator was stopped with a snapshot (or Quick Boot was
  turned off), the next start first resumes the snapshot with the settings it was made with,
  shuts Android down cleanly, and then cold boots with the new settings. This adds about 15 s
  to that one start.

If the snapshot cannot be resumed (it is damaged, or the emulator crashed while saving it), Shard
warns and starts with the new settings; changes made just before the last stop can then be lost.

**Wipe data** resets the device to a fresh Android: everything installed and every account on it
is erased. The next start takes as long as the very first one, and the steps above run again.

## Portability and the documented exceptions

Shard never writes the registry, environment variables, the desktop, the Start menu or
`%APPDATA%`/`%LOCALAPPDATA%` itself, installs nothing and creates no shortcuts. Every emulator
and adb process gets its own environment pointing into the Shard folder (`ANDROID_SDK_ROOT`,
`ANDROID_AVD_HOME`, `ANDROID_USER_HOME`, `ANDROID_EMULATOR_HOME`, `TEMP`/`TMP`, adb server port
5038 so another SDK's adb on 5037 is never touched). On top of that Shard turns off emulator
usage statistics and crash reports (`-no-metrics`, `-crash-report-mode disabled`, no crash
handler), pre-seeds the emulator's update check so it never contacts Google, sends screenshots
to `logs\` (`-save-path`; without it the emulator probes the Desktop), and turns off netsim.
adb's mDNS discovery is off too (`ADB_MDNS=0`). Without it, adb binds UDP 5353 on every
network interface, Windows Firewall asks about adb.exe, and when nobody answers it silently
stores Block rules for that adb.exe path. With it, adb opens no UDP socket, and the emulator's
and adb's TCP ports (console, adb and the emulator's internal ones) listen on loopback only. The emulator's own UDP
sockets carry Android's network traffic (DNS and so on) out through Windows, like any app's.
`verify` checks this: it lists any firewall rules for programs inside the Shard folder, and any
TCP listener beyond `127.0.0.1` or adb UDP socket while the emulator runs.

Some things are outside Shard's control:

1. **Items placed temporarily while Shard runs.** adb and the emulator insist on these
   locations and no variable or flag redirects them:
   - `%USERPROFILE%\.android\adbkey` and `adbkey.pub` (adb's key)
   - `%USERPROFILE%\.emulator_console_auth_token` (emulator console)
   - `HKCU\Software\Android Open Source Project\Emulator` (the emulator window's settings)

   Shard keeps its own copies in `home\profile\` and `state\`. Before a start it puts any of these
   that do not already exist in place and records that in `state\placed.toml`; after the
   emulator and adb stop it copies changes back and removes exactly what it placed. Items that
   already existed (from Android Studio, say) are used and never modified or removed. Keeping
   the adb key identical across runs keeps the device authorised. If Shard crashes, the next
   start finishes the cleanup. While the emulator is left running after closing Shard, the items
   stay until Shard stops it.
2. **Windows Hypervisor Platform** has to be turned on by you (see Requirements).
3. **GPU drivers** keep their own shader caches (e.g. `%LOCALAPPDATA%\NVIDIA`), as for any 3D app.
4. *(Milestone 4, opt-in)* switching the power plan while playing, restored afterwards.

Shard does not root the device, bypass Play Integrity or spoof the device, never signs into a
Google account and never installs games for you. It does not bundle or redistribute any of
Google's files: they are downloaded from Google on your PC, under Google's terms.

## Building

Stable Rust, edition 2021, the GNU toolchain (`x86_64-pc-windows-gnu`). HTTPS uses Windows'
own TLS (Schannel, via `native-tls`) so no C compiler or OpenSSL is needed.

```
cargo build --release
cargo test
```

## License

MIT, see [LICENSE](LICENSE). The Android Emulator, platform-tools and system images are Google's
and remain under Google's Android SDK license terms.
