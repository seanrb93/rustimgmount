//! Notification-area (tray) app: `rustimgmountw --tray`.
//!
//! Left- or right-click the icon for a menu of mounted images (open / unmount each),
//! "Mount image...", "Unmount all", "Start with Windows" and "Exit". It reads the state
//! from `rustimgmount list --json` each time the menu opens, so it's always current, and
//! runs every action through the same hidden runner as double-click (progress and error
//! dialogs included). Only one tray instance runs at a time.

use super::{cli_exe, message_box, run_hidden, wide};
use std::os::windows::process::CommandExt;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST, OPENFILENAMEW};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
const TIMER_REFRESH: usize = 1;
const ID_MOUNT: usize = 10;
const ID_UNMOUNT_ALL: usize = 11;
const ID_AUTOSTART: usize = 12;
const ID_EXIT: usize = 13;
const ID_CONVERT: usize = 14;
const ID_UNINSTALL: usize = 15;
const ID_CREATE: usize = 16;
/// Recently mounted images: ID_RECENT_BASE + index
const ID_RECENT_BASE: usize = 500;
/// Per-image items: ID_IMAGE_BASE + index * 4 + (0 = open, 1 = unmount)
const ID_IMAGE_BASE: usize = 1000;

#[derive(Clone)]
struct Mounted {
    source: String,
    /// Display name when it differs from the source (a disc's audio-tracks drive).
    name: Option<String>,
    mounts: Vec<String>,
    ours: bool,
}

static mut IMAGES: Vec<Mounted> = Vec::new();
static mut RECENT: Vec<String> = Vec::new();

/// Images mounted lately that still exist (rustimgmount.exe keeps the list).
fn recent_images() -> Vec<String> {
    let list: Vec<String> = std::fs::read(super::convertui::app_dir().join("recent.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    list.into_iter().filter(|p| std::path::Path::new(p.rsplit_once('#').map_or(p.as_str(), |(f, _)| f)).exists() || std::path::Path::new(p).exists()).collect()
}
static mut TASKBAR_CREATED: u32 = 0;

/// Run rustimgmount.exe hidden and return its standard output.
fn cli_output(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cli_exe()?).args(args).creation_flags(0x0800_0000).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn mounted_images() -> Vec<Mounted> {
    let Some(json) = cli_output(&["list", "--json"]) else { return Vec::new() };
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str(&json) else { return Vec::new() };
    items
        .iter()
        .map(|v| Mounted {
            source: v["source"].as_str().unwrap_or("").to_string(),
            name: v["name"].as_str().map(String::from),
            mounts: v["mounts"].as_array().map(|a| a.iter().filter_map(|m| m.as_str().map(String::from)).collect()).unwrap_or_default(),
            ours: v["ours"].as_bool().unwrap_or(false),
        })
        .collect()
}

fn short_name(source: &str) -> String {
    let (path, disc) = match source.rsplit_once('#') {
        Some((p, d)) if d.chars().all(|c| c.is_ascii_digit()) && !d.is_empty() => (p, Some(d)),
        _ => (source, None),
    };
    let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string());
    match disc {
        Some(d) => format!("{name} (disc {d})"),
        None => name,
    }
}

unsafe fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid
}

unsafe fn set_tooltip(hwnd: HWND, text: &str) {
    let mut nid = notify_data(hwnd);
    nid.uFlags = NIF_TIP;
    for (i, c) in text.encode_utf16().take(nid.szTip.len() - 1).enumerate() {
        nid.szTip[i] = c;
    }
    Shell_NotifyIconW(NIM_MODIFY, &nid);
}

unsafe fn add_icon(hwnd: HWND) {
    let mut nid = notify_data(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    // The program's own icon, straight from the executable.
    let exe = wide(&std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default());
    let mut small: HICON = null_mut();
    ExtractIconExW(exe.as_ptr(), 0, null_mut(), &mut small, 1);
    nid.hIcon = if small.is_null() { LoadIconW(null_mut(), IDI_APPLICATION) } else { small };
    for (i, c) in "rustimgmount".encode_utf16().enumerate() {
        nid.szTip[i] = c;
    }
    Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn refresh_tooltip(hwnd: HWND) {
    let imgs = mounted_images();
    let text = match imgs.len() {
        0 => "rustimgmount: no images mounted".to_string(),
        1 => format!("rustimgmount: {} mounted", short_name(&imgs[0].source)),
        n => format!("rustimgmount: {n} images mounted"),
    };
    set_tooltip(hwnd, &text);
}

unsafe fn append(menu: HMENU, flags: u32, id: usize, text: &str) {
    let w = wide(text);
    AppendMenuW(menu, flags, id, w.as_ptr());
}

unsafe fn show_menu(hwnd: HWND) {
    let imgs = mounted_images();
    let menu = CreatePopupMenu();
    if imgs.is_empty() {
        append(menu, MF_STRING | MF_GRAYED, 0, "No images mounted");
    }
    for (i, m) in imgs.iter().enumerate() {
        let sub = CreatePopupMenu();
        let letters = if m.mounts.is_empty() { "no drive letter".to_string() } else { m.mounts.join(", ") };
        let open_flags = if m.mounts.is_empty() { MF_STRING | MF_GRAYED } else { MF_STRING };
        append(sub, open_flags, ID_IMAGE_BASE + i * 4, "Open");
        append(sub, MF_STRING, ID_IMAGE_BASE + i * 4 + 1, "Unmount");
        let label = format!("{}  ({letters}){}", m.name.clone().unwrap_or_else(|| short_name(&m.source)), if m.ours { "" } else { "  [mounted by Windows]" });
        append(menu, MF_POPUP, sub as usize, &label);
    }
    append(menu, MF_SEPARATOR, 0, "");
    append(menu, MF_STRING, ID_MOUNT, "Mount image...");
    let recent = recent_images();
    if !recent.is_empty() {
        let sub = CreatePopupMenu();
        for (i, r) in recent.iter().enumerate() {
            let already = imgs.iter().any(|m| m.source.eq_ignore_ascii_case(r));
            append(sub, if already { MF_STRING | MF_GRAYED } else { MF_STRING }, ID_RECENT_BASE + i, &short_name(r).replace('&', "&&"));
        }
        append(menu, MF_POPUP, sub as usize, "Mount recent");
    }
    RECENT = recent;
    append(menu, MF_STRING, ID_CONVERT, "Convert image...");
    append(menu, MF_STRING, ID_CREATE, "Create ISO from folder...");
    append(menu, if imgs.is_empty() { MF_STRING | MF_GRAYED } else { MF_STRING }, ID_UNMOUNT_ALL, "Unmount all");
    append(menu, MF_SEPARATOR, 0, "");
    let auto = cli_output(&["autostart", "status"]).map(|s| s.trim() == "on").unwrap_or(false);
    append(menu, MF_STRING | if auto { MF_CHECKED } else { MF_UNCHECKED }, ID_AUTOSTART, "Start with Windows");
    append(menu, MF_SEPARATOR, 0, "");
    append(menu, MF_STRING, ID_UNINSTALL, "Uninstall rustimgmount...");
    append(menu, MF_STRING, ID_EXIT, "Exit");
    IMAGES = imgs;
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd); // so the menu closes when clicking elsewhere
    TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, hwnd, null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
}

/// Run a rustimgmount command in the background so the tray stays responsive.
fn background(args: Vec<String>, hwnd: HWND) {
    let h = hwnd as isize;
    std::thread::spawn(move || {
        run_hidden(&args, true);
        unsafe { PostMessageW(h as HWND, WM_TIMER, TIMER_REFRESH, 0) };
    });
}

unsafe fn pick_image(hwnd: HWND, title: &str) -> Option<String> {
    let mut file = vec![0u16; 4096];
    let filter: Vec<u16> = "Disk and disc images\0*.iso;*.img;*.ima;*.bin;*.cue;*.nrg;*.mdf;*.mds;*.mdx;*.ccd;*.cdi;*.gdi;*.chd;*.cso;*.zso;*.dax;*.isz;*.pbp;*.m3u;*.ecm;*.vhd;*.vhdx;*.vmdk;*.vdi;*.qcow2;*.hdd;*.hdi;*.fdi;*.nhd;*.d88;*.st;*.msa;*.e01;*.ex01;*.001;*.flp;*.vfd;*.dsk;*.raw;*.dd;*.zip;*.7z;*.rar;*.gz;*.xz;*.bz2\0All files\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title = wide(title);
    let mut ofn: OPENFILENAMEW = std::mem::zeroed();
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = hwnd;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = file.as_mut_ptr();
    ofn.nMaxFile = file.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST;
    if GetOpenFileNameW(&mut ofn) == 0 {
        return None;
    }
    let end = file.iter().position(|&c| c == 0).unwrap_or(0);
    Some(String::from_utf16_lossy(&file[..end]))
}

/// Remove rustimgmount from this PC: through the installer's uninstaller when it was
/// installed that way (it asks for confirmation itself), otherwise `rustimgmount setup --remove`.
unsafe fn uninstall(hwnd: HWND) {
    let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_owned())) else { return };
    let unins = dir.join("unins000.exe");
    if unins.exists() {
        let _ = std::process::Command::new(unins).spawn();
        return;
    }
    let text = wide(
        "Remove rustimgmount from this PC?\n\nThis removes the program, its right-click menu, double-click mounting and the Start Menu entry. \
         Images that are mounted stay mounted until you eject them or restart.",
    );
    let title = wide("Uninstall rustimgmount");
    if MessageBoxW(hwnd, text.as_ptr(), title.as_ptr(), MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_SETFOREGROUND) != IDYES {
        return;
    }
    let Some(cli) = cli_exe() else { return };
    // The removal stops this tray app itself, so it is started on its own and not waited for.
    let _ = std::process::Command::new(cli).args(["setup", "--remove"]).creation_flags(0x0800_0000).spawn();
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            let event = (lparam & 0xFFFF) as u32;
            if event == WM_LBUTTONUP || event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                show_menu(hwnd);
            }
            0
        }
        WM_TIMER => {
            refresh_tooltip(hwnd);
            0
        }
        WM_COMMAND => {
            let id = wparam & 0xFFFF;
            #[allow(static_mut_refs)]
            let imgs = IMAGES.clone();
            match id {
                ID_MOUNT => {
                    if let Some(p) = pick_image(hwnd, "Mount image") {
                        background(vec!["mount".into(), p, "--open".into()], hwnd);
                    }
                }
                ID_CONVERT => {
                    if let Some(p) = pick_image(hwnd, "Convert image") {
                        // Its own process: the dialog and a long conversion shouldn't tie up the tray.
                        if let Ok(me) = std::env::current_exe() {
                            let _ = std::process::Command::new(me).args(["convert-ui", &p]).spawn();
                        }
                    }
                }
                ID_CREATE => {
                    if let Some(p) = crate::convertui::browse_for(hwnd, "Folder to make an ISO image of") {
                        // Its own process, which shows the progress and any error.
                        if let Ok(me) = std::env::current_exe() {
                            let _ = std::process::Command::new(me).args(["create", &p]).spawn();
                        }
                    }
                }
                id if (ID_RECENT_BASE..ID_RECENT_BASE + 20).contains(&id) => {
                    #[allow(static_mut_refs)]
                    if let Some(p) = RECENT.get(id - ID_RECENT_BASE) {
                        background(vec!["mount".into(), p.clone(), "--open".into()], hwnd);
                    }
                }
                ID_UNMOUNT_ALL => background(vec!["unmount".into(), "all".into()], hwnd),
                ID_AUTOSTART => {
                    let on = cli_output(&["autostart", "status"]).map(|s| s.trim() == "on").unwrap_or(false);
                    if cli_output(&["autostart", if on { "off" } else { "on" }]).is_none() {
                        message_box("Could not change the start-with-Windows setting.", true);
                    }
                }
                ID_UNINSTALL => uninstall(hwnd),
                ID_EXIT => {
                    DestroyWindow(hwnd);
                }
                id if id >= ID_IMAGE_BASE => {
                    let (i, action) = ((id - ID_IMAGE_BASE) / 4, (id - ID_IMAGE_BASE) % 4);
                    if let Some(m) = imgs.get(i) {
                        match action {
                            0 => {
                                if let Some(mp) = m.mounts.first() {
                                    let _ = std::process::Command::new("explorer.exe").arg(mp).spawn();
                                }
                            }
                            _ => background(vec!["unmount".into(), m.source.clone()], hwnd),
                        }
                    }
                }
                _ => {}
            }
            0
        }
        WM_DESTROY => {
            let nid = notify_data(hwnd);
            Shell_NotifyIconW(NIM_DELETE, &nid);
            PostQuitMessage(0);
            0
        }
        // Explorer restarted: the notification area is new, so add the icon again.
        m if m == TASKBAR_CREATED && m != 0 => {
            add_icon(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub fn run() {
    unsafe {
        // One tray icon is enough.
        let name = wide("Local\\rustimgmount-tray");
        let mutex = CreateMutexW(null(), 1, name.as_ptr());
        if mutex.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        let hinst = GetModuleHandleW(null());
        let class = wide("rustimgmount_tray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let tc = wide("TaskbarCreated");
        TASKBAR_CREATED = RegisterWindowMessageW(tc.as_ptr());
        let title = wide("rustimgmount");
        let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), 0, 0, 0, 0, 0, null_mut(), null_mut(), hinst, null());
        if hwnd.is_null() {
            message_box("Could not start the tray app.", true);
            return;
        }
        add_icon(hwnd);
        refresh_tooltip(hwnd);
        SetTimer(hwnd, TIMER_REFRESH, 10_000, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        CloseHandle(mutex);
    }
}
