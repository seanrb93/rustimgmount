//! Converting from Explorer: the "Convert" dialog and the conversion queue.
//!
//! Explorer starts one rustimgmountw.exe per selected file. So that a selection of 20
//! images gives one dialog and one progress window instead of 20, every instance drops
//! what it was asked to do into a folder and then either becomes the single owner that
//! handles everything dropped there, or leaves as soon as the owner has taken its request.

use super::{cli_exe, message_box, parse_percent, summarize, wide, CREATE_NO_WINDOW};
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::{CreateFontIndirectW, DeleteObject, HFONT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Where rustimgmount keeps its records (same rule as the command-line program).
pub fn app_dir() -> PathBuf {
    match std::env::var_os("RUSTIMGMOUNT_DATA").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("rustimgmount"),
    }
}

// ---------------------------------------------------------------- drop folders

/// Put a request into the drop folder `kind`; returns its file.
fn drop_request(kind: &str, content: &str) -> Option<PathBuf> {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = app_dir().join(kind);
    std::fs::create_dir_all(&dir).ok()?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let file = dir.join(format!("{now:030}-{}-{n}.txt", std::process::id()));
    // Written under another name first, so the owner never reads half a request.
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, content).ok()?;
    std::fs::rename(&tmp, &file).ok()?;
    Some(file)
}

/// Is a process with this id still running?
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == 259 // STILL_ACTIVE
    }
}

/// The process that dropped a request (its id is part of the file name) stays around until
/// the request is taken. If it is gone, the request is a leftover from a batch that was cut
/// short, and must not start by surprise later.
fn sender_alive(file: &Path) -> bool {
    let name = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    name.split('-').nth(1).and_then(|p| p.parse().ok()).is_some_and(process_alive)
}

/// Take every request out of the drop folder, oldest first.
fn take_requests(kind: &str) -> Vec<String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(app_dir().join(kind))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        if !sender_alive(&f) {
            let _ = std::fs::remove_file(&f);
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&f) {
            if std::fs::remove_file(&f).is_ok() {
                out.push(text);
            }
        }
    }
    out
}

/// Keep taking requests until none have arrived for a moment (Explorer starts the
/// instances of a multi-selection one after another).
fn take_burst(kind: &str) -> Vec<String> {
    let mut all = Vec::new();
    let mut quiet = 0;
    let start = Instant::now();
    while quiet < 3 && start.elapsed() < Duration::from_secs(5) {
        let more = take_requests(kind);
        quiet = if more.is_empty() { quiet + 1 } else { 0 };
        all.extend(more);
        if quiet < 3 {
            std::thread::sleep(Duration::from_millis(120));
        }
    }
    all
}

struct Owner(HANDLE);
impl Drop for Owner {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

/// Become the one instance that handles the drop folder `kind`, or return `None` once
/// another instance has taken `mine`.
fn own(kind: &str, mine: &[PathBuf]) -> Option<Owner> {
    let name = wide(&format!("Local\\rustimgmountw-{kind}"));
    let h = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
    if h.is_null() {
        return None;
    }
    loop {
        // WAIT_OBJECT_0, or WAIT_ABANDONED when the previous owner was killed.
        let r = unsafe { WaitForSingleObject(h, 150) };
        if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
            return Some(Owner(h));
        }
        if !mine.iter().any(|f| f.exists()) {
            unsafe { CloseHandle(h) };
            return None;
        }
    }
}

// ---------------------------------------------------------------- the queue

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Job {
    pub image: String,
    pub to: String,
    #[serde(default)]
    pub out_dir: Option<String>,
    #[serde(default)]
    pub verify: bool,
    #[serde(default)]
    pub delete: bool,
    #[serde(default)]
    pub overwrite: bool,
}

struct Queue {
    child: Mutex<Option<Child>>,
    title: Mutex<String>,
    line: Mutex<String>,
    percent: Mutex<Option<u32>>,
    done: AtomicBool,
    cancelled: AtomicBool,
    closing: AtomicBool,
    /// (image name, error) of the conversions that failed.
    failures: Mutex<Vec<(String, String)>>,
    finished: Mutex<usize>,
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
}

/// Run one conversion hidden, feeding its progress to the dialog. Returns its error, if any.
fn run_job(q: &Queue, job: &Job, cli: &Path) -> Option<String> {
    let mut cmd = Command::new(cli);
    cmd.args(["convert", &job.image, "--to", &job.to]);
    if let Some(d) = &job.out_dir {
        cmd.args(["--out-dir", d]);
    }
    for (on, flag) in [(job.verify, "--verify"), (job.delete, "--delete-original"), (job.overwrite, "--overwrite")] {
        if on {
            cmd.arg(flag);
        }
    }
    let child = cmd.arg("--quiet-ui").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).creation_flags(CREATE_NO_WINDOW).spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return Some(format!("could not start rustimgmount.exe: {e}")),
    };
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    *q.child.lock().unwrap() = Some(child);
    let t_out = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    // stderr carries progress ("Converting to CHD: 45% ...") separated by carriage returns.
    let mut text = String::new();
    let (mut chunk, mut pending) = ([0u8; 4096], Vec::new());
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
            match parse_percent(&piece) {
                Some(p) => *q.percent.lock().unwrap() = Some(p),
                None => {
                    text.push_str(&piece);
                    text.push('\n');
                }
            }
            *q.line.lock().unwrap() = piece;
        }
    }
    text.push_str(String::from_utf8_lossy(&pending).trim());
    let stdout = t_out.join().unwrap_or_default();
    let status = q.child.lock().unwrap().take().and_then(|mut c| c.wait().ok());
    if status.is_some_and(|s| s.success()) {
        return None;
    }
    let msg = summarize(&format!("{stdout}\n{text}"));
    Some(msg.lines().last().unwrap_or("failed").trim_start_matches("error: ").to_string())
}

/// Whether the progress bar currently shows a percentage (rather than the "busy" animation).
/// Switching modes resets the bar, so it is done once per change, not on every update.
static BAR_SHOWS_PERCENT: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn queue_cb(hwnd: HWND, msg: i32, _w: WPARAM, _l: LPARAM, data: isize) -> i32 {
    let q = &*(data as *const Queue);
    match msg {
        TDN_CREATED => {
            BAR_SHOWS_PERCENT.store(false, Ordering::SeqCst);
            SendMessageW(hwnd, TDM_SET_PROGRESS_BAR_MARQUEE as u32, 1, 30);
        }
        TDN_TIMER => {
            if q.done.load(Ordering::SeqCst) {
                q.closing.store(true, Ordering::SeqCst);
                SendMessageW(hwnd, TDM_CLICK_BUTTON as u32, IDCANCEL as usize, 0);
                return 0;
            }
            match *q.percent.lock().unwrap() {
                Some(p) => {
                    if !BAR_SHOWS_PERCENT.swap(true, Ordering::SeqCst) {
                        // Stop the "busy" animation and show a real 0-100 bar.
                        SendMessageW(hwnd, TDM_SET_PROGRESS_BAR_MARQUEE as u32, 0, 0);
                        SendMessageW(hwnd, TDM_SET_MARQUEE_PROGRESS_BAR as u32, 0, 0);
                        SendMessageW(hwnd, TDM_SET_PROGRESS_BAR_RANGE as u32, 0, 100 << 16);
                    }
                    SendMessageW(hwnd, TDM_SET_PROGRESS_BAR_POS as u32, p.min(100) as usize, 0);
                }
                None => {
                    if BAR_SHOWS_PERCENT.swap(false, Ordering::SeqCst) {
                        SendMessageW(hwnd, TDM_SET_MARQUEE_PROGRESS_BAR as u32, 1, 0);
                        SendMessageW(hwnd, TDM_SET_PROGRESS_BAR_MARQUEE as u32, 1, 30);
                    }
                }
            }
            let title = wide(&q.title.lock().unwrap());
            SendMessageW(hwnd, TDM_SET_ELEMENT_TEXT as u32, TDE_MAIN_INSTRUCTION as usize, title.as_ptr() as isize);
            let line = q.line.lock().unwrap().clone();
            if !line.is_empty() {
                let w = wide(&line);
                SendMessageW(hwnd, TDM_SET_ELEMENT_TEXT as u32, TDE_CONTENT as usize, w.as_ptr() as isize);
            }
        }
        TDN_BUTTON_CLICKED => {
            if !q.closing.load(Ordering::SeqCst) {
                // Cancel: stop the current conversion and drop the ones still waiting.
                q.cancelled.store(true, Ordering::SeqCst);
                if let Some(c) = q.child.lock().unwrap().as_mut() {
                    let _ = c.kill();
                }
            }
        }
        _ => {}
    }
    0
}

/// Add conversions to the queue and, unless another instance is already working through
/// it, run the queue: one conversion at a time, one progress window for all of them.
pub fn enqueue(jobs: &[Job]) -> i32 {
    let mine: Vec<PathBuf> = jobs.iter().filter_map(|j| drop_request("queue", &serde_json::to_string(j).ok()?)).collect();
    if mine.len() != jobs.len() {
        message_box("Could not queue the conversion (is the rustimgmount data folder writable?).", true);
        return super::ERROR_SHOWN;
    }
    let Some(owner) = own("queue", &mine) else { return 0 };
    let Some(cli) = cli_exe() else {
        message_box("rustimgmount.exe was not found next to rustimgmountw.exe.", true);
        return super::ERROR_SHOWN;
    };
    let q = Arc::new(Queue {
        child: Mutex::new(None),
        title: Mutex::new("Converting...".into()),
        line: Mutex::new(String::new()),
        percent: Mutex::new(None),
        done: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        closing: AtomicBool::new(false),
        failures: Mutex::new(Vec::new()),
        finished: Mutex::new(0),
    });
    let start = Instant::now();
    let worker = {
        let (q, cli) = (q.clone(), cli.clone());
        std::thread::spawn(move || {
            let mut seen = 0usize;
            loop {
                let batch: Vec<Job> = take_burst("queue").iter().filter_map(|t| serde_json::from_str(t).ok()).collect();
                if batch.is_empty() || q.cancelled.load(Ordering::SeqCst) {
                    break;
                }
                seen += batch.len();
                for job in &batch {
                    if q.cancelled.load(Ordering::SeqCst) {
                        break;
                    }
                    let n = *q.finished.lock().unwrap() + 1;
                    let name = file_name(&job.image);
                    *q.title.lock().unwrap() = if seen > 1 { format!("Converting {name} ({n} of {seen})") } else { format!("Converting {name}") };
                    *q.percent.lock().unwrap() = None;
                    *q.line.lock().unwrap() = "Preparing...".into();
                    if let Some(err) = run_job(&q, job, &cli) {
                        if !q.cancelled.load(Ordering::SeqCst) {
                            q.failures.lock().unwrap().push((name, err));
                        }
                    }
                    *q.finished.lock().unwrap() += 1;
                }
            }
            q.done.store(true, Ordering::SeqCst);
        })
    };
    // Quick conversions finish silently; slower ones get the progress window.
    let delay = std::env::var("RUSTIMGMOUNTW_DIALOG_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500);
    while !q.done.load(Ordering::SeqCst) && start.elapsed() < Duration::from_millis(delay) {
        std::thread::sleep(Duration::from_millis(30));
    }
    if !q.done.load(Ordering::SeqCst) {
        let (title, main, content) = (wide("rustimgmount"), wide(&q.title.lock().unwrap()), wide("Preparing..."));
        let mut cfg: TASKDIALOGCONFIG = unsafe { std::mem::zeroed() };
        cfg.cbSize = std::mem::size_of::<TASKDIALOGCONFIG>() as u32;
        cfg.dwFlags = TDF_SHOW_PROGRESS_BAR | TDF_SHOW_MARQUEE_PROGRESS_BAR | TDF_CALLBACK_TIMER | TDF_ALLOW_DIALOG_CANCELLATION | TDF_CAN_BE_MINIMIZED;
        cfg.dwCommonButtons = TDCBF_CANCEL_BUTTON;
        cfg.pszWindowTitle = title.as_ptr();
        cfg.pszMainInstruction = main.as_ptr();
        cfg.pszContent = content.as_ptr();
        cfg.pfCallback = Some(queue_cb);
        cfg.lpCallbackData = Arc::as_ptr(&q) as isize;
        cfg.cxWidth = 300;
        unsafe { TaskDialogIndirect(&cfg, null_mut(), null_mut(), null_mut()) };
    }
    let _ = worker.join();
    let cancelled = q.cancelled.load(Ordering::SeqCst);
    if cancelled {
        // Requests still waiting belong to the cancelled batch.
        take_requests("queue");
        // Any rustimgmount command tidies up; this removes the half-written file.
        let _ = Command::new(&cli).args(["list"]).creation_flags(CREATE_NO_WINDOW).output();
    }
    drop(owner);
    let (done, failures) = (*q.finished.lock().unwrap(), q.failures.lock().unwrap().clone());
    if cancelled {
        return super::ERROR_SHOWN;
    }
    if !failures.is_empty() {
        let mut text = if done == 1 {
            format!("{} could not be converted.\n\n{}", failures[0].0, failures[0].1)
        } else {
            let mut t = format!("{} of {done} images could not be converted:\n", failures.len());
            for (name, err) in failures.iter().take(8) {
                t.push_str(&format!("\n{name}\n    {err}\n"));
            }
            t
        };
        if failures.len() > 8 {
            text.push_str(&format!("\n...and {} more.", failures.len() - 8));
        }
        message_box(&text, true);
        return super::ERROR_SHOWN;
    }
    // Something long finished while the user was probably doing something else: say so.
    if start.elapsed() > Duration::from_secs(10) || done > 1 {
        notify(&if done == 1 { "The image has been converted.".to_string() } else { format!("{done} images have been converted.") });
    }
    0
}

/// Show a notification from the notification area, without needing the tray app.
fn notify(text: &str) {
    unsafe {
        let class = wide("STATIC");
        let hwnd = CreateWindowExW(0, class.as_ptr(), null(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), GetModuleHandleW(null()), null());
        if hwnd.is_null() {
            return;
        }
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 77;
        nid.uFlags = NIF_ICON | NIF_INFO | NIF_TIP;
        nid.hIcon = app_icon();
        nid.dwInfoFlags = NIIF_INFO;
        let put = |dst: &mut [u16], s: &str| {
            for (i, c) in s.encode_utf16().take(dst.len() - 1).enumerate() {
                dst[i] = c;
            }
        };
        put(&mut nid.szTip, "rustimgmount");
        put(&mut nid.szInfoTitle, "rustimgmount");
        put(&mut nid.szInfo, text);
        if Shell_NotifyIconW(NIM_ADD, &nid) != 0 {
            // The icon has to stay for the notification to be shown.
            std::thread::sleep(Duration::from_secs(8));
            Shell_NotifyIconW(NIM_DELETE, &nid);
        }
        DestroyWindow(hwnd);
    }
}

pub unsafe fn app_icon() -> HICON {
    let exe = wide(&std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default());
    let mut small: HICON = null_mut();
    ExtractIconExW(exe.as_ptr(), 0, null_mut(), &mut small, 1);
    if small.is_null() {
        LoadIconW(null_mut(), IDI_APPLICATION)
    } else {
        small
    }
}

// ---------------------------------------------------------------- the dialog

/// What the Convert dialog was last set to, so it opens the same way next time.
/// "Move the original to the Recycle Bin" is deliberately not remembered: deleting
/// originals should be asked for each time.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Prefs {
    /// Formats chosen before, most recent first (discs and disks have different lists).
    #[serde(default)]
    formats: Vec<String>,
    #[serde(default)]
    out_dir: Option<String>,
    #[serde(default)]
    verify: bool,
    #[serde(default)]
    overwrite: bool,
}

impl Prefs {
    fn file() -> PathBuf {
        app_dir().join("convert-dialog.json")
    }
    fn load() -> Prefs {
        std::fs::read(Self::file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    fn save(&self) {
        let _ = std::fs::create_dir_all(app_dir());
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(Self::file(), json);
        }
    }
}

struct Choice {
    id: String,
    label: String,
}

/// Ask rustimgmount.exe which formats every one of these images can be converted to.
fn formats_for(files: &[String]) -> Result<Vec<Choice>, String> {
    let cli = cli_exe().ok_or("rustimgmount.exe was not found next to rustimgmountw.exe.")?;
    let out = Command::new(cli).arg("formats").args(files).creation_flags(CREATE_NO_WINDOW).output().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| summarize(&String::from_utf8_lossy(&out.stderr)))?;
    let list: Vec<Choice> = v["formats"]
        .as_array()
        .map(|a| a.iter().map(|f| Choice { id: f["id"].as_str().unwrap_or("").into(), label: f["label"].as_str().unwrap_or("").into() }).collect())
        .unwrap_or_default();
    if let Some(e) = v["errors"].as_array().and_then(|a| a.first()) {
        let name = file_name(e["image"].as_str().unwrap_or(""));
        return Err(format!("{name} can't be converted:\n\n{}", e["error"].as_str().unwrap_or("")));
    }
    if list.is_empty() {
        return Err("These images have no format in common that all of them can be converted to (discs and disks can't be mixed). Select them separately.".into());
    }
    Ok(list)
}

const ID_FORMAT: i32 = 100;
const ID_FOLDER: i32 = 101;
const ID_BROWSE: i32 = 102;
const ID_VERIFY: i32 = 103;
const ID_DELETE: i32 = 104;
const ID_OVERWRITE: i32 = 105;
const ID_SAME: i32 = 106;

struct Dialog {
    out_dir: Option<String>,
    /// Set when "Convert" is pressed: (format index, verify, delete, overwrite).
    result: Option<(usize, bool, bool, bool)>,
    closed: bool,
}

static mut DIALOG: Dialog = Dialog { out_dir: None, result: None, closed: false };

unsafe fn checked(hwnd: HWND, id: i32) -> bool {
    SendMessageW(GetDlgItem(hwnd, id), BM_GETCHECK, 0, 0) == 1
}

unsafe fn browse(owner: HWND) -> Option<String> {
    browse_for(owner, "Folder for the converted images")
}

/// Let the user pick a folder.
pub unsafe fn browse_for(owner: HWND, title: &str) -> Option<String> {
    let title = wide(title);
    let mut bi: BROWSEINFOW = std::mem::zeroed();
    bi.hwndOwner = owner;
    bi.lpszTitle = title.as_ptr();
    bi.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
    let pidl = SHBrowseForFolderW(&bi);
    if pidl.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let ok = SHGetPathFromIDListW(pidl, buf.as_mut_ptr());
    windows_sys::Win32::System::Com::CoTaskMemFree(pidl as _);
    let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
    (ok != 0 && end > 0).then(|| String::from_utf16_lossy(&buf[..end]))
}

#[allow(static_mut_refs)]
unsafe extern "system" fn dialog_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam & 0xFFFF) as i32;
            match id {
                ID_BROWSE => {
                    if let Some(dir) = browse(hwnd) {
                        let w = wide(&dir);
                        SetWindowTextW(GetDlgItem(hwnd, ID_FOLDER), w.as_ptr());
                        DIALOG.out_dir = Some(dir);
                        EnableWindow(GetDlgItem(hwnd, ID_SAME), 1);
                    }
                }
                ID_SAME => {
                    let w = wide("The same folder as each image");
                    SetWindowTextW(GetDlgItem(hwnd, ID_FOLDER), w.as_ptr());
                    DIALOG.out_dir = None;
                    EnableWindow(GetDlgItem(hwnd, ID_SAME), 0);
                }
                ID_DELETE => {
                    // Originals are only removed after a successful check.
                    let on = checked(hwnd, ID_DELETE);
                    let verify = GetDlgItem(hwnd, ID_VERIFY);
                    if on {
                        SendMessageW(verify, BM_SETCHECK, 1, 0);
                    }
                    EnableWindow(verify, if on { 0 } else { 1 });
                }
                1 => {
                    let sel = SendMessageW(GetDlgItem(hwnd, ID_FORMAT), CB_GETCURSEL, 0, 0).max(0) as usize;
                    DIALOG.result = Some((sel, checked(hwnd, ID_VERIFY), checked(hwnd, ID_DELETE), checked(hwnd, ID_OVERWRITE)));
                    DestroyWindow(hwnd);
                }
                2 => {
                    DestroyWindow(hwnd);
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            DIALOG.closed = true;
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Show the Convert dialog for these images. Returns the conversions to run.
#[allow(static_mut_refs)]
fn dialog(files: &[String]) -> Vec<Job> {
    let choices = match formats_for(files) {
        Ok(c) => c,
        Err(e) => {
            message_box(&e, true);
            return Vec::new();
        }
    };
    unsafe {
        windows_sys::Win32::System::Com::CoInitializeEx(null(), 2); // apartment-threaded, for the folder picker
        let hinst = GetModuleHandleW(null());
        let class = wide("rustimgmount_convert");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(dialog_proc),
            hInstance: hinst,
            lpszClassName: class.as_ptr(),
            hbrBackground: 16 as _, // COLOR_BTNFACE + 1
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hIcon: app_icon(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        DIALOG = Dialog { out_dir: None, result: None, closed: false };

        let dpi = windows_sys::Win32::UI::HiDpi::GetDpiForSystem().max(96) as i32;
        let s = |v: i32| v * dpi / 96;
        let (cw, ch) = (s(460), s(262));
        let style = WS_CAPTION | WS_SYSMENU | WS_POPUP | WS_MINIMIZEBOX;
        let mut rc = RECT { left: 0, top: 0, right: cw, bottom: ch };
        AdjustWindowRectEx(&mut rc, style, 0, WS_EX_DLGMODALFRAME);
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        let (sx, sy) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let title = wide("Convert image");
        let hwnd = CreateWindowExW(WS_EX_DLGMODALFRAME, class.as_ptr(), title.as_ptr(), style, (sx - w) / 2, (sy - h) / 2, w, h, null_mut(), null_mut(), hinst, null());
        if hwnd.is_null() {
            return Vec::new();
        }
        let mut ncm: NONCLIENTMETRICSW = std::mem::zeroed();
        ncm.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoW(SPI_GETNONCLIENTMETRICS, ncm.cbSize, &mut ncm as *mut _ as _, 0);
        let font: HFONT = CreateFontIndirectW(&ncm.lfMessageFont);
        let add = |class: &str, text: &str, style: u32, x: i32, y: i32, w: i32, h: i32, id: i32| -> HWND {
            let (c, t) = (wide(class), wide(text));
            let ctl = CreateWindowExW(0, c.as_ptr(), t.as_ptr(), WS_CHILD | WS_VISIBLE | style, s(x), s(y), s(w), s(h), hwnd, id as isize as HMENU, hinst, null());
            SendMessageW(ctl, WM_SETFONT, font as usize, 1);
            ctl
        };
        let what = if files.len() == 1 { file_name(&files[0]) } else { format!("{} images", files.len()) };
        add("STATIC", &format!("Convert {what}"), 0x80 | 0x4000 /* SS_NOPREFIX | SS_ENDELLIPSIS */, 16, 14, 428, 20, -1);
        add("STATIC", "Convert to:", 0, 16, 50, 90, 20, -1);
        let combo = add("COMBOBOX", "", (CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP, 110, 46, 334, 400, ID_FORMAT);
        for c in &choices {
            let l = wide(&c.label);
            SendMessageW(combo, CB_ADDSTRING, 0, l.as_ptr() as isize);
        }
        // Start on CHD for discs (the usual reason to convert), else the first choice.
        // Start on the format used last time that this image can be converted to; failing
        // that, CHD for discs (the usual reason to convert), else the first choice.
        let mut prefs = Prefs::load();
        let first = prefs
            .formats
            .iter()
            .find_map(|id| choices.iter().position(|c| &c.id == id))
            .or_else(|| choices.iter().position(|c| c.id == "chd"))
            .unwrap_or(0);
        SendMessageW(combo, CB_SETCURSEL, first, 0);
        add("STATIC", "Save in:", 0, 16, 86, 90, 20, -1);
        add("EDIT", "The same folder as each image", (ES_READONLY as u32) | (ES_AUTOHSCROLL as u32) | WS_BORDER, 110, 82, 334, 23, ID_FOLDER);
        add("BUTTON", "Choose folder...", WS_TABSTOP, 110, 112, 120, 26, ID_BROWSE);
        let same = add("BUTTON", "Use the same folder", WS_TABSTOP, 238, 112, 140, 26, ID_SAME);
        EnableWindow(same, 0);
        add("BUTTON", "Check the new image against the original when it's done", (BS_AUTOCHECKBOX as u32) | WS_TABSTOP, 16, 152, 428, 20, ID_VERIFY);
        add("BUTTON", "Then move the original to the Recycle Bin", (BS_AUTOCHECKBOX as u32) | WS_TABSTOP, 16, 176, 428, 20, ID_DELETE);
        add("BUTTON", "Replace files that already exist", (BS_AUTOCHECKBOX as u32) | WS_TABSTOP, 16, 200, 428, 20, ID_OVERWRITE);
        add("BUTTON", "Convert", (BS_DEFPUSHBUTTON as u32) | WS_TABSTOP, 258, 226, 90, 26, 1);
        add("BUTTON", "Cancel", WS_TABSTOP, 354, 226, 90, 26, 2);
        if let Some(dir) = prefs.out_dir.clone().filter(|d| Path::new(d).is_dir()) {
            let w = wide(&dir);
            SetWindowTextW(GetDlgItem(hwnd, ID_FOLDER), w.as_ptr());
            DIALOG.out_dir = Some(dir);
            EnableWindow(same, 1);
        }
        for (id, on) in [(ID_VERIFY, prefs.verify), (ID_OVERWRITE, prefs.overwrite)] {
            SendMessageW(GetDlgItem(hwnd, id), BM_SETCHECK, on as usize, 0);
        }
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(combo);

        let mut msg: MSG = std::mem::zeroed();
        while !DIALOG.closed && GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        DeleteObject(font);
        let Some((sel, verify, delete, overwrite)) = DIALOG.result else { return Vec::new() };
        let Some(choice) = choices.get(sel) else { return Vec::new() };
        prefs.formats.retain(|id| id != &choice.id);
        prefs.formats.insert(0, choice.id.clone());
        prefs.formats.truncate(8);
        // (Ticking "move the original" forces the check on; that isn't the user's own setting.)
        prefs.verify = verify && !delete || prefs.verify && delete;
        prefs.overwrite = overwrite;
        prefs.out_dir = DIALOG.out_dir.clone();
        prefs.save();
        files
            .iter()
            .map(|f| Job { image: f.clone(), to: choice.id.clone(), out_dir: DIALOG.out_dir.clone(), verify: verify || delete, delete, overwrite })
            .collect()
    }
}

/// `rustimgmountw convert-ui <image>...`: one dialog for everything selected, then the queue.
pub fn pick(files: &[String]) -> i32 {
    let mine: Vec<PathBuf> = files.iter().filter_map(|f| drop_request("pick", f)).collect();
    let Some(owner) = own("pick", &mine) else { return 0 };
    let mut jobs = Vec::new();
    loop {
        let selected: Vec<String> = take_burst("pick").into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if selected.is_empty() {
            break;
        }
        jobs.extend(dialog(&selected));
    }
    drop(owner);
    if jobs.is_empty() {
        return 0;
    }
    enqueue(&jobs)
}
