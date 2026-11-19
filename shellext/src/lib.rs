//! The right-click menu entry for rustimgmount on Windows 11.
//!
//! Windows 11's main context menu only lists commands from packaged apps that implement
//! `IExplorerCommand`. This DLL is that implementation: one "rustimgmount" entry whose
//! flyout holds Mount, Unmount, the usual formats to convert the selected kind of image
//! to, and the Convert dialog. Each one just starts rustimgmountw.exe (next to this DLL).
//! `rustimgmount setup` registers the package that points Explorer at this DLL.

#![allow(non_snake_case)]

use std::cell::Cell;
use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::core::{implement, IUnknown, Interface, Ref, Result, BOOL, GUID, HRESULT, HSTRING, PWSTR};
use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_NOTIMPL, E_POINTER, HINSTANCE, HMODULE, S_FALSE, S_OK};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx, IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::UI::Shell::{
    IEnumExplorerCommand, IEnumExplorerCommand_Impl, IExplorerCommand, IExplorerCommand_Impl, IShellItemArray, SHStrDupW, ECF_DEFAULT, ECF_HASSUBCOMMANDS,
    ECF_ISSEPARATOR, ECS_ENABLED, ECS_HIDDEN, SIGDN_FILESYSPATH,
};

/// Class ID of the menu entry; the package manifest written by `rustimgmount setup` names it.
const CLSID_MENU: GUID = GUID::from_u128(0x7b3f5e2a_9c41_4d6b_a8e3_52c1d0f4b916);

static MODULE: AtomicIsize = AtomicIsize::new(0);

/// The folder this DLL is in (where rustimgmountw.exe is too).
fn folder() -> PathBuf {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(Some(HMODULE(MODULE.load(Ordering::Relaxed) as *mut c_void)), &mut buf) } as usize;
    PathBuf::from(String::from_utf16_lossy(&buf[..n])).parent().map(|p| p.to_owned()).unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq)]
enum Verb {
    /// The top-level entry, which only opens the flyout.
    Root,
    Mount,
    Unmount,
    Separator,
    /// Convert straight to this format (a `--to` name).
    To(&'static str),
    /// The Convert dialog.
    More,
}

const SUBCOMMANDS: [Verb; 15] = [
    Verb::Mount,
    Verb::Unmount,
    Verb::Separator,
    Verb::To("iso"),
    Verb::To("cso"),
    Verb::To("chd"),
    Verb::To("cue"),
    Verb::To("gdi"),
    Verb::To("vhd"),
    Verb::To("vhdx"),
    Verb::To("img"),
    Verb::To("vmdk"),
    Verb::To("vdi"),
    Verb::To("qcow2"),
    Verb::More,
];

/// The formats this selection can really be converted to, found by having rustimgmount
/// look inside the files (the same question the Convert dialog asks). `None` when that
/// can't be had quickly: a big selection, a slow answer, or files it didn't look into.
/// The answer is kept for a few seconds, because Explorer asks once per menu entry.
fn suitable(files: &[String]) -> Option<Vec<String>> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static LAST: Mutex<Option<(Vec<String>, Instant, Option<Vec<String>>)>> = Mutex::new(None);
    if files.is_empty() || files.len() > 12 {
        return None;
    }
    let mut last = LAST.lock().ok()?;
    if let Some((f, at, answer)) = last.as_ref() {
        if f == files && at.elapsed() < Duration::from_secs(5) {
            return answer.clone();
        }
    }
    let answer = (|| {
        let mut child = std::process::Command::new(folder().join("rustimgmount.exe"))
            .arg("formats")
            .args(files)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x0800_0000)
            .spawn()
            .ok()?;
        // A menu must not hang on a slow or odd file: give it a moment, then go by extension.
        let start = Instant::now();
        while child.try_wait().ok()?.is_none() {
            if start.elapsed() > Duration::from_millis(1500) {
                let _ = child.kill();
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut text = String::new();
        std::io::Read::read_to_string(&mut child.stdout.take()?, &mut text).ok()?;
        if !text.contains("\"inspected\":true") {
            return None;
        }
        // {"errors":[...],"formats":[{"id":"cso","label":"..."},...],"inspected":true}
        Some(text.split("\"id\":\"").skip(1).filter_map(|s| s.split('"').next()).map(str::to_string).collect())
    })();
    *last = Some((files.to_vec(), Instant::now(), answer.clone()));
    answer
}

/// The formats offered for a file extension when the files themselves can't be asked
/// (the same lists as the classic menu's "Convert to"). Extensions used for both discs
/// and disks get none: for those the Convert dialog looks inside the file first.
fn quick_formats(ext: &str) -> &'static [&'static str] {
    match ext {
        "img" | "raw" | "001" | "e01" | "ex01" | "m3u" => &[],
        "gdi" => &["iso", "cso", "chd"],
        "chd" => &["iso", "cso", "cue", "gdi"],
        "iso" => &["cso", "chd", "cue"],
        "cso" => &["iso", "chd", "cue"],
        "cue" => &["iso", "cso", "chd"],
        "vhd" => &["vhdx", "img", "vmdk", "vdi", "qcow2"],
        "vhdx" => &["vhd", "img", "vmdk", "vdi", "qcow2"],
        "vmdk" => &["vhd", "vhdx", "img", "vdi", "qcow2"],
        "vdi" => &["vhd", "vhdx", "img", "vmdk", "qcow2"],
        "qcow2" => &["vhd", "vhdx", "img", "vmdk", "vdi"],
        "avhd" | "avhdx" | "hdd" | "hdi" | "fdi" | "nhd" | "st" | "msa" | "d88" | "d77" | "d98" | "flp" | "vfd" | "dsk" | "ima" | "dd" => {
            &["vhd", "vhdx", "img", "vmdk", "vdi", "qcow2"]
        }
        _ => &["iso", "cso", "chd", "cue"],
    }
}

fn extension(path: &str) -> String {
    Path::new(path).extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

/// Whether rustimgmount has this image mounted, from its records. `None` if they can't be read.
fn is_mounted(records: &Option<String>, path: &str) -> Option<bool> {
    let text = records.as_ref()?;
    // In the JSON file every backslash is doubled.
    let escaped = path.to_lowercase().replace('\\', "\\\\");
    // The whole image, or one disc of a multi-disc image ("game.pbp#2").
    Some(text.contains(&format!("\"{escaped}\"")) || text.contains(&format!("\"{escaped}#")))
}

/// Image files Windows has attached as virtual disks right now, lower-cased. This covers
/// images mounted without rustimgmount (Explorer's own Mount for .iso, .vhd and .vhdx).
fn attached() -> Vec<String> {
    use windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows::Win32::Storage::Vhd::GetAllAttachedVirtualDiskPhysicalPaths;
    let mut size: u32 = 4096;
    for _ in 0..4 {
        let mut buf = vec![0u16; size as usize / 2 + 1];
        let code = unsafe { GetAllAttachedVirtualDiskPhysicalPaths(&mut size, windows::core::PWSTR(buf.as_mut_ptr())) };
        if code == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if code.is_err() {
            break;
        }
        let n = (size as usize / 2).min(buf.len());
        return buf[..n]
            .split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf16_lossy(s).trim_start_matches(r"\\?\").to_lowercase())
            .collect();
    }
    Vec::new()
}

fn records() -> Option<String> {
    let dir = match std::env::var_os("RUSTIMGMOUNT_DATA").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("rustimgmount"),
    };
    std::fs::read_to_string(dir.join("mounts.json")).ok().map(|t| t.to_lowercase())
}

#[implement(IExplorerCommand)]
struct Command(Verb);

/// File system paths of the selected items.
fn paths(items: Ref<'_, IShellItemArray>) -> Vec<String> {
    let mut out = Vec::new();
    let Some(items) = items.as_ref() else { return out };
    unsafe {
        for i in 0..items.GetCount().unwrap_or(0) {
            let Ok(item) = items.GetItemAt(i) else { continue };
            if let Ok(name) = item.GetDisplayName(SIGDN_FILESYSPATH) {
                if let Ok(s) = name.to_string() {
                    out.push(s);
                }
                CoTaskMemFree(Some(name.0 as *const c_void));
            }
        }
    }
    out
}

fn launch(args: &[&str]) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new(folder().join("rustimgmountw.exe")).args(args).creation_flags(CREATE_NO_WINDOW).spawn();
}

impl IExplorerCommand_Impl for Command_Impl {
    fn GetTitle(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        let title = match self.0 {
            Verb::Root => "rustimgmount".to_string(),
            Verb::Mount => "Mount".to_string(),
            Verb::Unmount => "Unmount".to_string(),
            Verb::Separator => String::new(),
            Verb::To(id) => format!(
                "Convert to {}",
                match id {
                    "cue" => "BIN/CUE".to_string(),
                    other => other.to_uppercase(),
                }
            ),
            Verb::More => "More formats and options...".to_string(),
        };
        unsafe { SHStrDupW(&HSTRING::from(title)) }
    }

    fn GetIcon(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        if self.0 != Verb::Root {
            return Err(E_NOTIMPL.into());
        }
        // The program's icon (resource 1 of rustimgmount.exe).
        unsafe { SHStrDupW(&HSTRING::from(format!("{},-1", folder().join("rustimgmount.exe").display()))) }
    }

    fn GetToolTip(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<GUID> {
        Ok(GUID::zeroed())
    }

    /// Entries that don't apply to the selection are left out: Unmount for images that
    /// aren't mounted, Mount for ones that are, and formats that don't fit the file type.
    fn GetState(&self, items: Ref<'_, IShellItemArray>, _slow_ok: BOOL) -> Result<u32> {
        let files = paths(items);
        let show = match self.0 {
            Verb::Root | Verb::Separator | Verb::More => true,
            Verb::Mount | Verb::Unmount => {
                let (records, attached) = (records(), attached());
                let mounted: Vec<Option<bool>> = files
                    .iter()
                    .map(|f| if attached.contains(&f.to_lowercase()) { Some(true) } else { is_mounted(&records, f) })
                    .collect();
                if self.0 == Verb::Mount {
                    // Hidden only when every selected image is known to be mounted already.
                    files.is_empty() || !mounted.iter().all(|m| *m == Some(true))
                } else {
                    files.is_empty() || mounted.iter().any(|m| *m != Some(false))
                }
            }
            // (Converting to the format a file is already in, to change its flavour, is
            // left to the Convert dialog: "Convert to CHD" on a CHD would only puzzle.)
            Verb::To(id) if files.iter().any(|f| extension(f) == id) => false,
            Verb::To(id) => match suitable(&files) {
                Some(ids) => ids.iter().any(|i| i == id),
                None => !files.is_empty() && files.iter().all(|f| quick_formats(&extension(f)).contains(&id)),
            },
        };
        Ok(if show { ECS_ENABLED.0 } else { ECS_HIDDEN.0 } as u32)
    }

    fn Invoke(&self, items: Ref<'_, IShellItemArray>, _ctx: Ref<'_, IBindCtx>) -> Result<()> {
        let files = paths(items);
        match self.0 {
            Verb::Root | Verb::Separator => {}
            Verb::Mount => files.iter().for_each(|f| launch(&["mount", f, "--open"])),
            Verb::Unmount => files.iter().for_each(|f| launch(&["unmount", f])),
            // Each goes into rustimgmountw's one queue, so they run one after another.
            Verb::To(id) => files.iter().for_each(|f| launch(&["convert", f, "--to", id])),
            // One dialog for the whole selection. Windows limits a command line to about
            // 32,000 characters, so a huge selection goes in several lots; rustimgmountw
            // gathers them into the same dialog.
            Verb::More => {
                let mut lot: Vec<&str> = vec!["convert-ui"];
                let mut len = 0;
                for f in &files {
                    if len + f.len() > 24_000 {
                        launch(&lot);
                        lot.truncate(1);
                        len = 0;
                    }
                    lot.push(f);
                    len += f.len() + 3;
                }
                if lot.len() > 1 {
                    launch(&lot);
                }
            }
        }
        Ok(())
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(match self.0 {
            Verb::Root => ECF_HASSUBCOMMANDS.0,
            Verb::Separator => ECF_ISSEPARATOR.0,
            _ => ECF_DEFAULT.0,
        } as u32)
    }

    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        if self.0 != Verb::Root {
            return Err(E_NOTIMPL.into());
        }
        Ok(SubCommands(Cell::new(0)).into())
    }
}

#[implement(IEnumExplorerCommand)]
struct SubCommands(Cell<usize>);

impl IEnumExplorerCommand_Impl for SubCommands_Impl {
    fn Next(&self, count: u32, out: *mut Option<IExplorerCommand>, fetched: *mut u32) -> HRESULT {
        let mut given = 0u32;
        unsafe {
            while given < count && self.0.get() < SUBCOMMANDS.len() {
                out.add(given as usize).write(Some(Command(SUBCOMMANDS[self.0.get()]).into()));
                self.0.set(self.0.get() + 1);
                given += 1;
            }
            if !fetched.is_null() {
                *fetched = given;
            }
        }
        if given == count {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, count: u32) -> Result<()> {
        self.0.set((self.0.get() + count as usize).min(SUBCOMMANDS.len()));
        Ok(())
    }

    fn Reset(&self) -> Result<()> {
        self.0.set(0);
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumExplorerCommand> {
        Ok(SubCommands(Cell::new(self.0.get())).into())
    }
}

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Ref<'_, IUnknown>, iid: *const GUID, object: *mut *mut c_void) -> Result<()> {
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let command: IExplorerCommand = Command(Verb::Root).into();
        unsafe { command.query(iid, object).ok() }
    }

    fn LockServer(&self, _lock: BOOL) -> Result<()> {
        Ok(())
    }
}

#[no_mangle]
extern "system" fn DllMain(module: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == 1 {
        // DLL_PROCESS_ATTACH
        MODULE.store(module.0 as isize, Ordering::Relaxed);
    }
    BOOL(1)
}

#[no_mangle]
extern "system" fn DllGetClassObject(clsid: *const GUID, iid: *const GUID, object: *mut *mut c_void) -> HRESULT {
    if clsid.is_null() || iid.is_null() || object.is_null() {
        return E_POINTER;
    }
    unsafe {
        if *clsid != CLSID_MENU {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = Factory.into();
        factory.query(iid, object)
    }
}

#[no_mangle]
extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}
