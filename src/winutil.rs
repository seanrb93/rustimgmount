//! Elevation (UAC) and Explorer context-menu registration.

use crate::vdisk::wide;
use anyhow::{bail, Result};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub fn is_elevated() -> bool {
    unsafe {
        let mut tok: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return false;
        }
        let mut el = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret = 0u32;
        let ok = GetTokenInformation(
            tok,
            TokenElevation,
            &mut el as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        );
        CloseHandle(tok);
        ok != 0 && el.TokenIsElevated != 0
    }
}

/// Quote one argument per the MSVC command-line parsing rules.
pub fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in a.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// Set when running hidden under rustimgmountw.exe (no console window).
pub static QUIET_UI: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The windowless launcher next to this exe, if present.
pub fn launcher() -> Option<std::path::PathBuf> {
    let p = std::env::current_exe().ok()?.parent()?.join("rustimgmountw.exe");
    p.exists().then_some(p)
}

/// The command Explorer runs for `verb` (mount/unmount) on "%1": through the windowless
/// launcher when it is installed, otherwise through a console window that pauses.
fn ui_command(verb: &str) -> Result<String> {
    Ok(match launcher() {
        Some(w) => format!("\"{}\" {verb} \"%1\"{}", w.to_string_lossy(), if verb == "mount" { " --open" } else { "" }),
        None => format!("\"{}\" {verb} \"%1\" --pause{}", std::env::current_exe()?.to_string_lossy(), if verb == "mount" { " --open" } else { "" }),
    })
}

/// Re-run this executable elevated with `args`, wait for it, and return its exit code.
/// When running hidden, the elevated copy runs hidden too (through the launcher), and
/// `--pause` is dropped: errors then appear in the launcher's dialog.
pub fn run_elevated(args: &[String]) -> Result<u32> {
    let quiet = QUIET_UI.load(std::sync::atomic::Ordering::Relaxed);
    let (exe, args): (std::path::PathBuf, Vec<String>) = match (quiet, launcher()) {
        (true, Some(w)) => (w, args.iter().filter(|a| *a != "--pause").cloned().collect()),
        _ => (std::env::current_exe()?, args.to_vec()),
    };
    let params = args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ");
    let verb = wide("runas");
    let file = wide(&exe);
    let params_w = wide(&params);
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params_w.as_ptr();
    info.nShow = SW_SHOWNORMAL;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            bail!("administrator permission was declined");
        }
        bail!("could not start elevated process: {e}");
    }
    if info.hProcess.is_null() {
        return Ok(0);
    }
    let mut code = 0u32;
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    Ok(code)
}

// ---------------------------------------------------------------- Windows 11 menu

/// The DLL with the Windows 11 menu entry (built from the `shellext` folder).
pub const SHELL_DLL: &str = "rustimgmount_shell.dll";
/// Name of the package that tells Explorer about it.
const MENU_PACKAGE: &str = "rustimgmount.ShellMenu";
/// Must match CLSID_MENU in shellext/src/lib.rs.
const MENU_CLSID: &str = "7b3f5e2a-9c41-4d6b-a8e3-52c1d0f4b916";

fn powershell(script: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x0800_0000)
        .output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("{}", err.lines().find(|l| !l.trim().is_empty()).unwrap_or("PowerShell failed").trim().to_string());
    }
    Ok(())
}

/// Put "rustimgmount" into Windows 11's main right-click menu. Windows only lists
/// commands of packaged apps there, so this registers a small package whose content is
/// the install folder itself ("sparse package"). It is unsigned, which Windows accepts
/// only with Developer Mode on.
pub fn register_modern_menu(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir.join("assets"))?;
    for (name, data) in [
        ("logo44.png", &include_bytes!("../assets/logo44.png")[..]),
        ("logo150.png", &include_bytes!("../assets/logo150.png")[..]),
        ("logo50.png", &include_bytes!("../assets/logo50.png")[..]),
    ] {
        std::fs::write(dir.join("assets").join(name), data)?;
    }
    // Only the image file types. Windows takes no notice of "Directory" or "*" from this
    // package (found by trying: the entries are accepted and then never offered), and
    // rejects "Drive" outright, so folders and drives have ordinary registry entries.
    let types: String = SHELL_EXTS
        .iter()
        .map(|e| format!("            <desktop5:ItemType Type=\".{e}\"><desktop5:Verb Id=\"RustImgMount\" Clsid=\"{MENU_CLSID}\" /></desktop5:ItemType>\n"))
        .collect();
    let manifest = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
  xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10"
  xmlns:uap10="http://schemas.microsoft.com/appx/manifest/uap/windows10/10"
  xmlns:rescap="http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities"
  xmlns:desktop4="http://schemas.microsoft.com/appx/manifest/desktop/windows10/4"
  xmlns:desktop5="http://schemas.microsoft.com/appx/manifest/desktop/windows10/5"
  xmlns:com="http://schemas.microsoft.com/appx/manifest/com/windows10"
  IgnorableNamespaces="uap uap10 rescap desktop4 desktop5 com">
  <Identity Name="{MENU_PACKAGE}" Publisher="CN=rustimgmount" Version="1.0.0.0" ProcessorArchitecture="x64" />
  <Properties>
    <DisplayName>rustimgmount</DisplayName>
    <PublisherDisplayName>rustimgmount</PublisherDisplayName>
    <Logo>assets\logo50.png</Logo>
    <uap10:AllowExternalContent>true</uap10:AllowExternalContent>
  </Properties>
  <Resources><Resource Language="en-us" /></Resources>
  <Dependencies>
    <TargetDeviceFamily Name="Windows.Desktop" MinVersion="10.0.22000.0" MaxVersionTested="10.0.26100.0" />
  </Dependencies>
  <Capabilities>
    <rescap:Capability Name="runFullTrust" />
    <rescap:Capability Name="unvirtualizedResources" />
  </Capabilities>
  <Applications>
    <Application Id="App" Executable="rustimgmountw.exe" uap10:TrustLevel="mediumIL" uap10:RuntimeBehavior="win32App">
      <uap:VisualElements AppListEntry="none" DisplayName="rustimgmount" Description="Mount and convert disk images"
        BackgroundColor="transparent" Square150x150Logo="assets\logo150.png" Square44x44Logo="assets\logo44.png" />
      <Extensions>
        <desktop4:Extension Category="windows.fileExplorerContextMenus">
          <desktop4:FileExplorerContextMenus>
{types}          </desktop4:FileExplorerContextMenus>
        </desktop4:Extension>
        <com:Extension Category="windows.comServer">
          <com:ComServer>
            <com:SurrogateServer DisplayName="rustimgmount menu">
              <com:Class Id="{MENU_CLSID}" Path="{SHELL_DLL}" ThreadingModel="STA" />
            </com:SurrogateServer>
          </com:ComServer>
        </com:Extension>
      </Extensions>
    </Application>
  </Applications>
</Package>
"#
    );
    let file = dir.join("AppxManifest.xml");
    std::fs::write(&file, manifest)?;
    let q = |p: &std::path::Path| p.display().to_string().replace('\'', "''");
    powershell(&format!(
        "$ErrorActionPreference='Stop'; Get-AppxPackage -Name '{MENU_PACKAGE}' | Remove-AppxPackage; Add-AppxPackage -Register '{}' -ExternalLocation '{}'",
        q(&file),
        q(dir)
    ))
}

const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\rustimgmount";

/// List rustimgmount in Settings > Apps > Installed apps, so it can be uninstalled from
/// there. Only for copies set up with `rustimgmount setup`: the installer adds its own
/// entry (and its own uninstaller, unins000.exe).
pub fn register_uninstaller(dir: &std::path::Path) -> Result<()> {
    if dir.join("unins000.exe").exists() {
        reg_delete_tree(HKEY_CURRENT_USER, UNINSTALL_KEY);
        return Ok(());
    }
    let exe = dir.join("rustimgmount.exe");
    // Through the windowless companion when it is there, so no console window flashes up.
    let w = dir.join("rustimgmountw.exe");
    let runner = if w.exists() { &w } else { &exe };
    for (name, value) in [
        ("DisplayName", "rustimgmount".to_string()),
        ("DisplayVersion", env!("CARGO_PKG_VERSION").to_string()),
        ("Publisher", "rustimgmount".to_string()),
        ("DisplayIcon", exe.display().to_string()),
        ("InstallLocation", dir.display().to_string()),
        ("URLInfoAbout", "https://github.com/seanrb93/rustimgmount".to_string()),
        ("UninstallString", format!("\"{}\" setup --remove", runner.display())),
    ] {
        reg_set(HKEY_CURRENT_USER, UNINSTALL_KEY, Some(name), &value)?;
    }
    Ok(())
}

pub fn unregister_uninstaller() {
    reg_delete_tree(HKEY_CURRENT_USER, UNINSTALL_KEY);
}

/// Is the tray app running? (It holds a named mutex while it is.)
pub fn tray_running() -> bool {
    use windows_sys::Win32::System::Threading::OpenMutexW;
    let name = wide("Local\\rustimgmount-tray");
    let h = unsafe { OpenMutexW(0x0010_0000, 0, name.as_ptr()) }; // SYNCHRONIZE
    if h.is_null() {
        return false;
    }
    unsafe { CloseHandle(h) };
    true
}

/// The user's Desktop folder, wherever it is (OneDrive's backup moves it out of the profile).
pub fn desktop_dir() -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Desktop, SHGetKnownFolderPath};
    let mut path: *mut u16 = null_mut();
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_Desktop, 0, null_mut(), &mut path) };
    if hr != 0 || path.is_null() {
        return None;
    }
    let dir = unsafe {
        let len = (0..).take_while(|&i| *path.add(i) != 0).count();
        let dir = std::ffi::OsString::from_wide(std::slice::from_raw_parts(path, len));
        windows_sys::Win32::System::Com::CoTaskMemFree(path as _);
        dir
    };
    Some(std::path::PathBuf::from(dir)).filter(|d| d.is_dir())
}

/// Ask the running tray app to close, and wait a moment for it to do so.
pub fn stop_tray() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowExW, FindWindowW, PostMessageW, HWND_MESSAGE, WM_CLOSE};
    let class = wide("rustimgmount_tray");
    unsafe {
        let mut hwnd = FindWindowW(class.as_ptr(), null());
        if hwnd.is_null() {
            hwnd = FindWindowExW(HWND_MESSAGE, null_mut(), class.as_ptr(), null());
        }
        if hwnd.is_null() {
            return;
        }
        PostMessageW(hwnd, WM_CLOSE, 0, 0);
    }
    for _ in 0..40 {
        if !tray_running() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Start the tray app from `dir`, detached.
pub fn start_tray(dir: &std::path::Path) {
    let _ = spawn_detached_exe(&dir.join("rustimgmountw.exe"), &["--tray".to_string()]);
}

pub fn unregister_modern_menu() {
    let _ = powershell(&format!("Get-AppxPackage -Name '{MENU_PACKAGE}' | Remove-AppxPackage"));
}

/// Move files to the Recycle Bin. Windows asks first if one can't go there (too big, or
/// on a drive without a bin) and would be deleted for good instead.
pub fn recycle(files: &[std::path::PathBuf]) -> Result<()> {
    use windows_sys::Win32::UI::Shell::{SHFileOperationW, FO_DELETE, SHFILEOPSTRUCTW};
    const FOF_SILENT: u16 = 0x4;
    const FOF_NOCONFIRMATION: u16 = 0x10;
    const FOF_ALLOWUNDO: u16 = 0x40;
    const FOF_NOERRORUI: u16 = 0x400;
    const FOF_WANTNUKEWARNING: u16 = 0x4000;
    // The list is NUL-separated and ends with an extra NUL.
    let mut from: Vec<u16> = Vec::new();
    for f in files {
        from.extend(f.to_string_lossy().encode_utf16());
        from.push(0);
    }
    from.push(0);
    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.wFunc = FO_DELETE;
    op.pFrom = from.as_ptr();
    op.fFlags = FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_WANTNUKEWARNING;
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 || op.fAnyOperationsAborted != 0 {
        bail!("the converted image is fine, but the original could not be moved to the Recycle Bin (code {rc})");
    }
    Ok(())
}

/// Is a process with this id still running?
pub fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return false;
    }
    let mut code = 0u32;
    let ok = unsafe { GetExitCodeProcess(h, &mut code) };
    unsafe { CloseHandle(h) };
    ok != 0 && code == 259 // STILL_ACTIVE
}

/// When the process with this id started, as Windows counts time. `None` if it is gone.
pub fn process_started(pid: u32) -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    let ok = unsafe { GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user) };
    unsafe { CloseHandle(h) };
    (ok != 0).then(|| (created.dwHighDateTime as u64) << 32 | created.dwLowDateTime as u64)
}

/// Start this executable in the background with `args`: no window, no console, and no
/// inherited handles (so it can't keep a caller's output pipe open after we exit).
pub fn spawn_detached(args: &[String]) -> Result<()> {
    spawn_detached_exe(&std::env::current_exe()?, args)
}

/// Start `exe` on its own: no window, and none of this process's handles. (A child that
/// inherited them would keep this process's output pipe open for as long as it runs, and
/// whoever reads that output would wait just as long.)
pub fn spawn_detached_exe(exe: &std::path::Path, args: &[String]) -> Result<()> {
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS, PROCESS_INFORMATION, STARTUPINFOW,
    };
    let mut cmd = quote_arg(&exe.to_string_lossy());
    for a in args {
        cmd.push(' ');
        cmd.push_str(&quote_arg(a));
    }
    let mut cmd_w = wide(&cmd);
    let app = wide(exe);
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(
            app.as_ptr(),
            cmd_w.as_mut_ptr(),
            null(),
            null(),
            0, // bInheritHandles = FALSE
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
            null(),
            null(),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        bail!("could not start background helper: {}", std::io::Error::last_os_error());
    }
    unsafe {
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
    }
    Ok(())
}

// ---------------------------------------------------------------- Explorer integration

pub const SHELL_EXTS: &[&str] = &[
    "iso", "img", "ima", "bin", "cue", "nrg", "mdf", "mds", "ccd", "cdi", "b5t", "b6t", "pdi", "vhd", "vhdx", "avhd",
    "avhdx", "vmdk", "vdi", "qcow2", "hdd", "chd", "cso", "zso", "dax", "gdi", "ecm", "isz", "pbp", "m3u", "mdx", "hdi", "fdi", "nhd", "st", "msa", "e01",
    "ex01", "001", "d88", "d77", "d98", "flp", "vfd", "dsk", "raw", "dd",
];

fn reg_set(root: HKEY, key: &str, name: Option<&str>, value: &str) -> Result<()> {
    let k = wide(key);
    let mut h: HKEY = null_mut();
    let rc = unsafe {
        RegCreateKeyExW(root, k.as_ptr(), 0, null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, null(), &mut h, null_mut())
    };
    if rc != ERROR_SUCCESS {
        bail!("RegCreateKeyEx({key}) failed: {}", std::io::Error::from_raw_os_error(rc as i32));
    }
    let n = name.map(wide);
    let v = wide(value);
    let rc = unsafe {
        RegSetValueExW(
            h,
            n.as_ref().map(|n| n.as_ptr()).unwrap_or(null()),
            0,
            REG_SZ,
            v.as_ptr() as *const u8,
            (v.len() * 2) as u32,
        )
    };
    unsafe { RegCloseKey(h) };
    if rc != ERROR_SUCCESS {
        bail!("RegSetValueEx({key}) failed: {}", std::io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

fn reg_delete_tree(root: HKEY, key: &str) {
    let k = wide(key);
    unsafe { RegDeleteTreeW(root, k.as_ptr()) };
    unsafe { RegDeleteKeyW(root, k.as_ptr()) };
}

const VERBS: [(&str, &str, &str); 2] = [
    ("rustimgmount.mount", "Mount with rustimgmount", "mount"),
    ("rustimgmount.unmount", "Unmount (rustimgmount)", "unmount"),
];

fn reg_get(root: HKEY, key: &str) -> Option<String> {
    reg_get_value(root, key, None)
}

fn reg_get_value(root: HKEY, key: &str, name: Option<&str>) -> Option<String> {
    let k = wide(key);
    let n = name.map(wide);
    let mut buf = vec![0u16; 2048];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            root,
            k.as_ptr(),
            n.as_ref().map(|n| n.as_ptr()).unwrap_or(null()),
            RRF_RT_REG_SZ,
            null_mut(),
            buf.as_mut_ptr() as *mut _,
            &mut size,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let n = (size as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// If the right-click menu or the file associations point at an exe that no longer
/// exists (the program was moved), re-register them for this exe. Returns true if repaired.
pub fn repair_shell_if_moved() -> bool {
    let moved = |key: &str| -> bool {
        let Some(cmd) = reg_get(HKEY_CURRENT_USER, key) else { return false };
        let Some(old) = cmd.strip_prefix('"').and_then(|r| r.split('"').next()) else { return false };
        let Ok(exe) = std::env::current_exe() else { return false };
        // Still valid, or a different copy that still exists: leave it.
        !(std::path::Path::new(old).exists() || crate::state::same_path(std::path::Path::new(old), &exe))
    };
    // A menu installed by an earlier version of this copy has no "Convert to" yet: add it.
    let mount_cmd = reg_get(HKEY_CURRENT_USER, &format!(r"Software\Classes\SystemFileAssociations\.iso\shell\{}\command", VERBS[0].0));
    let ours = mount_cmd.as_deref().and_then(|c| c.strip_prefix('"')).and_then(|r| r.split('"').next()).is_some_and(|old| {
        let old = std::path::Path::new(old).parent().map(|p| p.to_owned());
        let here = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_owned()));
        matches!((old, here), (Some(a), Some(b)) if crate::state::same_path(&a, &b))
    });
    if ours && reg_get_value(HKEY_CURRENT_USER, &format!(r"Software\Classes\SystemFileAssociations\.iso\shell\{CONVERT_ID}"), Some("MenuVersion")).as_deref() != Some(MENU_VERSION) {
        let _ = install_shell();
    }
    let mut repaired = false;
    if moved(&format!(r"Software\Classes\SystemFileAssociations\.iso\shell\{}\command", VERBS[0].0)) {
        repaired |= install_shell().is_ok();
    }
    if moved(&format!(r"Software\Classes\{}\shell\open\command", progid("iso"))) || moved(&format!(r"Software\Classes\{OLD_PROGID}\shell\open\command")) {
        repaired |= register_progid().is_ok();
    }
    repaired
}

// ---------------------------------------------------------------- setup

/// Where `setup` installs the programs.
pub fn install_dir() -> std::path::PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Programs")
        .join("rustimgmount")
}

fn user_path() -> String {
    let k = wide("Environment");
    let n = wide("Path");
    let mut buf = vec![0u16; 32768];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            k.as_ptr(),
            n.as_ptr(),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
            null_mut(),
            buf.as_mut_ptr() as *mut _,
            &mut size,
        )
    };
    if rc != ERROR_SUCCESS {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..(size as usize / 2).saturating_sub(1)])
}

fn set_user_path(value: &str) -> Result<()> {
    let k = wide("Environment");
    let mut h: HKEY = null_mut();
    let rc = unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, k.as_ptr(), 0, null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, null(), &mut h, null_mut()) };
    if rc != ERROR_SUCCESS {
        bail!("cannot open the user environment: {}", std::io::Error::from_raw_os_error(rc as i32));
    }
    let n = wide("Path");
    let v = wide(value);
    let rc = unsafe { RegSetValueExW(h, n.as_ptr(), 0, REG_EXPAND_SZ, v.as_ptr() as *const u8, (v.len() * 2) as u32) };
    unsafe { RegCloseKey(h) };
    if rc != ERROR_SUCCESS {
        bail!("cannot update PATH: {}", std::io::Error::from_raw_os_error(rc as i32));
    }
    // Tell Explorer (and new terminals) that the environment changed.
    use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE};
    let env = wide("Environment");
    unsafe { SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, env.as_ptr() as isize, SMTO_ABORTIFHUNG, 3000, null_mut()) };
    Ok(())
}

fn same_dir(a: &str, b: &std::path::Path) -> bool {
    crate::state::same_path(std::path::Path::new(a.trim_end_matches('\\')), std::path::Path::new(b.to_string_lossy().trim_end_matches('\\')))
}

/// Add `dir` to the user PATH. Returns false if it was already there.
pub fn add_to_path(dir: &std::path::Path) -> Result<bool> {
    let cur = user_path();
    if cur.split(';').any(|p| same_dir(p, dir)) {
        return Ok(false);
    }
    let new = if cur.is_empty() || cur.ends_with(';') { format!("{cur}{}", dir.display()) } else { format!("{cur};{}", dir.display()) };
    set_user_path(&new)?;
    Ok(true)
}

pub fn remove_from_path(dir: &std::path::Path) -> Result<()> {
    let cur = user_path();
    let kept: Vec<&str> = cur.split(';').filter(|p| !p.is_empty() && !same_dir(p, dir)).collect();
    if kept.len() != cur.split(';').filter(|p| !p.is_empty()).count() {
        set_user_path(&kept.join(";"))?;
    }
    Ok(())
}

/// Start Menu shortcut for the tray app.
pub fn start_menu_shortcut() -> std::path::PathBuf {
    std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(r"Microsoft\Windows\Start Menu\Programs\rustimgmount.lnk")
}

/// Write a shortcut (.lnk) with the shell's own ShellLink object.
pub fn create_shortcut(lnk: &std::path::Path, target: &std::path::Path, args: &str, description: &str) -> Result<()> {
    use std::ffi::c_void;
    use windows_sys::core::{GUID, HRESULT};
    use windows_sys::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    // The parts of IShellLinkW and IPersistFile used here, in their vtable order.
    #[repr(C)]
    struct ShellLinkVtbl {
        query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        get_path: usize,
        get_id_list: usize,
        set_id_list: usize,
        get_description: usize,
        set_description: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        get_working_directory: usize,
        set_working_directory: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        get_arguments: usize,
        set_arguments: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        get_hotkey: usize,
        set_hotkey: usize,
        get_show_cmd: usize,
        set_show_cmd: usize,
        get_icon_location: usize,
        set_icon_location: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT,
        set_relative_path: usize,
        resolve: usize,
        set_path: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    }
    #[repr(C)]
    struct PersistFileVtbl {
        query_interface: usize,
        add_ref: usize,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        get_class_id: usize,
        is_dirty: usize,
        load: usize,
        save: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT,
    }
    const CLSID_SHELL_LINK: GUID = GUID::from_u128(0x00021401_0000_0000_c000_000000000046);
    const IID_ISHELL_LINK_W: GUID = GUID::from_u128(0x000214f9_0000_0000_c000_000000000046);
    const IID_IPERSIST_FILE: GUID = GUID::from_u128(0x0000010b_0000_0000_c000_000000000046);
    let check = |hr: HRESULT, what: &str| if hr < 0 { bail!("could not create the Start Menu shortcut ({what} failed: 0x{hr:08x})") } else { Ok(()) };
    unsafe {
        let init = CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
        let result = (|| -> Result<()> {
            let mut link: *mut c_void = null_mut();
            check(CoCreateInstance(&CLSID_SHELL_LINK, null_mut(), CLSCTX_INPROC_SERVER, &IID_ISHELL_LINK_W, &mut link), "ShellLink")?;
            let lv = &**(link as *mut *const ShellLinkVtbl);
            let dir = target.parent().map(|p| p.to_path_buf()).unwrap_or_default();
            let r = (|| -> Result<()> {
                check((lv.set_path)(link, wide(target).as_ptr()), "SetPath")?;
                check((lv.set_arguments)(link, wide(args).as_ptr()), "SetArguments")?;
                check((lv.set_description)(link, wide(description).as_ptr()), "SetDescription")?;
                check((lv.set_working_directory)(link, wide(&dir).as_ptr()), "SetWorkingDirectory")?;
                check((lv.set_icon_location)(link, wide(target).as_ptr(), 0), "SetIconLocation")?;
                let mut file: *mut c_void = null_mut();
                check((lv.query_interface)(link, &IID_IPERSIST_FILE, &mut file), "IPersistFile")?;
                let fv = &**(file as *mut *const PersistFileVtbl);
                let saved = (fv.save)(file, wide(lnk).as_ptr(), 1);
                (fv.release)(file);
                check(saved, "Save")
            })();
            (lv.release)(link);
            r
        })();
        if init >= 0 {
            CoUninitialize();
        }
        result
    }
}

/// Start the tray app when the user signs in (or stop doing so).
pub fn set_autostart(on: bool) -> Result<()> {
    let key = r"Software\Microsoft\Windows\CurrentVersion\Run";
    if on {
        let w = launcher().ok_or_else(|| anyhow::anyhow!("rustimgmountw.exe not found"))?;
        reg_set(HKEY_CURRENT_USER, key, Some("rustimgmount"), &format!("\"{}\" --tray", w.display()))
    } else {
        reg_delete_value(HKEY_CURRENT_USER, key, "rustimgmount");
        Ok(())
    }
}

pub fn autostart_enabled() -> bool {
    reg_get_value(HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Run", Some("rustimgmount")).is_some()
}

// ---------------------------------------------------------------- file associations

/// Until version 0.1.1 every extension pointed at this one file type.
const OLD_PROGID: &str = "rustimgmount.image";

/// The file type registered for an extension. Each format has its own, so that each can
/// have its own icon and description in Explorer.
fn progid(ext: &str) -> String {
    format!("rustimgmount.file.{ext}")
}

fn is_our_progid(id: &str) -> bool {
    id == OLD_PROGID || id.starts_with("rustimgmount.file.")
}

// FORMAT_ICONS: which icon resource belongs to which extension (written by build.rs).
include!(concat!(env!("OUT_DIR"), "/format_icons.rs"));

/// What Explorer shows in its "Type" column.
fn type_name(ext: &str) -> String {
    let upper = ext.to_uppercase();
    match ext {
        "iso" | "bin" | "nrg" | "mdf" | "mdx" | "cdi" | "b5t" | "b6t" | "pdi" | "isz" | "chd" | "cso" | "zso" | "dax" | "ecm" | "pbp" => format!("{upper} disc image"),
        "cue" | "mds" | "ccd" | "gdi" => format!("{upper} disc image sheet"),
        "vhd" | "vhdx" | "avhd" | "avhdx" | "vmdk" | "vdi" | "qcow2" | "hdd" => format!("{upper} virtual disk"),
        "hdi" | "fdi" | "nhd" => format!("{upper} PC-98 disk image"),
        "st" | "msa" => format!("{upper} Atari ST disk image"),
        "d88" | "d77" | "d98" | "flp" | "vfd" => format!("{upper} floppy image"),
        "e01" | "ex01" => format!("{upper} forensic disk image"),
        _ => format!("{upper} disk image"),
    }
}
const APP_KEY: &str = r"Software\rustimgmount";

/// Extensions double-click should mount. `.m3u` is left out on purpose: music players use
/// it for playlists. (Archives like .zip/.7z/.gz are never associated for the same reason.)
pub fn assoc_exts() -> Vec<&'static str> {
    SHELL_EXTS.iter().copied().filter(|e| *e != "m3u").collect()
}

/// The user's choice between one icon per format (the default) and the rustimgmount icon
/// for every image type. Remembered, so that re-registering later keeps it.
pub fn set_plain_icons(plain: bool) -> Result<()> {
    if plain {
        reg_set(HKEY_CURRENT_USER, APP_KEY, Some("PlainIcons"), "1")
    } else {
        reg_delete_value(HKEY_CURRENT_USER, APP_KEY, "PlainIcons");
        Ok(())
    }
}

fn plain_icons() -> bool {
    reg_get_value(HKEY_CURRENT_USER, APP_KEY, Some("PlainIcons")).as_deref() == Some("1")
}

fn register_progid() -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe_s = exe.to_string_lossy();
    reg_delete_tree(HKEY_CURRENT_USER, &format!(r"Software\Classes\{OLD_PROGID}"));
    let plain = plain_icons();
    let open = ui_command("mount")?;
    // Show up in Settings > Default apps, so the user can pick it where Windows requires that.
    let caps = format!(r"{APP_KEY}\Capabilities");
    reg_set(HKEY_CURRENT_USER, &caps, Some("ApplicationName"), "rustimgmount")?;
    reg_set(HKEY_CURRENT_USER, &caps, Some("ApplicationDescription"), "Mounts disk and disc images as drives")?;
    reg_set(HKEY_CURRENT_USER, &caps, Some("ApplicationIcon"), &format!("{exe_s},0"))?;
    for ext in assoc_exts() {
        let id = progid(ext);
        let base = format!(r"Software\Classes\{id}");
        let name = type_name(ext);
        reg_set(HKEY_CURRENT_USER, &base, None, &name)?;
        reg_set(HKEY_CURRENT_USER, &base, Some("FriendlyTypeName"), &name)?;
        // The format's own icon; the program's if there is none for it, or if the user
        // asked for one icon for everything.
        let icon = match FORMAT_ICONS.iter().find(|(e, _)| *e == ext).filter(|_| !plain) {
            Some((_, res)) => format!("{exe_s},-{res}"),
            None => format!("{exe_s},0"),
        };
        reg_set(HKEY_CURRENT_USER, &format!(r"{base}\DefaultIcon"), None, &icon)?;
        reg_set(HKEY_CURRENT_USER, &format!(r"{base}\shell"), None, "open")?;
        reg_set(HKEY_CURRENT_USER, &format!(r"{base}\shell\open"), None, "Mount")?;
        reg_set(HKEY_CURRENT_USER, &format!(r"{base}\shell\open\command"), None, &open)?;
        reg_set(HKEY_CURRENT_USER, &format!(r"{caps}\FileAssociations"), Some(&format!(".{ext}")), &id)?;
    }
    reg_set(HKEY_CURRENT_USER, r"Software\RegisteredApplications", Some("rustimgmount"), &caps)?;
    Ok(())
}

fn notify_assoc_changed() {
    use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, null(), null()) };
}

/// The command Windows will run when a file with this extension is double-clicked.
pub fn effective_open_command(ext: &str) -> Option<String> {
    use windows_sys::Win32::UI::Shell::{AssocQueryStringW, ASSOCF_NONE, ASSOCSTR_COMMAND};
    let e = wide(format!(".{ext}"));
    let verb = wide("open");
    let mut buf = vec![0u16; 2048];
    let mut len = buf.len() as u32;
    let hr = unsafe { AssocQueryStringW(ASSOCF_NONE, ASSOCSTR_COMMAND, e.as_ptr(), verb.as_ptr(), buf.as_mut_ptr(), &mut len) };
    (hr == 0).then(|| String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)]))
}

pub struct AssocReport {
    /// Extensions that now mount on double-click.
    pub ours: Vec<&'static str>,
    /// Extensions where an app the user chose still wins: (ext, its command).
    pub others: Vec<(&'static str, String)>,
}

/// Make rustimgmount the per-user handler for every supported extension. Any per-user
/// default it replaces is saved so `unassociate` can restore it.
pub fn associate() -> Result<AssocReport> {
    register_progid()?;
    let saved = format!(r"{APP_KEY}\PreviousDefaults");
    for ext in assoc_exts() {
        let key = format!(r"Software\Classes\.{ext}");
        match reg_get(HKEY_CURRENT_USER, &key) {
            Some(prev) if !is_our_progid(&prev) && !prev.is_empty() => {
                if reg_get_value(HKEY_CURRENT_USER, &saved, Some(&format!(".{ext}"))).is_none() {
                    reg_set(HKEY_CURRENT_USER, &saved, Some(&format!(".{ext}")), &prev)?;
                }
            }
            _ => {}
        }
        reg_set(HKEY_CURRENT_USER, &key, None, &progid(ext))?;
        reg_delete_value(HKEY_CURRENT_USER, &format!(r"{key}\OpenWithProgids"), OLD_PROGID);
        reg_set(HKEY_CURRENT_USER, &format!(r"{key}\OpenWithProgids"), Some(&progid(ext)), "")?;
    }
    notify_assoc_changed();
    // Ours if the command runs rustimgmount.exe or rustimgmountw.exe from this folder.
    let exe = std::env::current_exe()?;
    let ours_names: Vec<String> = [exe.clone(), exe.with_file_name("rustimgmountw.exe")]
        .iter()
        .map(|p| p.to_string_lossy().to_lowercase())
        .collect();
    let mut report = AssocReport { ours: Vec::new(), others: Vec::new() };
    for ext in assoc_exts() {
        match effective_open_command(ext) {
            Some(c) if ours_names.iter().any(|n| c.to_lowercase().contains(n.as_str())) => report.ours.push(ext),
            Some(c) => report.others.push((ext, c)),
            None => report.others.push((ext, String::from("(nothing)"))),
        }
    }
    // Where Windows did not hand the type over (another app can open it too, and the choice
    // is the user's), take the claim back: left in place it would only blank the type's
    // icon and show our internal name as its description until the user chooses. The type
    // goes back to what it was, and rustimgmount stays on its "Open with" list and in
    // Settings > Default apps, where it can be picked.
    for (ext, _) in &report.others {
        let key = format!(r"Software\Classes\.{ext}");
        match reg_get_value(HKEY_CURRENT_USER, &saved, Some(&format!(".{ext}"))) {
            Some(prev) => reg_set(HKEY_CURRENT_USER, &key, None, &prev)?,
            None => reg_delete_value(HKEY_CURRENT_USER, &key, ""),
        }
    }
    if !report.others.is_empty() {
        notify_assoc_changed();
    }
    Ok(report)
}

/// Undo `associate`: restore saved defaults, remove rustimgmount's file type.
pub fn unassociate() -> Result<()> {
    let saved = format!(r"{APP_KEY}\PreviousDefaults");
    for ext in assoc_exts() {
        let key = format!(r"Software\Classes\.{ext}");
        reg_delete_value(HKEY_CURRENT_USER, &format!(r"{key}\OpenWithProgids"), OLD_PROGID);
        reg_delete_value(HKEY_CURRENT_USER, &format!(r"{key}\OpenWithProgids"), &progid(ext));
        reg_delete_tree(HKEY_CURRENT_USER, &format!(r"Software\Classes\{}", progid(ext)));
        if reg_get(HKEY_CURRENT_USER, &key).is_some_and(|p| is_our_progid(&p)) {
            match reg_get_value(HKEY_CURRENT_USER, &saved, Some(&format!(".{ext}"))) {
                Some(prev) => reg_set(HKEY_CURRENT_USER, &key, None, &prev)?,
                None => reg_delete_value(HKEY_CURRENT_USER, &key, ""),
            }
        }
    }
    reg_delete_tree(HKEY_CURRENT_USER, &format!(r"Software\Classes\{OLD_PROGID}"));
    reg_delete_value(HKEY_CURRENT_USER, r"Software\RegisteredApplications", "rustimgmount");
    reg_delete_tree(HKEY_CURRENT_USER, APP_KEY);
    notify_assoc_changed();
    Ok(())
}

fn reg_delete_value(root: HKEY, key: &str, name: &str) {
    let k = wide(key);
    let n = wide(name);
    // An empty name deletes the key's default value.
    unsafe { RegDeleteKeyValueW(root, k.as_ptr(), if name.is_empty() { null() } else { n.as_ptr() }) };
}

/// Open Settings > Default apps on rustimgmount's page.
pub fn open_default_apps_settings() {
    let _ = std::process::Command::new("explorer.exe").arg("ms-settings:defaultapps?registeredAppUser=rustimgmount").spawn();
}

/// Menu entries registered by versions released under the old name, "imgmount".
const LEGACY_VERB_IDS: [&str; 2] = ["imgmount.mount", "imgmount.unmount"];

fn remove_legacy_verbs() {
    for ext in SHELL_EXTS {
        for id in LEGACY_VERB_IDS {
            reg_delete_tree(HKEY_CURRENT_USER, &format!(r"Software\Classes\SystemFileAssociations\.{ext}\shell\{id}"));
        }
    }
}

/// The "Convert to" submenu.
const CONVERT_ID: &str = "rustimgmount.convert";
/// Bumped when the menu's layout changes, so an installed menu gets refreshed.
const MENU_VERSION: &str = "4";
/// The entries on folders and on drives: (key, label, what rustimgmountw is asked to do).
/// The first makes an ISO image straight away; the second opens the Create dialog.
const FOLDER_VERBS: [(&str, &str, &str); 2] =
    [("rustimgmount.createiso", "Create ISO image", "create"), ("rustimgmount.createmore", "Create disc image: more options...", "create-ui")];
const FOLDER_KINDS: [&str; 2] = ["Directory", "Drive"];

/// Formats offered directly under "Convert to" for a file extension. `None` for
/// extensions used for both discs and disks (.img, .raw, ...): those get a single
/// "Convert..." entry that opens the dialog, which looks inside the file first.
fn quick_formats(ext: &str) -> Option<Vec<&'static str>> {
    let mut v = match ext {
        "img" | "raw" | "001" | "e01" | "ex01" => return None,
        "gdi" => vec!["iso", "cso", "chd"],
        "chd" => vec!["iso", "cso", "cue", "gdi"],
        "vhd" | "vhdx" | "avhd" | "avhdx" | "vmdk" | "vdi" | "qcow2" | "hdd" | "hdi" | "fdi" | "nhd" | "st" | "msa" | "d88" | "d77" | "d98"
        | "flp" | "vfd" | "dsk" | "ima" | "dd" => vec!["vhd", "vhdx", "img", "vmdk", "vdi", "qcow2"],
        _ => vec!["iso", "cso", "chd", "cue"],
    };
    v.retain(|t| *t != ext);
    Some(v)
}

fn install_convert_menu(ext: &str, icon: &str) -> Result<()> {
    let base = format!(r"Software\Classes\SystemFileAssociations\.{ext}\shell\{CONVERT_ID}");
    reg_delete_tree(HKEY_CURRENT_USER, &base);
    reg_set(HKEY_CURRENT_USER, &base, Some("Icon"), icon)?;
    reg_set(HKEY_CURRENT_USER, &base, Some("MenuVersion"), MENU_VERSION)?;
    let exe = std::env::current_exe()?;
    let leaf = |key: &str, label: &str, args: &str| -> Result<()> {
        reg_set(HKEY_CURRENT_USER, key, Some("MUIVerb"), label)?;
        // Let the entry show for big selections; each file is handed to one shared queue.
        reg_set(HKEY_CURRENT_USER, key, Some("MultiSelectModel"), "Player")?;
        let cmd = match launcher() {
            Some(w) => format!("\"{}\" {args}", w.to_string_lossy()),
            None => format!("\"{}\" {args} --pause", exe.to_string_lossy()),
        };
        reg_set(HKEY_CURRENT_USER, &format!(r"{key}\command"), None, &cmd)
    };
    let dialog = launcher().is_some(); // the dialog lives in rustimgmountw.exe
    let quick = match (quick_formats(ext), dialog) {
        (None, true) => return leaf(&base, "Convert...", "convert-ui \"%1\""),
        (None, false) => vec!["iso", "cso", "chd", "cue", "vhd", "vhdx", "vmdk", "vdi", "qcow2"],
        (Some(q), _) => q,
    };
    reg_set(HKEY_CURRENT_USER, &base, Some("MUIVerb"), "Convert to")?;
    reg_set(HKEY_CURRENT_USER, &base, Some("SubCommands"), "")?;
    for (i, id) in quick.iter().enumerate() {
        let label = crate::export::Format::parse(id).map(|f| f.label).unwrap_or(id);
        // Explorer orders the entries by key name.
        leaf(&format!(r"{base}\shell\{i:02}{id}"), label, &format!("convert \"%1\" --to {id}"))?;
    }
    if dialog {
        leaf(&format!(r"{base}\shell\99more"), "More formats and options...", "convert-ui \"%1\"")?;
    }
    Ok(())
}

/// "Create ISO image" and the Create dialog on folders and/or drives ("Directory", "Drive").
pub fn install_folder_menu(kinds: &[&str]) -> Result<()> {
    let exe = std::env::current_exe()?;
    for kind in kinds {
        for (id, label, verb) in FOLDER_VERBS {
            // The dialog lives in rustimgmountw.exe.
            if verb == "create-ui" && launcher().is_none() {
                continue;
            }
            let key = format!(r"Software\Classes\{kind}\shell\{id}");
            reg_set(HKEY_CURRENT_USER, &key, Some("MUIVerb"), label)?;
            reg_set(HKEY_CURRENT_USER, &key, Some("Icon"), &format!("{},0", exe.to_string_lossy()))?;
            reg_set(HKEY_CURRENT_USER, &format!(r"{key}\command"), None, &ui_command(verb)?)?;
        }
    }
    Ok(())
}

pub fn uninstall_folder_menu(kinds: &[&str]) {
    for kind in kinds {
        for (id, _, _) in FOLDER_VERBS {
            reg_delete_tree(HKEY_CURRENT_USER, &format!(r"Software\Classes\{kind}\shell\{id}"));
        }
    }
}

pub fn install_shell() -> Result<()> {
    remove_legacy_verbs();
    let exe = std::env::current_exe()?;
    let exe_s = exe.to_string_lossy();
    for ext in SHELL_EXTS {
        install_convert_menu(ext, &format!("{exe_s},0"))?;
        for (id, label, cmd) in VERBS {
            let base = format!(r"Software\Classes\SystemFileAssociations\.{ext}\shell\{id}");
            reg_set(HKEY_CURRENT_USER, &base, Some("MUIVerb"), label)?;
            reg_set(HKEY_CURRENT_USER, &base, Some("Icon"), &format!("{exe_s},0"))?;
            reg_set(HKEY_CURRENT_USER, &format!(r"{base}\command"), None, &ui_command(cmd)?)?;
        }
    }
    install_folder_menu(&FOLDER_KINDS)?;
    Ok(())
}

pub fn uninstall_shell() -> Result<()> {
    remove_legacy_verbs();
    for ext in SHELL_EXTS {
        for id in VERBS.iter().map(|v| v.0).chain([CONVERT_ID]) {
            reg_delete_tree(HKEY_CURRENT_USER, &format!(r"Software\Classes\SystemFileAssociations\.{ext}\shell\{id}"));
        }
    }
    uninstall_folder_menu(&FOLDER_KINDS);
    Ok(())
}
