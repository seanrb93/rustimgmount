//! The Windows 11 right-click menu entry for rustimgmount.
//!
//! Windows 11's main context menu only lists commands from packaged apps that implement
//! `IExplorerCommand`. This DLL is that implementation: a "rustimgmount" entry with a
//! flyout holding Mount, Unmount and Convert. Each one just starts rustimgmountw.exe (next
//! to this DLL) the same way the classic menu entries do. `rustimgmount setup` registers
//! the package that points Explorer at this DLL.

#![allow(non_snake_case)]

use std::cell::Cell;
use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::core::{implement, IUnknown, Interface, Ref, Result, BOOL, GUID, HRESULT, HSTRING, PWSTR};
use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_NOTIMPL, E_POINTER, HINSTANCE, HMODULE, S_FALSE, S_OK};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx, IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::UI::Shell::{
    IEnumExplorerCommand, IEnumExplorerCommand_Impl, IExplorerCommand, IExplorerCommand_Impl, IShellItemArray, SHStrDupW, ECF_DEFAULT, ECF_HASSUBCOMMANDS,
    ECS_ENABLED, SIGDN_FILESYSPATH,
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
    Convert,
}

const SUBCOMMANDS: [Verb; 3] = [Verb::Mount, Verb::Unmount, Verb::Convert];

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
            Verb::Root => "rustimgmount",
            Verb::Mount => "Mount",
            Verb::Unmount => "Unmount",
            Verb::Convert => "Convert...",
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

    fn GetState(&self, _items: Ref<'_, IShellItemArray>, _slow_ok: BOOL) -> Result<u32> {
        // The package manifest lists the file types, so the entry is only asked for on images.
        Ok(ECS_ENABLED.0 as u32)
    }

    fn Invoke(&self, items: Ref<'_, IShellItemArray>, _ctx: Ref<'_, IBindCtx>) -> Result<()> {
        let files = paths(items);
        match self.0 {
            Verb::Root => {}
            Verb::Mount => files.iter().for_each(|f| launch(&["mount", f, "--open"])),
            Verb::Unmount => files.iter().for_each(|f| launch(&["unmount", f])),
            // One dialog for the whole selection. Windows limits a command line to about
            // 32,000 characters, so a huge selection goes in several lots; rustimgmountw
            // gathers them into the same dialog.
            Verb::Convert => {
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
        Ok(if self.0 == Verb::Root { ECF_HASSUBCOMMANDS.0 as u32 } else { ECF_DEFAULT.0 as u32 })
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
