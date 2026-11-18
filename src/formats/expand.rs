//! Whole-file compression that can't be read randomly (gzip, xz, zip, ECM).
//! These are decompressed once into the cache; the result is then detected like any
//! other image. Every directory created here is recorded so it is removed on unmount.

use crate::source::{open_ro, read_vec};
use crate::util::Progress;
use anyhow::{anyhow, bail, Context, Result};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static CREATED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Cache directories created (or reused) by `expand` since the last call.
pub fn take_created() -> Vec<PathBuf> {
    std::mem::take(&mut *CREATED.lock().unwrap())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Gzip,
    Xz,
    Bzip2,
    Zip,
    SevenZip,
    Rar,
    Ecm,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Gzip => "gzip",
            Kind::Xz => "xz",
            Kind::Bzip2 => "bzip2",
            Kind::Zip => "zip",
            Kind::SevenZip => "7-Zip",
            Kind::Rar => "RAR",
            Kind::Ecm => "ECM",
        }
    }
}

pub fn kind(path: &Path) -> Result<Option<Kind>> {
    let f = open_ro(path)?;
    let h = read_vec(&f, 0, 8)?;
    Ok(if h[0..2] == [0x1F, 0x8B] {
        Some(Kind::Gzip)
    } else if h[0..6] == [0xFD, b'7', b'z', b'X', b'Z', 0] {
        Some(Kind::Xz)
    } else if &h[0..4] == b"PK\x03\x04" {
        Some(Kind::Zip)
    } else if &h[0..4] == b"ECM\0" {
        Some(Kind::Ecm)
    } else if h[0..6] == [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C] {
        Some(Kind::SevenZip)
    } else if &h[0..3] == b"BZh" && (b'1'..=b'9').contains(&h[3]) && h[4..8] == [0x31, 0x41, 0x59, 0x26] {
        Some(Kind::Bzip2)
    } else if &h[0..6] == b"Rar!\x1A\x07" {
        Some(Kind::Rar)
    } else {
        None
    })
}

/// Counts bytes read, for progress reporting.
struct Counting<R> {
    inner: R,
    n: u64,
    p: Progress,
}
impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let k = self.inner.read(buf)?;
        self.n += k as u64;
        self.p.update(self.n);
        Ok(k)
    }
}

fn strip_ext(path: &Path, exts: &[&str]) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "image".into());
    for e in exts {
        if name.to_ascii_lowercase().ends_with(e) && name.len() > e.len() {
            return name[..name.len() - e.len()].to_string();
        }
    }
    format!("{name}.img")
}

/// Name stored in a gzip header (FNAME), if any.
fn gzip_name(path: &Path) -> Option<String> {
    let f = open_ro(path).ok()?;
    let h = read_vec(&f, 0, 4096).ok()?;
    let flags = h[3];
    if flags & 0x08 == 0 {
        return None;
    }
    let mut p = 10;
    if flags & 0x04 != 0 {
        p += 2 + u16::from_le_bytes([h[10], h[11]]) as usize;
    }
    let end = h[p..].iter().position(|&b| b == 0)? + p;
    let name = String::from_utf8_lossy(&h[p..end]).to_string();
    // Only the file name; never let an archive pick a directory.
    let name = name.rsplit(['/', '\\']).next()?.to_string();
    (!name.is_empty() && !name.contains("..")).then_some(name)
}

fn safe_name(n: &str) -> Option<PathBuf> {
    let p = Path::new(n);
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::Normal(s) => out.push(s),
            std::path::Component::CurDir => {}
            _ => return None, // absolute paths, "..", drive letters
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// Decompress `path` into a cache directory and return the file to mount.
/// Re-uses a previous decompression of the same (unchanged) file.
pub fn expand(path: &Path) -> Result<PathBuf> {
    let kind = kind(path)?.ok_or_else(|| anyhow!("{} is not compressed", path.display()))?;
    let dir = crate::cache_path(path, "d")?;
    let done_marker = dir.join(".complete");
    if !done_marker.exists() {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let total = std::fs::metadata(path)?.len();
        let reader = BufReader::with_capacity(1 << 20, open_ro(path)?);
        let label = format!("Decompressing {}", kind.name());
        let counting = Counting { inner: reader, n: 0, p: Progress::new(&label, total) };
        let res = match kind {
            Kind::Gzip => {
                let name = gzip_name(path).unwrap_or_else(|| strip_ext(path, &[".gz", ".gzip"]));
                copy_to(flate2::read::MultiGzDecoder::new(counting), &dir.join(name))
            }
            Kind::Xz => {
                let name = strip_ext(path, &[".xz"]);
                let mut r = BufReader::new(counting);
                let mut w = BufWriter::with_capacity(1 << 20, File::create(dir.join(name))?);
                lzma_rs::xz_decompress(&mut r, &mut w).map_err(|e| anyhow!("xz: {e:?}"))?;
                w.flush()?;
                Ok(())
            }
            Kind::Ecm => {
                let name = strip_ext(path, &[".ecm"]);
                let name = if Path::new(&name).extension().is_some() { name } else { format!("{name}.bin") };
                unecm(BufReader::new(counting), &dir.join(name))
            }
            Kind::Bzip2 => {
                let name = strip_ext(path, &[".bz2", ".bzip2"]);
                copy_to(bzip2::read::MultiBzDecoder::new(counting), &dir.join(name))
            }
            Kind::Zip => unzip(path, &dir),
            Kind::SevenZip => un7z(path, &dir),
            Kind::Rar => unrar(path, &dir),
        };
        if let Err(e) = res {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        eprintln!();
        std::fs::write(&done_marker, b"")?;
    }
    CREATED.lock().unwrap().push(dir.clone());
    main_file(&dir)
}

fn copy_to<R: Read>(mut r: R, out: &Path) -> Result<()> {
    let mut w = BufWriter::with_capacity(1 << 20, File::create(out)?);
    io::copy(&mut r, &mut w).with_context(|| format!("decompressing into {}", out.display()))?;
    w.flush()?;
    Ok(())
}

/// Archives state their unpacked size up front: refuse before filling the drive
/// (this also stops "zip bombs" that claim terabytes).
fn check_room(dir: &Path, total: u64) -> Result<()> {
    if let Some(free) = crate::vdisk::free_space(dir) {
        if total.saturating_add(64 << 20) > free {
            bail!(
                "the archive unpacks to {}, but only {} is free on that drive (set RUSTIMGMOUNT_CACHE to use another drive)",
                crate::util::human(total),
                crate::util::human(free)
            );
        }
    }
    Ok(())
}

fn unzip(path: &Path, dir: &Path) -> Result<()> {
    let mut z = zip::ZipArchive::new(BufReader::new(open_ro(path)?)).context("reading zip")?;
    let total: u64 = (0..z.len()).filter_map(|i| z.by_index_raw(i).ok().map(|f| f.size())).sum();
    check_room(dir, total)?;
    let mut p = Progress::new("Extracting zip", total);
    let mut done = 0;
    for i in 0..z.len() {
        let mut f = z.by_index(i)?;
        if f.is_dir() {
            continue;
        }
        let Some(rel) = safe_name(f.name()) else { continue };
        let out = dir.join(rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&out)?);
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n])?;
            done += n as u64;
            p.update(done);
        }
        w.flush()?;
    }
    p.finish();
    Ok(())
}

fn un7z(path: &Path, dir: &Path) -> Result<()> {
    use sevenz_rust2::{ArchiveReader, Password};
    let mut a = ArchiveReader::open(path, Password::empty()).map_err(|e| anyhow!("reading 7z: {e}"))?;
    let total: u64 = a.archive().files.iter().map(|f| f.size()).sum();
    check_room(dir, total)?;
    let mut p = Progress::new("Extracting 7z", total);
    let mut done = 0u64;
    let mut err: Option<anyhow::Error> = None;
    a.for_each_entries(|entry, reader| {
        if entry.is_directory() || !entry.has_stream() {
            return Ok(true);
        }
        let Some(rel) = safe_name(entry.name()) else { return Ok(true) };
        let out = dir.join(rel);
        let res = (|| -> Result<()> {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut w = BufWriter::with_capacity(1 << 20, File::create(&out)?);
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                w.write_all(&buf[..n])?;
                done += n as u64;
                p.update(done);
            }
            w.flush()?;
            Ok(())
        })();
        match res {
            Ok(()) => Ok(true),
            Err(e) => {
                err = Some(e);
                Ok(false)
            }
        }
    })
    .map_err(|e| match e {
        sevenz_rust2::Error::PasswordRequired => anyhow!("this 7z archive is password-protected"),
        e => anyhow!("extracting 7z: {e}"),
    })?;
    if let Some(e) = err {
        return Err(e);
    }
    p.finish();
    Ok(())
}

/// RAR (v4 and v5, including multi-part sets: game.part1.rar, .r00, ...).
/// Decompression is RARLAB's UnRAR code; its license is in LICENSE-UNRAR.txt:
/// "UnRAR source code may be used in any software to handle RAR archives without
/// limitations free of charge, but cannot be used to develop RAR (WinRAR) compatible
/// archiver and to re-create RAR compression algorithm, which is proprietary.
/// Distribution of modified UnRAR source code in separate form or as a part of other
/// software is permitted, provided that full text of this paragraph, starting from
/// "UnRAR source code" words, is included in license, or in documentation if license is
/// not available, and in source code comments of resulting package."
fn unrar(path: &Path, dir: &Path) -> Result<()> {
    let rar_err = |e: unrar::error::UnrarError| anyhow!("RAR: {e}");
    // List first: total size for the space check, and spot password protection.
    let mut total = 0u64;
    for e in unrar::Archive::new(path).as_first_part().open_for_listing().map_err(rar_err)? {
        let e = e.map_err(rar_err)?;
        if e.is_encrypted() {
            bail!("this RAR archive is password-protected");
        }
        total += e.unpacked_size;
    }
    check_room(dir, total)?;
    let mut p = Progress::new("Extracting RAR", total);
    let mut done = 0u64;
    let mut arc = unrar::Archive::new(path).as_first_part().open_for_processing().map_err(rar_err)?;
    while let Some(h) = arc.read_header().map_err(rar_err)? {
        let e = h.entry();
        let target = e.is_file().then(|| safe_name(&e.filename.to_string_lossy())).flatten().map(|rel| dir.join(rel));
        let size = e.unpacked_size;
        arc = match target {
            Some(out) => {
                if let Some(parent) = out.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let next = h.extract_to(&out).map_err(rar_err)?;
                done += size;
                p.update(done);
                next
            }
            None => h.skip().map_err(rar_err)?,
        };
    }
    p.finish();
    Ok(())
}

/// Pick the file to mount from an expanded directory: a sheet (cue/gdi/mds/ccd) if
/// present, otherwise the largest file.
fn main_file(dir: &Path) -> Result<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)?.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().map(|n| n != ".complete").unwrap_or(false) {
                let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                files.push((p, len));
            }
        }
    }
    for sheet in ["cue", "gdi", "mds", "ccd", "vmdk"] {
        if let Some((p, _)) = files.iter().find(|(p, _)| super::ext(p) == sheet) {
            // A .vmdk is only a sheet when it is a small text descriptor.
            if sheet != "vmdk" || files.iter().find(|(q, _)| q == p).map(|f| f.1 < 64 << 10).unwrap_or(false) {
                return Ok(p.clone());
            }
        }
    }
    files
        .into_iter()
        .max_by_key(|f| f.1)
        .map(|f| f.0)
        .ok_or_else(|| anyhow!("the archive is empty"))
}

// ---------------------------------------------------------------- ECM

/// Decode an ECM (Error Code Modeler) stream. EDC/ECC fields are written as zeros:
/// they are regenerated data that mounting never reads.
fn unecm<R: Read>(mut r: R, out: &Path) -> Result<()> {
    let mut w = BufWriter::with_capacity(1 << 20, File::create(out)?);
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != b"ECM\0" {
        bail!("not an ECM file");
    }
    fn take<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<()> {
        r.read_exact(buf).context("truncated ECM stream")
    }
    let mut bulk = vec![0u8; 1 << 16];
    loop {
        let mut b = [0u8; 1];
        take(&mut r, &mut b)?;
        let mut c = b[0] as u32;
        let ty = c & 3;
        let mut num = (c >> 2) & 0x1F;
        let mut bits = 5;
        while c & 0x80 != 0 {
            take(&mut r, &mut b)?;
            c = b[0] as u32;
            if bits > 31 {
                bail!("corrupt ECM stream");
            }
            num |= (c & 0x7F) << bits;
            bits += 7;
        }
        if num == 0xFFFF_FFFF {
            break; // end marker (followed by an EDC we don't need)
        }
        let num = num as u64 + 1;
        match ty {
            0 => {
                // Raw bytes.
                let mut left = num;
                while left > 0 {
                    let n = left.min(bulk.len() as u64) as usize;
                    take(&mut r, &mut bulk[..n])?;
                    w.write_all(&bulk[..n])?;
                    left -= n as u64;
                }
            }
            1 => {
                // Mode 1: address (3) + user data (2048) -> full 2352-byte sector.
                let mut s = [0u8; 2352];
                s[1..11].fill(0xFF);
                s[15] = 1;
                for _ in 0..num {
                    take(&mut r, &mut s[12..15])?;
                    take(&mut r, &mut s[16..16 + 2048])?;
                    w.write_all(&s)?;
                }
            }
            2 | 3 => {
                // Mode 2 form 1/2 without sync/header (2336 bytes):
                // subheader copy + subheader + data (+ EDC/ECC left as zeros).
                let body = if ty == 2 { 0x804 } else { 0x918 };
                let mut s = [0u8; 2336];
                for _ in 0..num {
                    take(&mut r, &mut s[4..4 + body])?;
                    s.copy_within(4..8, 0);
                    w.write_all(&s)?;
                }
            }
            _ => unreachable!(),
        }
    }
    w.flush()?;
    Ok(())
}
