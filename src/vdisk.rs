//! Thin safe wrapper over the Windows VirtDisk API (virtdisk.dll) plus the
//! volume/drive-letter plumbing needed to report where an image landed.

use crate::formats::NativeKind;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::Storage::Vhd::*;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER};

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..end]).to_string_lossy().into_owned()
}

fn check(code: WIN32_ERROR) -> io::Result<()> {
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

pub fn is_access_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<io::Error>()
            .and_then(|e| e.raw_os_error())
            // When not elevated, re-opening an attached image yields a handle that cannot
            // reach the live disk: Windows reports "device not ready" / "no longer available".
            .map(|c| {
                c == ERROR_ACCESS_DENIED as i32
                    || c == ERROR_PRIVILEGE_NOT_HELD as i32
                    || c == ERROR_NOT_READY as i32
                    || c == ERROR_DEV_NOT_EXIST as i32
            })
            .unwrap_or(false)
    })
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn storage_type(kind: NativeKind) -> VIRTUAL_STORAGE_TYPE {
    VIRTUAL_STORAGE_TYPE {
        DeviceId: match kind {
            NativeKind::Iso => VIRTUAL_STORAGE_TYPE_DEVICE_ISO,
            NativeKind::Vhd => VIRTUAL_STORAGE_TYPE_DEVICE_VHD,
            NativeKind::Vhdx => VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
        },
        VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
    }
}

pub struct VirtualDisk {
    h: Handle,
}

impl VirtualDisk {
    /// Open an image file. `read_only` matters for VHD/VHDX; ISO is always read-only.
    pub fn open(path: &Path, kind: NativeKind, read_only: bool) -> io::Result<Self> {
        let st = storage_type(kind);
        let p = wide(path);
        let mut h: HANDLE = null_mut();
        let code = unsafe {
            if kind == NativeKind::Iso {
                let mut params: OPEN_VIRTUAL_DISK_PARAMETERS = std::mem::zeroed();
                params.Version = OPEN_VIRTUAL_DISK_VERSION_1;
                params.Anonymous.Version1.RWDepth = OPEN_VIRTUAL_DISK_RW_DEPTH_DEFAULT;
                OpenVirtualDisk(&st, p.as_ptr(), VIRTUAL_DISK_ACCESS_READ, OPEN_VIRTUAL_DISK_FLAG_NONE, &params, &mut h)
            } else {
                let mut params: OPEN_VIRTUAL_DISK_PARAMETERS = std::mem::zeroed();
                params.Version = OPEN_VIRTUAL_DISK_VERSION_2;
                params.Anonymous.Version2.ReadOnly = read_only as BOOL;
                OpenVirtualDisk(&st, p.as_ptr(), VIRTUAL_DISK_ACCESS_NONE, OPEN_VIRTUAL_DISK_FLAG_NONE, &params, &mut h)
            }
        };
        check(code)?;
        Ok(Self { h: Handle(h) })
    }

    /// Attach so that the disk stays mounted after this process exits.
    pub fn attach(&self, read_only: bool, no_drive_letter: bool) -> io::Result<()> {
        let mut flags = ATTACH_VIRTUAL_DISK_FLAG_PERMANENT_LIFETIME;
        if read_only {
            flags |= ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY;
        }
        if no_drive_letter {
            flags |= ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER;
        }
        let mut params: ATTACH_VIRTUAL_DISK_PARAMETERS = unsafe { std::mem::zeroed() };
        params.Version = ATTACH_VIRTUAL_DISK_VERSION_1;
        check(unsafe { AttachVirtualDisk(self.h.0, null_mut(), flags, 0, &params, null()) })
    }

    /// Open an (attached) image just to query or detach it. The storage type is
    /// auto-detected from the file extension, which works for ISO, VHD and VHDX.
    pub fn open_query(path: &Path) -> io::Result<Self> {
        let st = VIRTUAL_STORAGE_TYPE {
            DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_UNKNOWN,
            VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_UNKNOWN,
        };
        let p = wide(path);
        let mut h: HANDLE = null_mut();
        let code = unsafe {
            OpenVirtualDisk(
                &st,
                p.as_ptr(),
                VIRTUAL_DISK_ACCESS_GET_INFO | VIRTUAL_DISK_ACCESS_DETACH,
                OPEN_VIRTUAL_DISK_FLAG_NONE,
                null(),
                &mut h,
            )
        };
        check(code)?;
        Ok(Self { h: Handle(h) })
    }

    pub fn detach(&self) -> io::Result<()> {
        check(unsafe { DetachVirtualDisk(self.h.0, DETACH_VIRTUAL_DISK_FLAG_NONE, 0) })
    }

    /// e.g. `\\.\PhysicalDrive3` or `\\.\CDROM1`. Fails if not attached.
    pub fn physical_path(&self) -> io::Result<String> {
        let mut buf = vec![0u16; 1024];
        let mut size = (buf.len() * 2) as u32;
        check(unsafe { GetVirtualDiskPhysicalPath(self.h.0, &mut size, buf.as_mut_ptr()) })?;
        Ok(from_wide(&buf))
    }
}

/// Image files of every virtual disk (ISO/VHD/VHDX) currently attached on this machine.
/// (Despite its name, GetAllAttachedVirtualDiskPhysicalPaths returns backing file paths.)
pub fn attached_images() -> io::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    for s in attached_raw()? {
        let p = PathBuf::from(s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s));
        // The same file can be attached more than once; report it once.
        if !out.iter().any(|q| q.to_string_lossy().eq_ignore_ascii_case(&p.to_string_lossy())) {
            out.push(p);
        }
    }
    Ok(out)
}

fn attached_raw() -> io::Result<Vec<String>> {
    let mut size: u32 = 4096;
    loop {
        let mut buf = vec![0u16; size as usize / 2 + 1];
        let code = unsafe { GetAllAttachedVirtualDiskPhysicalPaths(&mut size, buf.as_mut_ptr()) };
        if code == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        check(code)?;
        let n = (size as usize / 2).min(buf.len());
        return Ok(buf[..n]
            .split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(|s| OsString::from_wide(s).to_string_lossy().into_owned())
            .collect());
    }
}

fn open_device(path: &str) -> io::Result<Handle> {
    let p = wide(path);
    let h = unsafe {
        CreateFileW(p.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, null(), OPEN_EXISTING, 0, null_mut())
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(Handle(h))
}

/// Identifiers of the disks currently in this PC (physical and virtual). Windows keeps a
/// second disk with the same MBR signature or GPT disk GUID offline ("signature collision"),
/// and it cannot fix that on a read-only disk, so converted copies must not reuse these.
#[derive(Default, Debug)]
pub struct DiskIds {
    pub mbr: Vec<u32>,
    pub gpt_disks: Vec<[u8; 16]>,
    pub gpt_partitions: Vec<[u8; 16]>,
}

pub fn guid_bytes(g: &windows_sys::core::GUID) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0..4].copy_from_slice(&g.data1.to_le_bytes());
    b[4..6].copy_from_slice(&g.data2.to_le_bytes());
    b[6..8].copy_from_slice(&g.data3.to_le_bytes());
    b[8..16].copy_from_slice(&g.data4);
    b
}

/// Read the partition layout of every \\.\PhysicalDriveN (works without admin rights).
pub fn disk_ids() -> &'static DiskIds {
    use windows_sys::Win32::System::Ioctl::{DRIVE_LAYOUT_INFORMATION_EX, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, PARTITION_INFORMATION_EX};
    static IDS: std::sync::OnceLock<DiskIds> = std::sync::OnceLock::new();
    IDS.get_or_init(|| {
        let mut ids = DiskIds::default();
        for n in 0..128 {
            let Ok(h) = open_device(&format!(r"\\.\PhysicalDrive{n}")) else { continue };
            let mut buf = vec![0u64; 64 * 1024 / 8];
            let mut ret = 0u32;
            let ok = unsafe {
                DeviceIoControl(h.0, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, null(), 0, buf.as_mut_ptr() as *mut _, (buf.len() * 8) as u32, &mut ret, null_mut())
            };
            if ok == 0 {
                continue;
            }
            unsafe {
                let l = &*(buf.as_ptr() as *const DRIVE_LAYOUT_INFORMATION_EX);
                match l.PartitionStyle {
                    0 => ids.mbr.push(l.Anonymous.Mbr.Signature),
                    1 => {
                        ids.gpt_disks.push(guid_bytes(&l.Anonymous.Gpt.DiskId));
                        let entries = std::ptr::addr_of!(l.PartitionEntry) as *const PARTITION_INFORMATION_EX;
                        for i in 0..l.PartitionCount as usize {
                            let e = &*entries.add(i);
                            if e.PartitionStyle == 1 {
                                ids.gpt_partitions.push(guid_bytes(&e.Anonymous.Gpt.PartitionId));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        ids
    })
}

/// The name of the volume whose root folder is `root` ("D:\\"), as Explorer shows it.
pub fn volume_label(root: &Path) -> Option<String> {
    let w = wide(root);
    let mut name = [0u16; 261];
    let ok = unsafe { GetVolumeInformationW(w.as_ptr(), name.as_mut_ptr(), name.len() as u32, null_mut(), null_mut(), null_mut(), null_mut(), 0) };
    let end = name.iter().position(|&c| c == 0).unwrap_or(0);
    (ok != 0).then(|| String::from_utf16_lossy(&name[..end]))
}

/// Free space, in bytes, on the volume holding `dir`.
pub fn free_space(dir: &Path) -> Option<u64> {
    let w = wide(dir);
    let mut free = 0u64;
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, null_mut(), null_mut()) };
    (ok != 0).then_some(free)
}

fn mount_points(volume_name: &str) -> Vec<String> {
    let v = wide(volume_name);
    let mut buf = vec![0u16; 1024];
    let mut ret = 0u32;
    let ok = unsafe { GetVolumePathNamesForVolumeNameW(v.as_ptr(), buf.as_mut_ptr(), buf.len() as u32, &mut ret) };
    if ok == 0 {
        return vec![];
    }
    buf[..ret as usize]
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(|s| OsString::from_wide(s).to_string_lossy().into_owned())
        .collect()
}

fn device_number(h: &Handle) -> Option<STORAGE_DEVICE_NUMBER> {
    let mut n: STORAGE_DEVICE_NUMBER = unsafe { std::mem::zeroed() };
    let mut ret = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h.0,
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            null(),
            0,
            &mut n as *mut _ as *mut _,
            std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
            &mut ret,
            null_mut(),
        )
    };
    (ok != 0).then_some(n)
}

fn all_volumes() -> Vec<String> {
    let mut out = Vec::new();
    let mut name = vec![0u16; 260];
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        return out;
    }
    loop {
        out.push(from_wide(&name));
        if unsafe { FindNextVolumeW(find, name.as_mut_ptr(), name.len() as u32) } == 0 {
            break;
        }
    }
    unsafe { FindVolumeClose(find) };
    out
}

unsafe fn pwstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    from_wide(std::slice::from_raw_parts(p, n))
}

/// The image file a volume lives on, if the volume belongs to a virtual disk.
/// Unlike re-opening the image through VirtDisk, this works without admin rights.
fn volume_backing_file(volume: &str) -> io::Result<PathBuf> {
    volume_backing(volume).map(|(f, _)| f)
}

/// (backing image file, disk device path such as `\\.\CdRom1`) for a volume.
fn volume_backing(volume: &str) -> io::Result<(PathBuf, String)> {
    let vh = open_device(volume.trim_end_matches('\\'))?;
    // Ask the disk the volume lives on (storage dependencies hang off the disk device).
    let n = device_number(&vh).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no device number"))?;
    let dev = match n.DeviceType {
        FILE_DEVICE_CD_ROM => format!(r"\\.\CdRom{}", n.DeviceNumber),
        _ => format!(r"\\.\PhysicalDrive{}", n.DeviceNumber),
    };
    let dh = open_device(&dev)?;
    let file = dependency(&dh, GET_STORAGE_DEPENDENCY_FLAG_DISK_HANDLE | GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES)?;
    Ok((file, dev))
}

/// Disk device paths (e.g. `\\.\CdRom1`, `\\.\PhysicalDrive3`) currently backed by `file`.
/// A file attached twice shows up as two devices.
pub fn devices_of(file: &Path) -> Vec<String> {
    let want = file.to_string_lossy().to_lowercase();
    let mut out: Vec<String> = Vec::new();
    for vol in all_volumes() {
        if let Ok((f, dev)) = volume_backing(&vol) {
            if f.to_string_lossy().to_lowercase() == want && !out.iter().any(|d| d.eq_ignore_ascii_case(&dev)) {
                out.push(dev);
            }
        }
    }
    out
}

/// Fallback detach that targets a specific device instead of the image file, via the
/// Windows Storage Management service (`Dismount-DiskImage -DevicePath`). Needed when
/// the same file was attached twice and the path-based VirtDisk handle has gone stale.
pub fn dismount_device(device: &str) -> io::Result<()> {
    // Device paths come from our own enumeration (\\.\CdRomN / \\.\PhysicalDriveN).
    if !device.starts_with(r"\\.\") || device.contains('\'') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "unexpected device path"));
    }
    let script = format!("$ErrorActionPreference='Stop'; Dismount-DiskImage -DevicePath '{}' | Out-Null", device.to_uppercase());
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW: this also runs from the windowless helpers
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr);
        let first = msg.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("Dismount-DiskImage failed");
        Err(io::Error::other(first.to_string()))
    }
}

fn dependency(h: &Handle, flags: GET_STORAGE_DEPENDENCY_FLAG) -> io::Result<PathBuf> {
    let mut size: u32 = 4096;
    loop {
        let mut buf = vec![0u64; size as usize / 8 + 1];
        let info = buf.as_mut_ptr() as *mut STORAGE_DEPENDENCY_INFO;
        unsafe { (*info).Version = STORAGE_DEPENDENCY_INFO_VERSION_2 };
        let mut used = 0u32;
        let code = unsafe {
            GetStorageDependencyInformation(h.0, flags, (buf.len() * 8) as u32, info, &mut used)
        };
        if code == ERROR_INSUFFICIENT_BUFFER && used > size {
            size = used;
            continue;
        }
        check(code)?;
        unsafe {
            let n = (*info).NumberEntries as usize;
            let entries = std::ptr::addr_of!((*info).Anonymous.Version2Entries) as *const STORAGE_DEPENDENCY_INFO_TYPE_2;
            // Lowest ancestor level = the file actually attached (higher levels are VHD parents).
            let mut best: Option<(u32, PathBuf)> = None;
            for i in 0..n {
                let e = &*entries.add(i);
                let rel = pwstr(e.DependentVolumeRelativePath);
                if rel.is_empty() {
                    continue;
                }
                let host = pwstr(e.HostVolumeName);
                let mut hv = host.clone();
                if !hv.ends_with('\\') {
                    hv.push('\\');
                }
                let root = mount_points(&hv).into_iter().next().unwrap_or(hv);
                let p = Path::new(&root).join(rel.trim_start_matches('\\'));
                if best.as_ref().map(|(l, _)| e.AncestorLevel < *l).unwrap_or(true) {
                    best = Some((e.AncestorLevel, p));
                }
            }
            return best.map(|b| b.1).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "volume has no backing file"));
        }
    }
}

/// Every volume that lives on a virtual disk: (backing image file, mount points).
pub fn image_volumes() -> Vec<(PathBuf, Vec<String>)> {
    let mut out = Vec::new();
    for vol in all_volumes() {
        if let Ok(file) = volume_backing_file(&vol) {
            let mut mps = mount_points(&vol);
            if mps.is_empty() {
                mps.push(vol.clone());
            }
            out.push((file, mps));
        }
    }
    out
}

/// Mount points (drive letters / folders) of all volumes on the given physical device.
pub fn mount_points_for(physical_path: &str) -> Vec<String> {
    let Ok(dev) = open_device(physical_path) else { return vec![] };
    let Some(target) = device_number(&dev) else { return vec![] };
    let mut out = Vec::new();
    let mut name = vec![0u16; 260];
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        return out;
    }
    loop {
        let vol = from_wide(&name);
        // CreateFile needs the name without the trailing backslash.
        if let Ok(h) = open_device(vol.trim_end_matches('\\')) {
            if let Some(n) = device_number(&h) {
                if n.DeviceType == target.DeviceType && n.DeviceNumber == target.DeviceNumber {
                    let mps = mount_points(&vol);
                    if mps.is_empty() {
                        out.push(vol.clone());
                    } else {
                        out.extend(mps);
                    }
                }
            }
        }
        if unsafe { FindNextVolumeW(find, name.as_mut_ptr(), name.len() as u32) } == 0 {
            break;
        }
    }
    unsafe { FindVolumeClose(find) };
    out
}
