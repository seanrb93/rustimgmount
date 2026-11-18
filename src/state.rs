//! Persistent bookkeeping: which image is attached via which (possibly converted) file.

use crate::formats::NativeKind;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// The image the user asked to mount.
    pub source: PathBuf,
    /// The file actually attached (same as source, or a converted/aliased copy in the cache).
    pub attached: PathBuf,
    pub kind: NativeKind,
    /// `attached` lives in our cache and should be deleted after unmounting.
    pub temp: bool,
    pub format: String,
    pub read_only: bool,
    /// Cache folders (decompressed archives) to delete after unmounting.
    #[serde(default)]
    pub extra: Vec<PathBuf>,
    /// Extra images attached alongside this one (the audio-tracks drive of a mixed-mode
    /// disc). They are detached and deleted together with it.
    #[serde(default)]
    pub companions: Vec<PathBuf>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub mounts: Vec<Record>,
}

fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

pub fn app_dir() -> PathBuf {
    // RUSTIMGMOUNT_DATA relocates the records (used by the tests to stay away from real data).
    match std::env::var_os("RUSTIMGMOUNT_DATA").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => local_app_data().join("rustimgmount"),
    }
}

/// The program used to be called "imgmount": move its data folder over once. If that
/// fails (e.g. an image is still mounted from the old cache), the old folder is left alone.
pub fn migrate_legacy() {
    let old = local_app_data().join("imgmount");
    let new = app_dir();
    if old.is_dir() && !new.exists() {
        if let Err(e) = std::fs::rename(&old, &new) {
            eprintln!("note: could not move {} to {}: {e}", old.display(), new.display());
        }
    }
}

/// The cache folder, resolved to its real location on disk. Processes started from a
/// packaged (MSIX) app see %LOCALAPPDATA% redirected; Windows reports attached images
/// by their real path, so ours must match.
pub fn cache_dir() -> Result<PathBuf> {
    // RUSTIMGMOUNT_CACHE puts conversions on another drive (they can be large).
    let d = match std::env::var_os("RUSTIMGMOUNT_CACHE").filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => app_dir().join("cache"),
    };
    std::fs::create_dir_all(&d)?;
    let real = std::fs::canonicalize(&d).unwrap_or(d);
    let s = real.to_string_lossy();
    Ok(match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => real,
    })
}

fn state_file() -> PathBuf {
    app_dir().join("mounts.json")
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    let n = |p: &Path| {
        let s = p.to_string_lossy().replace('/', "\\");
        let s = s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s);
        s.to_lowercase()
    };
    n(a) == n(b)
}

/// (volume serial, file index): identifies a file or folder however its path is spelled.
fn file_id(p: &Path) -> Option<(u32, u64)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::*;
    let w = crate::vdisk::wide(p);
    unsafe {
        let h = CreateFileW(
            w.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        let ok = GetFileInformationByHandle(h, &mut info);
        CloseHandle(h);
        (ok != 0).then(|| (info.dwVolumeSerialNumber, ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64))
    }
}

/// Same file, compared by path first and then by identity. Needed because Windows
/// reports attached images by their real path, while a process started from a packaged
/// app sees %LOCALAPPDATA% (our cache) under a redirected path.
pub fn same_file(a: &Path, b: &Path) -> bool {
    same_path(a, b) || matches!((file_id(a), file_id(b)), (Some(x), Some(y)) if x == y)
}

/// Is `p` inside folder `dir` (by path or by identity of one of its parents)?
pub fn is_inside(p: &Path, dir: &Path) -> bool {
    p.starts_with(dir) || p.ancestors().skip(1).any(|a| a.parent().is_some() && same_file(a, dir))
}

/// Exclusive lock on the record file: a lock file created with `create_new`.
/// A lock older than 30 seconds is treated as left over from a crashed process.
struct Lock(PathBuf);

impl Lock {
    fn acquire() -> Result<Lock> {
        std::fs::create_dir_all(app_dir())?;
        let path = app_dir().join("mounts.lock");
        let start = std::time::Instant::now();
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Lock(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists || e.kind() == std::io::ErrorKind::PermissionDenied => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > std::time::Duration::from_secs(30));
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if start.elapsed() > std::time::Duration::from_secs(15) {
                        anyhow::bail!("another rustimgmount process is holding {}", path.display());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl State {
    pub fn load() -> State {
        std::fs::read(state_file())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> Result<()> {
        std::fs::create_dir_all(app_dir())?;
        let tmp = state_file().with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        if let Err(e) = std::fs::rename(&tmp, state_file()) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// Change the records safely when several rustimgmount processes run at once
    /// (mounts, unmounts, background eject watchers): take a lock, re-read the file,
    /// apply `f`, save. Never save a copy that was loaded earlier.
    pub fn modify<T>(f: impl FnOnce(&mut State) -> T) -> Result<T> {
        let _lock = Lock::acquire()?;
        let mut st = State::load();
        let out = f(&mut st);
        st.save()?;
        Ok(out)
    }

    pub fn by_source(&self, p: &Path) -> Option<&Record> {
        self.mounts.iter().find(|r| same_path(&r.source, p))
    }

    pub fn by_attached(&self, p: &Path) -> Option<&Record> {
        self.mounts.iter().find(|r| same_file(&r.attached, p))
    }

    /// The mount that `p` is a companion (audio drive) of.
    pub fn by_companion(&self, p: &Path) -> Option<&Record> {
        self.mounts.iter().find(|r| r.companions.iter().any(|c| same_file(c, p)))
    }

    pub fn remove_attached(&mut self, p: &Path) {
        self.mounts.retain(|r| !same_file(&r.attached, p));
    }

    pub fn upsert(&mut self, rec: Record) {
        self.remove_attached(&rec.attached.clone());
        self.mounts.retain(|r| !same_path(&r.source, &rec.source));
        self.mounts.push(rec);
    }
}

/// Remember a mounted image for the tray app's "Recent" list (newest first, ten at most).
pub fn add_recent(image: &Path) {
    let file = app_dir().join("recent.json");
    let mut list: Vec<String> = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let me = image.to_string_lossy().into_owned();
    list.retain(|p| !p.eq_ignore_ascii_case(&me));
    list.insert(0, me);
    list.truncate(10);
    let _ = std::fs::create_dir_all(app_dir());
    if let Ok(json) = serde_json::to_vec_pretty(&list) {
        let _ = std::fs::write(file, json);
    }
}
