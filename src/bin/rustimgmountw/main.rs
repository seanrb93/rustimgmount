//! rustimgmountw.exe: the windowless front end used by Explorer (double-click and the
//! right-click menu) and the notification-area (tray) app.
//!
//! For a command, it runs rustimgmount.exe (next to it) without a console window. Nothing
//! is shown when that's quick and succeeds; a progress dialog with Cancel appears if it
//! takes more than about 1.5 seconds; an error dialog shows any failure.
//! `rustimgmountw --tray` starts the tray app instead (see tray.rs).

#![windows_subsystem = "windows"]

mod convertui;
mod tray;

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageW, IDCANCEL};

/// Exit code meaning "failed, and the error has already been shown to the user".
pub const ERROR_SHOWN: i32 = 3;
/// Printed by rustimgmount.exe (hidden mode) just before it asks for admin rights; the
/// elevated copy shows its own progress, so this one steps aside.
const ELEVATING: &str = "@@rustimgmount:elevating";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn message_box(text: &str, error: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND};
    let (t, c) = (wide(text), wide("rustimgmount"));
    let icon = if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), MB_OK | icon | MB_SETFOREGROUND) };
}

/// The useful part of the output: drop progress updates and keep the last lines.
pub fn summarize(out: &str) -> String {
    let lines: Vec<&str> = out
        .lines()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty() && !l.contains("Press Enter") && !l.contains(ELEVATING) && parse_percent(l).is_none())
        .collect();
    let start = lines.len().saturating_sub(15);
    lines[start..].join("\n")
}

/// "Converting to ISO:  45%  (...)" -> 45
fn parse_percent(line: &str) -> Option<u32> {
    let (head, _) = line.split_once('%')?;
    let digits: String = head.chars().rev().take_while(|c| c.is_ascii_digit() || *c == ' ').collect::<String>().chars().rev().collect();
    let n = digits.trim().parse().ok()?;
    head.contains(':').then_some(n)
}

/// Path of rustimgmount.exe next to this program.
pub fn cli_exe() -> Option<std::path::PathBuf> {
    let p = std::env::current_exe().ok()?.parent()?.join("rustimgmount.exe");
    p.exists().then_some(p)
}

struct Shared {
    child: Mutex<Child>,
    output: Mutex<String>,
    line: Mutex<String>,
    percent: Mutex<Option<u32>>,
    exit: Mutex<Option<i32>>,
    cancelled: AtomicBool,
    elevating: AtomicBool,
    closing: AtomicBool,
    title: String,
}

/// Run rustimgmount.exe hidden with `args`. Shows a progress dialog if it takes a while
/// and an error dialog if it fails. Returns the exit code (ERROR_SHOWN after a dialog).
pub fn run_hidden(args: &[String], show_errors: bool) -> i32 {
    let Some(cli) = cli_exe() else {
        message_box("rustimgmount.exe was not found next to rustimgmountw.exe.", true);
        return ERROR_SHOWN;
    };
    let child = Command::new(&cli)
        .args(args)
        .arg("--quiet-ui")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            message_box(&format!("Could not start rustimgmount.exe: {e}"), true);
            return ERROR_SHOWN;
        }
    };
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let title = match (args.first().map(String::as_str), args.get(1)) {
        (Some(verb @ ("mount" | "unmount" | "convert" | "create")), Some(p)) => {
            let name = std::path::Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.clone());
            let doing = match verb {
                "mount" => "Mounting",
                "unmount" => "Unmounting",
                "create" => "Making an ISO image of",
                _ => "Converting",
            };
            format!("{doing} {name}")
        }
        _ => "Working...".to_string(),
    };
    let sh = Arc::new(Shared {
        child: Mutex::new(child),
        output: Mutex::new(String::new()),
        line: Mutex::new(String::new()),
        percent: Mutex::new(None),
        exit: Mutex::new(None),
        cancelled: AtomicBool::new(false),
        elevating: AtomicBool::new(false),
        closing: AtomicBool::new(false),
        title,
    });
    let s1 = sh.clone();
    let t_out = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = out.read_to_string(&mut buf);
        s1.output.lock().unwrap().push_str(&buf);
    });
    // stderr carries progress lines ("...: 45%") separated by carriage returns.
    let s2 = sh.clone();
    let t_err = std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        let mut pending = Vec::new();
        loop {
            let n = match err.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            pending.extend_from_slice(&chunk[..n]);
            while let Some(i) = pending.iter().position(|&b| b == b'\r' || b == b'\n') {
                let piece = String::from_utf8_lossy(&pending[..i]).trim().to_string();
                pending.drain(..=i);
                if piece.is_empty() {
                    continue;
                }
                if piece.contains(ELEVATING) {
                    s2.elevating.store(true, Ordering::SeqCst);
                    continue;
                }
                if let Some(p) = parse_percent(&piece) {
                    *s2.percent.lock().unwrap() = Some(p);
                } else {
                    let mut o = s2.output.lock().unwrap();
                    o.push_str(&piece);
                    o.push('\n');
                }
                *s2.line.lock().unwrap() = piece;
            }
        }
        let rest = String::from_utf8_lossy(&pending).trim().to_string();
        if !rest.is_empty() {
            s2.output.lock().unwrap().push_str(&rest);
        }
    });
    let s3 = sh.clone();
    std::thread::spawn(move || loop {
        if let Ok(Some(st)) = s3.child.lock().unwrap().try_wait() {
            *s3.exit.lock().unwrap() = Some(st.code().unwrap_or(1));
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    });

    // Quick operations finish silently; slow ones get a progress dialog.
    // (RUSTIMGMOUNTW_DIALOG_DELAY_MS changes the 1.5 s threshold; used by tests.)
    let delay = std::env::var("RUSTIMGMOUNTW_DIALOG_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500);
    let start = Instant::now();
    while sh.exit.lock().unwrap().is_none() && start.elapsed() < Duration::from_millis(delay) {
        std::thread::sleep(Duration::from_millis(30));
    }
    if sh.exit.lock().unwrap().is_none() && !sh.elevating.load(Ordering::SeqCst) {
        progress_dialog(&sh);
    }
    while sh.exit.lock().unwrap().is_none() {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = t_out.join();
    let _ = t_err.join();
    let code = sh.exit.lock().unwrap().unwrap_or(1);
    if sh.cancelled.load(Ordering::SeqCst) {
        // Any rustimgmount command tidies up; this removes the half-written conversion.
        if let Some(cli) = cli_exe() {
            let _ = Command::new(cli).args(["list"]).creation_flags(CREATE_NO_WINDOW).output();
        }
        return ERROR_SHOWN; // the user knows: they pressed Cancel
    }
    if code != 0 && code != ERROR_SHOWN && show_errors {
        message_box(&summarize(&sh.output.lock().unwrap()), true);
        return ERROR_SHOWN;
    }
    code
}

/// Whether the progress bar currently shows a percentage (rather than the "busy" animation).
/// Switching modes resets the bar, so it is done once per change, not on every update.
static BAR_SHOWS_PERCENT: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn progress_cb(hwnd: HWND, msg: i32, _w: WPARAM, _l: LPARAM, data: isize) -> i32 {
    let sh = &*(data as *const Shared);
    match msg {
        TDN_CREATED => {
            BAR_SHOWS_PERCENT.store(false, Ordering::SeqCst);
            SendMessageW(hwnd, TDM_SET_PROGRESS_BAR_MARQUEE as u32, 1, 30);
        }
        TDN_TIMER => {
            if sh.exit.lock().unwrap().is_some() || sh.elevating.load(Ordering::SeqCst) {
                sh.closing.store(true, Ordering::SeqCst);
                SendMessageW(hwnd, TDM_CLICK_BUTTON as u32, IDCANCEL as usize, 0);
                return 0;
            }
            if let Some(p) = *sh.percent.lock().unwrap() {
                if !BAR_SHOWS_PERCENT.swap(true, Ordering::SeqCst) {
                    // Stop the "busy" animation and show a real 0-100 bar.
                    SendMessageW(hwnd, TDM_SET_PROGRESS_BAR_MARQUEE as u32, 0, 0);
                    SendMessageW(hwnd, TDM_SET_MARQUEE_PROGRESS_BAR as u32, 0, 0);
                    SendMessageW(hwnd, TDM_SET_PROGRESS_BAR_RANGE as u32, 0, 100 << 16);
                }
                SendMessageW(hwnd, TDM_SET_PROGRESS_BAR_POS as u32, p.min(100) as usize, 0);
            }
            let line = sh.line.lock().unwrap().clone();
            if !line.is_empty() {
                let w = wide(&line);
                SendMessageW(hwnd, TDM_SET_ELEMENT_TEXT as u32, TDE_CONTENT as usize, w.as_ptr() as isize);
            }
        }
        TDN_BUTTON_CLICKED => {
            if !sh.closing.load(Ordering::SeqCst) {
                // The user pressed Cancel: stop the work.
                sh.cancelled.store(true, Ordering::SeqCst);
                let _ = sh.child.lock().unwrap().kill();
            }
        }
        _ => {}
    }
    0
}

fn progress_dialog(sh: &Arc<Shared>) {
    let title = wide("rustimgmount");
    let main = wide(&sh.title);
    let content = wide("Preparing...");
    let mut cfg: TASKDIALOGCONFIG = unsafe { std::mem::zeroed() };
    cfg.cbSize = std::mem::size_of::<TASKDIALOGCONFIG>() as u32;
    cfg.dwFlags = TDF_SHOW_PROGRESS_BAR | TDF_SHOW_MARQUEE_PROGRESS_BAR | TDF_CALLBACK_TIMER | TDF_ALLOW_DIALOG_CANCELLATION;
    cfg.dwCommonButtons = TDCBF_CANCEL_BUTTON;
    cfg.pszWindowTitle = title.as_ptr();
    cfg.pszMainInstruction = main.as_ptr();
    cfg.pszContent = content.as_ptr();
    cfg.pfCallback = Some(progress_cb);
    cfg.lpCallbackData = Arc::as_ptr(sh) as isize;
    cfg.cxWidth = 260;
    unsafe { TaskDialogIndirect(&cfg, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--tray") {
        tray::run();
        return;
    }
    // Conversions from Explorer go through one shared queue (and the Convert dialog when
    // no format was picked in the menu).
    match args.first().map(String::as_str) {
        Some("convert-ui") if args.len() > 1 => std::process::exit(convertui::pick(&args[1..])),
        Some("convert") if args.len() > 1 => {
            let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
            let has = |flag: &str| args.iter().any(|a| a == flag);
            let code = match value("--to") {
                Some(to) => convertui::enqueue(&[convertui::Job {
                    image: args[1].clone(),
                    to,
                    out_dir: value("--out-dir"),
                    verify: has("--verify"),
                    delete: has("--delete-original"),
                    overwrite: has("--overwrite"),
                    ..Default::default()
                }]),
                None => convertui::pick(&args[1..2]),
            };
            std::process::exit(code);
        }
        // "Create ISO image" on a folder joins the same queue, so several folders are
        // done one after another. (With an output name or options it runs on its own.)
        Some("create") if args.len() == 2 => {
            std::process::exit(convertui::enqueue(&[convertui::Job { image: convertui::folder_arg(&args[1]), create: true, ..Default::default() }]));
        }
        // The Create dialog: format, disc name and where to save.
        Some("create-ui") if args.len() > 1 => std::process::exit(convertui::pick_create(&args[1..])),
        _ => {}
    }
    // A file dropped onto the exe (or "Open with") arrives as the only argument.
    if args.len() == 1 && std::path::Path::new(&args[0]).is_file() {
        args = vec!["mount".into(), args.remove(0), "--open".into()];
    }
    std::process::exit(run_hidden(&args, true));
}
