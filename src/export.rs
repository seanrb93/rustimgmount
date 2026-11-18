//! `rustimgmount convert --to <format>`: the output formats beyond the ISO/VHD/VHDX that
//! mounting itself uses.
//!
//! Discs: ISO, CSO and ZSO hold the data track only; CHD, BIN/CUE, CCD/IMG/SUB and GDI keep
//! every track (audio included) as stored. Disks: VHD, VHDX, raw IMG, VMDK, VDI, QCOW2, CHD.
//!
//! Everything is written to `<name>.partial` and renamed when complete, so a cancelled
//! or failed conversion never leaves something that looks like a finished image.

use crate::formats::layout::{self, Layout, RawTrack};
use crate::formats::{self, Detected, NativeKind, Plan};
use crate::source::{FileSlice, Source};
use crate::util::{random_bytes, Progress};
use anyhow::{anyhow, bail, Context, Result};
use chd::header::CodecType;
use chd::metadata::KnownMetadata;
use chd::write::{ChdWriteOptions, ChdWriter, CompressedHunk, HunkCompressor};
use std::ffi::CString;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Iso,
    Cso,
    Zso,
    Chd,
    Cue,
    Ccd,
    Gdi,
    Vhd,
    Vhdx,
    Img,
    Vmdk,
    Vdi,
    Qcow2,
}

/// A flavour of a target format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    Plain,
    /// CHD compressed with Zstandard (bigger than the default LZMA, much faster to read).
    Zstd,
    /// VHD with all its space allocated up front.
    Fixed,
    /// QCOW2 with deflated clusters.
    Compressed,
}

/// One choice of output: what `--to` accepts and what the menus list.
pub struct Format {
    pub id: &'static str,
    pub target: Target,
    pub variant: Variant,
    pub label: &'static str,
}

const fn f(id: &'static str, target: Target, variant: Variant, label: &'static str) -> Format {
    Format { id, target, variant, label }
}

pub const FORMATS: [Format; 16] = [
    f("iso", Target::Iso, Variant::Plain, "ISO (data only)"),
    f("cso", Target::Cso, Variant::Plain, "CSO (compressed ISO)"),
    f("zso", Target::Zso, Variant::Plain, "ZSO (compressed ISO, loads faster)"),
    f("chd", Target::Chd, Variant::Plain, "CHD"),
    f("chd-zstd", Target::Chd, Variant::Zstd, "CHD (Zstandard, loads faster)"),
    f("cue", Target::Cue, Variant::Plain, "BIN/CUE"),
    f("ccd", Target::Ccd, Variant::Plain, "CCD/IMG/SUB (CloneCD)"),
    f("gdi", Target::Gdi, Variant::Plain, "GDI (Dreamcast)"),
    f("vhd", Target::Vhd, Variant::Plain, "VHD"),
    f("vhd-fixed", Target::Vhd, Variant::Fixed, "VHD (fixed size)"),
    f("vhdx", Target::Vhdx, Variant::Plain, "VHDX"),
    f("img", Target::Img, Variant::Plain, "IMG (raw)"),
    f("vmdk", Target::Vmdk, Variant::Plain, "VMDK (VMware)"),
    f("vdi", Target::Vdi, Variant::Plain, "VDI (VirtualBox)"),
    f("qcow2", Target::Qcow2, Variant::Plain, "QCOW2 (QEMU)"),
    f("qcow2-compressed", Target::Qcow2, Variant::Compressed, "QCOW2 (compressed)"),
];

impl Format {
    pub fn parse(s: &str) -> Option<&'static Format> {
        let s = s.trim().trim_start_matches('.').to_ascii_lowercase();
        let s = match s.as_str() {
            "bin" | "bincue" | "bin/cue" => "cue",
            "raw" | "dd" | "ima" => "img",
            "qcow" => "qcow2",
            "clonecd" => "ccd",
            other => other,
        };
        FORMATS.iter().find(|f| f.id == s)
    }

    pub fn of(target: Target) -> &'static Format {
        FORMATS.iter().find(|f| f.target == target && f.variant == Variant::Plain).unwrap()
    }

    pub fn ids() -> String {
        FORMATS.iter().map(|f| f.id).collect::<Vec<_>>().join(", ")
    }
}

impl Target {
    /// Extension of the (main) output file.
    pub fn ext(self) -> &'static str {
        Format::of(self).id
    }
}

const DISC_TARGETS: &str = "iso, cso, zso, chd, cue or ccd";
const DISK_TARGETS: &str = "vhd, vhdx, img, vmdk, vdi, qcow2 or chd";

/// What an image holds, as far as choosing an output format goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Disc,
    GdRom,
    Disk,
    /// Not looked into (archives would have to be unpacked first).
    Unknown,
}

pub fn kind_of(image: &Path) -> Result<Kind> {
    let (base, _) = formats::split_disc(image);
    if formats::multi(image)?.is_some() {
        return Ok(Kind::Disc);
    }
    if formats::expand::kind(&base)?.is_some() {
        return Ok(Kind::Unknown);
    }
    let det = formats::detect(image);
    let path = det.as_ref().map(|d| d.data_path.clone()).unwrap_or_else(|_| image.to_owned());
    Ok(match det.map(|d| d.plan) {
        Ok(Plan::Disk(_) | Plan::Native(NativeKind::Vhd | NativeKind::Vhdx)) => Kind::Disk,
        Ok(_) => match layout::parse(&path) {
            Ok(Some(l)) if l.gd => Kind::GdRom,
            _ => Kind::Disc,
        },
        Err(e) => match layout::parse(&path) {
            Ok(Some(l)) if l.gd => Kind::GdRom,
            Ok(Some(_)) => Kind::Disc, // e.g. an audio CD: nothing to mount, but it has tracks
            _ => return Err(e),
        },
    })
}

/// The formats an image of this kind (and file extension) can be converted to.
pub fn choices(kind: Kind, ext: &str) -> Vec<&'static Format> {
    use Target::*;
    FORMATS
        .iter()
        // Not the image's own format, except as a different flavour of it (recompressing).
        .filter(|f| f.target.ext() != ext || f.variant != Variant::Plain || matches!(f.target, Chd | Vhd | Qcow2))
        .filter(|f| match kind {
            Kind::Disc => matches!(f.target, Iso | Cso | Zso | Chd | Cue | Ccd),
            Kind::GdRom => matches!(f.target, Iso | Cso | Zso | Chd | Gdi),
            Kind::Disk => matches!(f.target, Vhd | Vhdx | Img | Vmdk | Vdi | Qcow2),
            Kind::Unknown => true,
        })
        .collect()
}

/// Every file an image consists of (a cue sheet and its .bin files, ...). `None` when
/// that can't be told reliably (split or multi-part images).
pub fn source_files(image: &Path) -> Option<Vec<PathBuf>> {
    let (base, disc) = formats::split_disc(image);
    if disc.is_some() {
        return None;
    }
    let ext = formats::ext(&base);
    let sibling = |e: &str| [e.to_string(), e.to_uppercase()].iter().map(|e| base.with_extension(e)).find(|p| p.exists());
    let mut files = vec![base.clone()];
    match ext.as_str() {
        "cue" => files.extend(formats::cd::parse_cue(&base).ok()?.0.into_iter().map(|t| t.file)),
        "gdi" => {
            for line in std::fs::read_to_string(&base).ok()?.lines().skip(1) {
                let rest = line.trim().splitn(5, char::is_whitespace).nth(4).unwrap_or("").trim();
                let name = match rest.strip_prefix('"') {
                    Some(r) => r.split('"').next().unwrap_or(""),
                    None => rest.split_whitespace().next().unwrap_or(""),
                };
                if !name.is_empty() {
                    files.push(formats::cd::resolve_sibling(&base, name)?);
                }
            }
        }
        "mds" | "mdf" => {
            files.extend(sibling(if ext == "mds" { "mdf" } else { "mds" }));
            let mdf = if ext == "mdf" { base.clone() } else { sibling("mdf")? };
            if formats::pieces::mdf_parts(&mdf).len() > 1 {
                return None;
            }
        }
        "ccd" | "sub" => files.extend(["ccd", "img", "sub"].iter().filter(|e| **e != ext).filter_map(|e| sibling(e))),
        "img" if sibling("ccd").is_some() => files.extend(["ccd", "sub"].iter().filter_map(|e| sibling(e))),
        "m3u" | "m3u8" | "001" | "e01" | "ex01" | "pbp" => return None,
        "vmdk" => {
            if crate::source::read_vec(&crate::source::open_ro(&base).ok()?, 0, 4).ok()? != b"KDMV" {
                return None; // a descriptor that points at other files
            }
        }
        _ => {}
    }
    files.sort();
    files.dedup();
    Some(files)
}

// ---------------------------------------------------------------- unfinished files

/// The output files of one conversion, written as `.partial` until `commit`.
pub struct Job {
    files: Vec<(PathBuf, PathBuf)>,
    marker: PathBuf,
    done: bool,
    overwrite: bool,
    /// The image being converted: never a valid place to write.
    protected: Vec<PathBuf>,
}

fn marker_dir() -> PathBuf {
    crate::state::app_dir().join("converting")
}

impl Job {
    pub fn new(overwrite: bool, protected: Vec<PathBuf>) -> Job {
        Job { files: Vec::new(), marker: marker_dir().join(format!("{}.txt", std::process::id())), done: false, overwrite, protected }
    }

    /// Reserve an output file; returns the temporary path to write to.
    pub fn add(&mut self, target: &Path) -> Result<PathBuf> {
        if target.exists() {
            if self.protected.iter().any(|p| crate::state::same_file(p, target)) {
                bail!("{} is part of the image being converted; choose another name or folder for the result", target.display());
            }
            if !self.overwrite {
                bail!("{} already exists", target.display());
            }
        }
        let mut name = target.file_name().unwrap_or_default().to_os_string();
        name.push(".partial");
        let tmp = target.with_file_name(name);
        self.files.push((tmp.clone(), target.to_owned()));
        // Recorded so that, if this process is killed (Cancel in the progress dialog), the
        // next rustimgmount command can delete the half-written files.
        let _ = std::fs::create_dir_all(marker_dir());
        let list: String = self.files.iter().map(|(t, _)| format!("{}\n", t.display())).collect();
        let _ = std::fs::write(&self.marker, list);
        Ok(tmp)
    }

    /// Everything was written: give the files their real names.
    pub fn commit(mut self) -> Result<Vec<PathBuf>> {
        for (tmp, target) in &self.files {
            std::fs::rename(tmp, target).with_context(|| format!("creating {}", target.display()))?;
        }
        self.done = true;
        let _ = std::fs::remove_file(&self.marker);
        Ok(self.files.iter().map(|(_, t)| t.clone()).collect())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if !self.done {
            for (tmp, _) in &self.files {
                let _ = std::fs::remove_file(tmp);
            }
            let _ = std::fs::remove_file(&self.marker);
        }
    }
}

/// Delete the half-written files of conversions whose process is gone.
pub fn sweep() {
    let Ok(dir) = std::fs::read_dir(marker_dir()) else { return };
    for e in dir.flatten() {
        let path = e.path();
        let pid = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u32>().ok());
        if pid.is_some_and(crate::winutil::process_alive) {
            continue;
        }
        for line in std::fs::read_to_string(&path).unwrap_or_default().lines() {
            if line.ends_with(".partial") {
                let _ = std::fs::remove_file(line);
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

// ---------------------------------------------------------------- entry point

#[derive(Clone, Copy, Default)]
pub struct Opts {
    /// Replace existing output files.
    pub overwrite: bool,
    /// Read the result back and compare it with the source.
    pub verify: bool,
}

/// What is being converted, in the form the target needs.
enum Content {
    /// The data of a disc, 2048 bytes per sector.
    Disc(Box<dyn Source>),
    Disk(Box<dyn Source>),
    /// Every track of a disc.
    Tracks(Layout),
}

fn open_content(image: &Path, det: Result<Detected>, target: Target) -> Result<Content> {
    use Target::*;
    match target {
        Iso | Cso | Zso => {
            let det = det?;
            match det.plan {
                Plan::Cd(d) => Ok(Content::Disc(Box::new(d))),
                // A plain ISO has no decoder in front of it: read the file itself.
                Plan::Native(NativeKind::Iso) => Ok(Content::Disc(Box::new(FileSlice::whole(&det.data_path)?))),
                _ => bail!("this is a hard disk or floppy image, not a disc; convert it to {DISK_TARGETS}"),
            }
        }
        Chd | Cue | Ccd | Gdi => {
            // Whole-disc formats work from the track list, which a disc can have even when
            // Windows has nothing to mount on it (an audio CD).
            let path = det.as_ref().map(|d| d.data_path.clone()).unwrap_or_else(|_| image.to_owned());
            let native_disk = match &det {
                Ok(Detected { plan: Plan::Native(NativeKind::Vhd | NativeKind::Vhdx), data_path, .. }) => Some(formats::vhd::open(data_path)?),
                _ => None,
            };
            let (disk, det) = match (native_disk, det) {
                (None, Ok(Detected { plan: Plan::Disk(s), .. })) => (Some(s), None),
                (d, det) => (d, Some(det)),
            };
            if let Some(s) = disk {
                if target != Chd {
                    bail!("this is a hard disk or floppy image, not a disc; convert it to {DISK_TARGETS}");
                }
                return Ok(Content::Disk(s));
            }
            Ok(Content::Tracks(match (layout::parse(&path)?, det.unwrap()) {
                (Some(l), _) => l,
                (None, Ok(Detected { plan: Plan::Cd(d), .. })) => layout::single(d),
                (None, Ok(d)) => layout::cooked(FileSlice::whole(&d.data_path)?),
                (None, Err(e)) => return Err(e),
            }))
        }
        Vhd | Vhdx | Img | Vmdk | Vdi | Qcow2 => {
            let det = det?;
            match det.plan {
                Plan::Disk(s) => Ok(Content::Disk(s)),
                Plan::Native(NativeKind::Vhd | NativeKind::Vhdx) => Ok(Content::Disk(formats::vhd::open(&det.data_path)?)),
                _ => bail!("this is a CD/DVD image, not a disk; convert it to {DISC_TARGETS}"),
            }
        }
    }
}

/// Convert a detected image, writing `out` (plus track files for BIN/CUE, CCD and GDI).
/// Returns the files written.
pub fn run(image: &Path, det: Result<Detected>, format: &Format, out: &Path, opts: Opts) -> Result<Vec<PathBuf>> {
    use Target::*;
    let target = format.target;
    let zstd = format.variant == Variant::Zstd;
    let mut content = open_content(image, det, target)?;
    let mut job = Job::new(opts.overwrite, source_files(image).unwrap_or_else(|| vec![formats::split_disc(image).0]));
    let dir = out.parent().unwrap_or(Path::new(".")).to_owned();
    match (&mut content, target) {
        (Content::Disc(src), Iso) => {
            crate::convert::check_free_space(&dir, src.len())?;
            write_raw(src.as_mut(), &job.add(out)?, "Converting to ISO", None)?;
        }
        (Content::Disc(src), Cso | Zso) => write_packed(src.as_mut(), &job.add(out)?, target == Zso)?,
        (Content::Disk(src), Chd) => write_chd_disk(src.as_mut(), &job.add(out)?, zstd)?,
        (Content::Tracks(l), Chd) => write_chd_cd(l, &job.add(out)?, zstd)?,
        (Content::Tracks(l), Cue | Gdi | Ccd) => {
            if l.gd && target != Gdi {
                bail!("this is a Dreamcast GD-ROM, which a {} can't describe; convert it to gdi or chd", if target == Cue { "cue sheet" } else { "CloneCD image" });
            }
            if !l.gd && target == Gdi {
                bail!("only Dreamcast GD-ROM images can be converted to GDI; convert this one to {DISC_TARGETS}");
            }
            if target == Ccd {
                write_ccd(l, out, &mut job)?;
            } else if let Some(t) = lone_bin(image, l, out) {
                // The .bin is already what a cue sheet would point at: write just the sheet.
                let name = formats::split_disc(image).0.file_name().unwrap_or_default().to_string_lossy().into_owned();
                std::fs::write(job.add(out)?, format!("FILE \"{name}\" BINARY\r\n  TRACK 01 {}\r\n    INDEX 01 00:00:00\r\n", t.cue_type()))?;
            } else {
                write_tracks(l, out, target == Gdi, &mut job)?;
            }
        }
        (Content::Disk(src), _) => {
            let tmp = job.add(out)?;
            match (target, format.variant) {
                (Vhd, Variant::Fixed) => {
                    let size = src.len().div_ceil(512) * 512;
                    if size > crate::convert::VHD_MAX {
                        bail!("this disk is larger than VHD's 2040 GiB limit; convert it to vhdx instead");
                    }
                    crate::convert::check_free_space(&dir, size)?;
                    write_raw(src.as_mut(), &tmp, "Converting to VHD", Some(&crate::convert::vhd_footer(size, false)))?;
                }
                (Vhd, _) => crate::convert::write_vhd(src.as_mut(), &tmp, false)?,
                (Vhdx, _) => crate::convert::write_vhdx(src.as_mut(), &tmp, false)?,
                (Img, _) => {
                    crate::convert::check_free_space(&dir, src.len())?;
                    write_raw(src.as_mut(), &tmp, "Converting to IMG", None)?;
                }
                (Vmdk, _) => write_vmdk(src.as_mut(), &tmp, out)?,
                (Vdi, _) => write_vdi(src.as_mut(), &tmp)?,
                (_, v) => write_qcow2(src.as_mut(), &tmp, v == Variant::Compressed)?,
            }
        }
        _ => unreachable!("open_content pairs every target with its kind of content"),
    }
    let mut written = job.commit()?;
    // Subchannel patch files for copy-protected PlayStation discs (LibCrypt) are looked
    // up by name next to the image, so they follow it.
    if matches!(content, Content::Tracks(_)) {
        let base = formats::split_disc(image).0;
        for ext in ["sbi", "lsd"] {
            let (from, to) = (base.with_extension(ext), out.with_extension(ext));
            if from.exists() && !to.exists() && std::fs::copy(&from, &to).is_ok() {
                written.push(to);
            }
        }
    }
    // Test hook: damage the result, to prove that verification notices.
    if std::env::var_os("RUSTIMGMOUNT_TEST_DAMAGE").is_some() {
        if let Some(f) = written.iter().max_by_key(|f| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0)) {
            let file = std::fs::OpenOptions::new().read(true).write(true).open(f)?;
            let at = file.metadata()?.len() * 2 / 3;
            let mut b = [0u8; 64];
            let n = file.seek_read(&mut b, at)?;
            b.iter_mut().for_each(|x| *x ^= 0x5A);
            file.seek_write(&b[..n], at)?;
        }
    }
    if opts.verify {
        if let Err(e) = verify(&mut content, target, out) {
            for f in &written {
                let _ = std::fs::remove_file(f);
            }
            return Err(e.context("the converted image did not read back the same as the original, so it was deleted"));
        }
    }
    Ok(written)
}

/// The one track of an image that is a bare file of raw sectors, when the .bin a cue
/// sheet at `out` would be written with is that very file.
fn lone_bin<'a>(image: &Path, l: &'a Layout, out: &Path) -> Option<&'a RawTrack> {
    let base = formats::split_disc(image).0;
    let [t] = l.tracks.as_slice() else { return None };
    let len = std::fs::metadata(&base).ok()?.len();
    let mut whole_file = t.base == 0 && t.stride == t.size && !t.audio && len == t.frames * t.size as u64;
    if whole_file && t.size == 2048 {
        // A plain 2048-byte image is sometimes presented rearranged (multi-session discs).
        // Only when the track really reads the same as the file is the file itself the track.
        let (mut file, mut a, mut b) = (FileSlice::whole(&base).ok()?, Vec::new(), vec![0u8; 2048]);
        for sector in [0, 16, 17, t.frames / 2, t.frames.saturating_sub(1)] {
            if sector < t.frames {
                t.read(sector, 1, &mut a).ok()?;
                file.read_at(sector * 2048, &mut b).ok()?;
                whole_file &= a == b;
            }
        }
    }
    (whole_file && crate::state::same_file(&base, &out.with_extension("bin"))).then_some(t)
}

fn threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(16)
}

// ---------------------------------------------------------------- verification

fn compare(a: &mut dyn Source, b: &mut dyn Source) -> Result<()> {
    const CHUNK: usize = 4 << 20;
    let len = a.len();
    if b.len() < len {
        bail!("the result is {} bytes, the original {}", b.len(), len);
    }
    let (mut x, mut y) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    let mut p = Progress::new("Verifying", len);
    let mut pos = 0u64;
    while pos < len {
        let n = ((len - pos) as usize).min(CHUNK);
        a.read_at(pos, &mut x[..n])?;
        b.read_at(pos, &mut y[..n])?;
        if x[..n] != y[..n] {
            let at = pos + x[..n].iter().zip(&y[..n]).position(|(p, q)| p != q).unwrap_or(0) as u64;
            bail!("the data differs at byte {at}");
        }
        pos += n as u64;
        p.update(pos);
    }
    p.finish();
    Ok(())
}

/// Read the finished output back with this program's readers and compare it with what
/// was converted.
fn verify(content: &mut Content, target: Target, out: &Path) -> Result<()> {
    use Target::*;
    match content {
        Content::Disc(src) => {
            let mut back: Box<dyn Source> = match target {
                Iso => Box::new(FileSlice::whole(out)?),
                _ => Box::new(formats::packed::Packed::open(out)?),
            };
            compare(src.as_mut(), back.as_mut())
        }
        Content::Disk(src) => {
            let mut back: Box<dyn Source> = match target {
                Vhd | Vhdx => formats::vhd::open(out)?,
                Vmdk => formats::vmdk::open(out)?,
                Vdi => Box::new(formats::vdi::Vdi::open(out)?),
                Qcow2 => Box::new(formats::qcow2::Qcow2::open(out)?),
                Chd => match formats::chd::open(out)? {
                    formats::chd::Opened::Disk(s) => s,
                    _ => bail!("the result is not a hard disk CHD"),
                },
                _ => Box::new(FileSlice::whole(out)?),
            };
            compare(src.as_mut(), back.as_mut())
        }
        Content::Tracks(l) => {
            let back = match layout::parse(out)? {
                Some(b) => b,
                // A DVD CHD has no track list.
                None => match formats::detect(out)?.plan {
                    Plan::Cd(d) => layout::single(d),
                    _ => bail!("the result is not a disc image"),
                },
            };
            if back.tracks.len() != l.tracks.len() {
                bail!("the result has {} tracks, the original {}", back.tracks.len(), l.tracks.len());
            }
            let total: u64 = l.tracks.iter().map(|t| t.frames * t.size as u64).sum();
            let mut p = Progress::new("Verifying", total);
            let (mut done, mut x, mut y) = (0u64, Vec::new(), Vec::new());
            for (a, b) in l.tracks.iter().zip(&back.tracks) {
                if (a.audio, a.size, a.frames) != (b.audio, b.size, b.frames) {
                    bail!("track {} came out as {} sectors of {} bytes, not {} of {}", a.number, b.frames, b.size, a.frames, a.size);
                }
                let keeps_sub = matches!(target, Chd | Ccd) && a.sub.is_some();
                let mut frame = 0u64;
                while frame < a.frames {
                    let n = (a.frames - frame).min(1024) as usize;
                    a.read(frame, n, &mut x)?;
                    b.read(frame, n, &mut y)?;
                    if x != y {
                        bail!("track {} differs near sector {frame}", a.number);
                    }
                    if keeps_sub {
                        a.read_sub(frame, n, true, &mut x)?;
                        if !b.read_sub(frame, n, true, &mut y)? || x != y {
                            bail!("the subchannel data of track {} differs near sector {frame}", a.number);
                        }
                    }
                    frame += n as u64;
                    done += (n * a.size as usize) as u64;
                    p.update(done);
                }
            }
            p.finish();
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- raw, CSO, ZSO

/// Copy the source as it is, optionally followed by a trailer (a fixed VHD's footer).
fn write_raw(src: &mut dyn Source, out: &Path, label: &str, trailer: Option<&[u8]>) -> Result<()> {
    const CHUNK: usize = 2 << 20;
    let total = src.len();
    let mut w = BufWriter::with_capacity(CHUNK, File::create(out)?);
    let mut buf = vec![0u8; CHUNK];
    let mut p = Progress::new(label, total);
    let mut pos = 0;
    while pos < total {
        let n = ((total - pos) as usize).min(CHUNK);
        src.read_at(pos, &mut buf[..n])?;
        w.write_all(&buf[..n])?;
        pos += n as u64;
        p.update(pos);
    }
    if let Some(t) = trailer {
        w.write_all(&vec![0u8; ((512 - total % 512) % 512) as usize])?;
        w.write_all(t)?;
    }
    w.flush()?;
    p.finish();
    Ok(())
}

/// CSO (CISO version 1, deflate) or ZSO (LZ4): 2048-byte blocks, each compressed on its own.
fn write_packed(src: &mut dyn Source, out: &Path, lz4: bool) -> Result<()> {
    use flate2::{write::DeflateEncoder, Compression};
    const BLOCK: usize = 2048;
    let total = src.len();
    if total > 256 << 30 {
        bail!("the disc is too large for this format");
    }
    let blocks = total.div_ceil(BLOCK as u64) as usize;
    let index_len = (blocks as u64 + 1) * 4;
    // Offsets are 31 bits; larger files store them shifted, with blocks aligned to match.
    let mut align = 0u8;
    while (24 + index_len + total + ((blocks as u64) << align)) >> align >= 1 << 31 {
        align += 1;
    }
    let mut header = [0u8; 24];
    header[0..4].copy_from_slice(if lz4 { b"ZISO" } else { b"CISO" });
    header[4..8].copy_from_slice(&24u32.to_le_bytes());
    header[8..16].copy_from_slice(&total.to_le_bytes());
    header[16..20].copy_from_slice(&(BLOCK as u32).to_le_bytes());
    header[20] = 1;
    header[21] = align;
    let mut index: Vec<u32> = Vec::with_capacity(blocks + 1);
    let mut w = BufWriter::with_capacity(4 << 20, File::create(out)?);
    w.write_all(&header)?;
    w.write_all(&vec![0u8; index_len as usize])?;
    let mut pos = 24 + index_len;

    let per_batch = threads() * 2048; // blocks
    let mut buf = vec![0u8; per_batch * BLOCK];
    let mut p = Progress::new(if lz4 { "Converting to ZSO" } else { "Converting to CSO" }, total);
    let mut done = 0usize;
    while done < blocks {
        let n = (blocks - done).min(per_batch);
        let bytes = ((total - (done * BLOCK) as u64) as usize).min(n * BLOCK);
        src.read_at((done * BLOCK) as u64, &mut buf[..bytes])?;
        buf[bytes..n * BLOCK].fill(0);
        let share = n.div_ceil(threads());
        // Each worker returns its blocks' bytes back to back, with (length, stored raw) per block.
        let parts: Vec<std::io::Result<(Vec<u8>, Vec<(u32, bool)>)>> = std::thread::scope(|s| {
            let hs: Vec<_> = buf[..n * BLOCK]
                .chunks(share * BLOCK)
                .map(|part| {
                    s.spawn(move || {
                        let (mut data, mut lens) = (Vec::with_capacity(part.len()), Vec::new());
                        for b in part.chunks(BLOCK) {
                            let c = if lz4 {
                                lz4_flex::block::compress(b)
                            } else {
                                let mut e = DeflateEncoder::new(Vec::with_capacity(BLOCK), Compression::best());
                                e.write_all(b)?;
                                e.finish()?
                            };
                            let raw = c.len() >= BLOCK;
                            data.extend_from_slice(if raw { b } else { &c });
                            lens.push((if raw { BLOCK } else { c.len() } as u32, raw));
                        }
                        Ok((data, lens))
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(std::io::Error::other("compression thread failed")))).collect()
        });
        for part in parts {
            let (data, lens) = part?;
            let mut at = 0usize;
            for (len, raw) in lens {
                let aligned = pos.div_ceil(1 << align) << align;
                w.write_all(&vec![0u8; (aligned - pos) as usize])?;
                pos = aligned;
                index.push((pos >> align) as u32 | if raw { 0x8000_0000 } else { 0 });
                w.write_all(&data[at..at + len as usize])?;
                at += len as usize;
                pos += len as u64;
            }
        }
        done += n;
        p.update(((done * BLOCK) as u64).min(total));
    }
    let aligned = pos.div_ceil(1 << align) << align;
    w.write_all(&vec![0u8; (aligned - pos) as usize])?;
    index.push((aligned >> align) as u32);
    let file = w.into_inner().map_err(|e| e.into_error())?;
    let raw: Vec<u8> = index.iter().flat_map(|v| v.to_le_bytes()).collect();
    file.seek_write(&raw, 24)?;
    p.finish();
    Ok(())
}

// ---------------------------------------------------------------- CHD

fn chd_err(e: chd::Error) -> anyhow::Error {
    anyhow!("writing CHD: {e}")
}

/// Feed `total` bytes from `fill` through the CHD writer, compressing on all cores.
fn chd_pump(mut w: ChdWriter<BufWriter<File>>, hunk: usize, total: u64, mut fill: impl FnMut(&mut [u8]) -> Result<()>) -> Result<()> {
    let n_threads = threads();
    let mut comps: Vec<HunkCompressor> = (0..n_threads).map(|_| w.compressor()).collect::<chd::Result<_>>().map_err(chd_err)?;
    let per = ((2 << 20) / hunk).max(1); // hunks per thread in one round
    let mut buf = vec![0u8; n_threads * per * hunk];
    let mut p = Progress::new("Converting to CHD", total);
    let mut done = 0u64;
    while done < total {
        let n = ((total - done) as usize).min(buf.len());
        let padded = n.div_ceil(hunk) * hunk;
        fill(&mut buf[..n])?;
        buf[n..padded].fill(0);
        let hunks: Vec<&[u8]> = buf[..padded].chunks_exact(hunk).collect();
        let share = hunks.len().div_ceil(n_threads);
        let results: Vec<chd::Result<Vec<CompressedHunk>>> = std::thread::scope(|s| {
            let hs: Vec<_> = comps
                .iter_mut()
                .zip(hunks.chunks(share))
                .map(|(c, part)| s.spawn(move || part.iter().map(|h| c.compress(h)).collect::<chd::Result<Vec<_>>>()))
                .collect();
            hs.into_iter().map(|h| h.join().unwrap_or(Err(chd::Error::InvalidState))).collect()
        });
        for r in results {
            for c in r.map_err(chd_err)? {
                w.write_compressed_hunk(c).map_err(chd_err)?;
            }
        }
        done += n as u64;
        p.update(done);
    }
    w.finish().map_err(chd_err)?.flush()?;
    p.finish();
    Ok(())
}

fn chd_file(out: &Path, opts: ChdWriteOptions) -> Result<ChdWriter<BufWriter<File>>> {
    ChdWriter::new(BufWriter::with_capacity(4 << 20, File::create(out)?), opts).map_err(chd_err)
}

/// Codecs for DVD and hard disk CHDs when Zstandard is asked for.
const ZSTD_CODECS: [CodecType; 4] = [CodecType::ZstdV5, CodecType::ZLibV5, CodecType::HuffV5, CodecType::FlacV5];

/// A hard disk CHD, as `chdman createhd` makes for MAME.
fn write_chd_disk(src: &mut dyn Source, out: &Path, zstd: bool) -> Result<()> {
    let size = src.len().div_ceil(512) * 512;
    let sectors = size / 512;
    let (heads, secs) = if sectors % (16 * 63) == 0 { (16, 63) } else { (1, 1) };
    let mut opts = ChdWriteOptions::hard_disk().logical_bytes(size);
    if zstd {
        opts = opts.compression(&ZSTD_CODECS);
    }
    let hunk = opts.sizes().0 as usize;
    let mut w = chd_file(out, opts)?;
    let meta = CString::new(format!("CYLS:{},HEADS:{heads},SECS:{secs},BPS:512", sectors / (heads * secs)))?;
    w.add_metadata(KnownMetadata::HardDisk, meta.as_c_str(), true).map_err(chd_err)?;
    let mut pos = 0u64;
    chd_pump(w, hunk, size, |buf| {
        src.read_at(pos, buf)?;
        pos += buf.len() as u64;
        Ok(())
    })
}

/// Largest CD: 99 minutes. Anything bigger with a single data track is a DVD.
const CD_MAX_SECTORS: u64 = 99 * 60 * 75;
const CHD_FRAME: usize = 2448;

/// A CD-ROM, GD-ROM or DVD CHD, as `chdman createcd` / `createdvd` make.
fn write_chd_cd(l: &Layout, out: &Path, zstd: bool) -> Result<()> {
    if let [t] = l.tracks.as_slice() {
        if t.size == 2048 && t.frames > CD_MAX_SECTORS {
            let total = t.frames * 2048;
            let mut opts = ChdWriteOptions::dvd().logical_bytes(total);
            if zstd {
                opts = opts.compression(&ZSTD_CODECS);
            }
            let hunk = opts.sizes().0 as usize;
            let mut w = chd_file(out, opts)?;
            w.add_metadata(u32::from_be_bytes(*b"DVD "), &b"\0"[..], true).map_err(chd_err)?;
            let (mut frame, mut tmp) = (0u64, Vec::new());
            return chd_pump(w, hunk, total, |buf| {
                t.read(frame, buf.len() / 2048, &mut tmp)?;
                buf.copy_from_slice(&tmp);
                frame += (buf.len() / 2048) as u64;
                Ok(())
            });
        }
    }
    // Each track is its sectors, then (GD-ROM) its gap, padded to a multiple of 4 frames.
    let slots = |t: &RawTrack| (t.frames + t.pad).div_ceil(4) * 4;
    let total = l.tracks.iter().map(slots).sum::<u64>() * CHD_FRAME as u64;
    let mut opts = ChdWriteOptions::cd().logical_bytes(total);
    if zstd {
        opts = opts.compression(&[CodecType::ZstdCdV5, CodecType::ZLibCdV5, CodecType::FlacCdV5]);
    }
    let hunk = opts.sizes().0 as usize;
    let mut w = chd_file(out, opts)?;
    for t in &l.tracks {
        let ty = t.chd_type();
        let sub = if t.sub.is_some() { "RW_RAW" } else { "NONE" };
        let pregap = match (t.pregap, t.gap) {
            (0, 0) => "PREGAP:0 PGTYPE:MODE1 PGSUB:NONE".to_string(),
            (0, gap) => format!("PREGAP:{gap} PGTYPE:MODE1 PGSUB:NONE"),
            (stored, _) => format!("PREGAP:{stored} PGTYPE:V{ty} PGSUB:{sub}"),
        };
        let (tag, text) = if l.gd {
            (KnownMetadata::GdRomTrack, format!("TRACK:{} TYPE:{ty} SUBTYPE:{sub} FRAMES:{} PAD:{} {pregap} POSTGAP:0", t.number, t.frames + t.pad, t.pad))
        } else {
            (KnownMetadata::CdRomTrack2, format!("TRACK:{} TYPE:{ty} SUBTYPE:{sub} FRAMES:{} {pregap} POSTGAP:0", t.number, t.frames))
        };
        w.add_metadata(tag, CString::new(text)?.as_c_str(), true).map_err(chd_err)?;
    }
    let (mut ti, mut slot, mut tmp, mut subs) = (0usize, 0u64, Vec::new(), Vec::new());
    chd_pump(w, hunk, total, |buf| {
        buf.fill(0);
        let mut at = 0usize; // frames filled in `buf`
        let want = buf.len() / CHD_FRAME;
        while at < want {
            let t = &l.tracks[ti];
            if slot >= slots(t) {
                (ti, slot) = (ti + 1, 0);
                continue;
            }
            let n = if slot < t.frames {
                let n = ((t.frames - slot) as usize).min(want - at);
                t.read(slot, n, &mut tmp)?;
                let has_sub = t.read_sub(slot, n, true, &mut subs)?;
                for k in 0..n {
                    let frame = &mut buf[(at + k) * CHD_FRAME..(at + k + 1) * CHD_FRAME];
                    let f = &mut frame[..t.size as usize];
                    f.copy_from_slice(&tmp[k * t.size as usize..][..t.size as usize]);
                    if t.audio {
                        // CHD stores CD audio big-endian.
                        for pair in f.chunks_exact_mut(2) {
                            pair.swap(0, 1);
                        }
                    }
                    if has_sub {
                        frame[2352..].copy_from_slice(&subs[k * 96..(k + 1) * 96]);
                    }
                }
                n
            } else {
                ((slots(t) - slot) as usize).min(want - at) // gap and padding: zeros
            };
            at += n;
            slot += n as u64;
        }
        Ok(())
    })
}

// ---------------------------------------------------------------- BIN/CUE, GDI, CCD

fn msf(frames: u64) -> String {
    format!("{:02}:{:02}:{:02}", frames / 4500, frames / 75 % 60, frames % 75)
}

/// Write the tracks as .bin files with a cue sheet, or as a Dreamcast .gdi set.
fn write_tracks(l: &Layout, out: &Path, gdi: bool, job: &mut Job) -> Result<()> {
    let stem = out.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let total: u64 = l.tracks.iter().map(|t| t.frames * t.size as u64).sum();
    crate::convert::check_free_space(out.parent().unwrap_or(Path::new(".")), total)?;
    // One .bin for a single track, or when every track has 2352-byte sectors (positions
    // in the file are then plain frame counts); otherwise a file per track.
    let one_file = !gdi && (l.tracks.len() == 1 || l.tracks.iter().all(|t| t.size == 2352));
    let name_of = |t: &RawTrack| {
        if one_file {
            format!("{stem}.bin")
        } else {
            format!("{stem} (Track {:02}).{}", t.number, if gdi && t.audio { "raw" } else { "bin" })
        }
    };
    let mut sheet = if gdi { format!("{}\r\n", l.tracks.len()) } else { String::new() };
    let mut p = Progress::new(if gdi { "Converting to GDI" } else { "Converting to BIN/CUE" }, total);
    let (mut done, mut file_frames, mut buf) = (0u64, 0u64, Vec::new());
    let mut w: Option<BufWriter<File>> = None;
    for (i, t) in l.tracks.iter().enumerate() {
        let name = name_of(t);
        if w.is_none() || !one_file {
            if let Some(mut old) = w.take() {
                old.flush()?;
            }
            w = Some(BufWriter::with_capacity(4 << 20, File::create(job.add(&out.with_file_name(&name))?)?));
            file_frames = 0;
        }
        if gdi {
            sheet.push_str(&format!("{} {} {} {} \"{name}\" 0\r\n", t.number, t.lba, if t.audio { 0 } else { 4 }, t.size));
        } else {
            if i == 0 || !one_file {
                sheet.push_str(&format!("FILE \"{name}\" BINARY\r\n"));
            }
            sheet.push_str(&format!("  TRACK {:02} {}\r\n", t.number, t.cue_type()));
            if t.gap > 0 {
                sheet.push_str(&format!("    PREGAP {}\r\n", msf(t.gap)));
            }
            if t.pregap > 0 {
                sheet.push_str(&format!("    INDEX 00 {}\r\n", msf(file_frames)));
            }
            sheet.push_str(&format!("    INDEX 01 {}\r\n", msf(file_frames + t.pregap)));
        }
        let file = w.as_mut().unwrap();
        let mut frame = 0u64;
        while frame < t.frames {
            let n = (t.frames - frame).min(1024) as usize;
            t.read(frame, n, &mut buf)?;
            file.write_all(&buf)?;
            frame += n as u64;
            done += buf.len() as u64;
            p.update(done);
        }
        file_frames += t.frames;
    }
    if let Some(mut last) = w {
        last.flush()?;
    }
    std::fs::write(job.add(out)?, sheet)?;
    p.finish();
    Ok(())
}

fn bcd(n: u64) -> u8 {
    ((n / 10 % 10) << 4 | n % 10) as u8
}

/// Subchannel data for a sector that has none stored: the position information (Q) a
/// drive would read, and the pause flag (P). Channel by channel, as CloneCD's .sub wants.
fn synth_sub(t: &RawTrack, frame: u64, lba: u64) -> [u8; 96] {
    let mut s = [0u8; 96];
    let in_pregap = frame < t.pregap;
    if in_pregap {
        s[..12].fill(0xFF);
    }
    let rel = if in_pregap { t.pregap - frame } else { frame - t.pregap };
    let abs = lba + 150;
    let q = &mut s[12..24];
    q[0] = if t.audio { 0x01 } else { 0x41 };
    q[1] = bcd(t.number as u64);
    q[2] = if in_pregap { 0 } else { 1 };
    q[3..6].copy_from_slice(&[bcd(rel / 4500), bcd(rel / 75 % 60), bcd(rel % 75)]);
    q[7..10].copy_from_slice(&[bcd(abs / 4500), bcd(abs / 75 % 60), bcd(abs % 75)]);
    let mut crc = 0u16;
    for &b in &q[..10] {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    q[10..12].copy_from_slice(&(!crc).to_be_bytes());
    s
}

/// CloneCD: .ccd (the table of contents), .img (raw sectors) and .sub (subchannel data).
fn write_ccd(l: &Layout, out: &Path, job: &mut Job) -> Result<()> {
    if l.tracks.iter().any(|t| t.size != 2352) {
        bail!("CloneCD images hold raw 2352-byte sectors, which this image doesn't have; convert it to chd or cue instead");
    }
    let frames: u64 = l.tracks.iter().map(|t| t.frames).sum();
    crate::convert::check_free_space(out.parent().unwrap_or(Path::new(".")), frames * 2448)?;
    let mut img = BufWriter::with_capacity(4 << 20, File::create(job.add(&out.with_extension("img"))?)?);
    let mut sub = BufWriter::with_capacity(1 << 20, File::create(job.add(&out.with_extension("sub"))?)?);
    let mut p = Progress::new("Converting to CCD", frames * 2352);
    // Tracks sit back to back in the .img; a sector's address is its position there.
    let mut starts = Vec::new();
    let (mut lba, mut buf, mut subs) = (0u64, Vec::new(), Vec::new());
    for t in &l.tracks {
        starts.push(lba);
        let mut frame = 0u64;
        while frame < t.frames {
            let n = (t.frames - frame).min(1024) as usize;
            t.read(frame, n, &mut buf)?;
            img.write_all(&buf)?;
            if !t.read_sub(frame, n, false, &mut subs)? {
                subs.clear();
                for k in 0..n as u64 {
                    subs.extend_from_slice(&synth_sub(t, frame + k, lba + k));
                }
            }
            sub.write_all(&subs)?;
            frame += n as u64;
            lba += n as u64;
            p.update(lba * 2352);
        }
    }
    img.flush()?;
    sub.flush()?;

    let entry = |i: usize, point: u32, control: u8, at: (u64, u64, u64), plba: i64| {
        format!(
            "[Entry {i}]\r\nSession=1\r\nPoint=0x{point:02x}\r\nADR=0x01\r\nControl=0x{control:02x}\r\nTrackNo=0\r\nAMin=0\r\nASec=0\r\nAFrame=0\r\nALBA=-150\r\nZero=0\r\nPMin={}\r\nPSec={}\r\nPFrame={}\r\nPLBA={plba}\r\n",
            at.0, at.1, at.2
        )
    };
    let split = |lba: u64| ((lba + 150) / 4500, (lba + 150) / 75 % 60, (lba + 150) % 75);
    let control = |t: &RawTrack| if t.audio { 0u8 } else { 4 };
    let (first, last) = (&l.tracks[0], l.tracks.last().unwrap());
    let mode2 = l.tracks.iter().any(|t| t.mode == 2);
    let mut s = format!(
        "[CloneCD]\r\nVersion=3\r\n[Disc]\r\nTocEntries={}\r\nSessions=1\r\nDataTracksScrambled=0\r\nCDTextLength=0\r\n[Session 1]\r\nPreGapMode={}\r\nPreGapSubC=0\r\n",
        l.tracks.len() + 3,
        first.mode
    );
    s += &entry(0, 0xa0, control(first), (first.number as u64, if mode2 { 0x20 } else { 0 }, 0), first.number as i64 * 4500 - 150);
    s += &entry(1, 0xa1, control(last), (last.number as u64, 0, 0), last.number as i64 * 4500 - 150);
    s += &entry(2, 0xa2, control(last), split(frames), frames as i64);
    for (i, t) in l.tracks.iter().enumerate() {
        let index1 = starts[i] + t.pregap;
        s += &entry(i + 3, t.number, control(t), split(index1), index1 as i64);
    }
    for (i, t) in l.tracks.iter().enumerate() {
        s += &format!("[TRACK {}]\r\nMODE={}\r\n", t.number, t.mode);
        if t.pregap > 0 {
            s += &format!("INDEX 0={}\r\n", starts[i]);
        }
        s += &format!("INDEX 1={}\r\n", starts[i] + t.pregap);
    }
    std::fs::write(job.add(out)?, s)?;
    p.finish();
    Ok(())
}

// ---------------------------------------------------------------- virtual machine disks

/// Call `f(index, block)` for every `block`-byte piece of the disk that isn't all zeros.
fn data_blocks(src: &mut dyn Source, size: u64, block: usize, label: &str, mut f: impl FnMut(u64, &[u8]) -> Result<()>) -> Result<()> {
    let mut buf = vec![0u8; block];
    let mut p = Progress::new(label, size);
    for b in 0..size.div_ceil(block as u64) {
        let pos = b * block as u64;
        let n = (size - pos).min(block as u64);
        p.update(pos + n);
        if src.is_zero(pos, n)? {
            continue;
        }
        src.read_at(pos, &mut buf[..n as usize])?;
        buf[n as usize..].fill(0);
        if buf.iter().any(|&x| x != 0) {
            f(b, &buf)?;
        }
    }
    p.finish();
    Ok(())
}

/// VMware "monolithicSparse" VMDK (one growable file with an embedded descriptor).
fn write_vmdk(src: &mut dyn Source, out: &Path, final_name: &Path) -> Result<()> {
    const GRAIN: u64 = 128; // sectors (64 KiB)
    const PER_TABLE: u64 = 512;
    let size = src.len().div_ceil(512) * 512;
    if size > 2040 << 30 {
        bail!("the disk is larger than 2040 GiB, the limit of this kind of VMDK; convert it to vhdx, vdi or qcow2");
    }
    let sectors = size / 512;
    let grains = sectors.div_ceil(GRAIN);
    let tables = grains.div_ceil(PER_TABLE).max(1);
    let gd_secs = (tables * 4).div_ceil(512);
    let gt_secs = tables * 4; // each table: 512 entries = 4 sectors
    let (desc_at, desc_secs) = (1u64, 20u64);
    let rgd = desc_at + desc_secs;
    let gd = rgd + gd_secs + gt_secs;
    let overhead = (gd + gd_secs + gt_secs).div_ceil(GRAIN) * GRAIN;

    let mut h = [0u8; 512];
    h[0..4].copy_from_slice(b"KDMV");
    h[4..8].copy_from_slice(&1u32.to_le_bytes());
    h[8..12].copy_from_slice(&3u32.to_le_bytes()); // newline check + redundant grain tables
    h[12..20].copy_from_slice(&sectors.to_le_bytes());
    h[20..28].copy_from_slice(&GRAIN.to_le_bytes());
    h[28..36].copy_from_slice(&desc_at.to_le_bytes());
    h[36..44].copy_from_slice(&desc_secs.to_le_bytes());
    h[44..48].copy_from_slice(&(PER_TABLE as u32).to_le_bytes());
    h[48..56].copy_from_slice(&rgd.to_le_bytes());
    h[56..64].copy_from_slice(&gd.to_le_bytes());
    h[64..72].copy_from_slice(&overhead.to_le_bytes());
    h[73..77].copy_from_slice(b"\n \r\n");
    let name = final_name.file_name().unwrap_or_default().to_string_lossy();
    let desc = format!(
        "# Disk DescriptorFile\nversion=1\nCID={:08x}\nparentCID=ffffffff\ncreateType=\"monolithicSparse\"\n\n\
         # Extent description\nRW {sectors} SPARSE \"{name}\"\n\n# The Disk Data Base\n#DDB\n\n\
         ddb.virtualHWVersion = \"4\"\nddb.geometry.cylinders = \"{}\"\nddb.geometry.heads = \"16\"\n\
         ddb.geometry.sectors = \"63\"\nddb.adapterType = \"ide\"\n",
        u32::from_le_bytes(random_bytes::<4>()) & 0xFFFF_FFFE,
        (sectors / (16 * 63)).clamp(1, 16383),
    );
    if desc.len() > (desc_secs * 512) as usize {
        bail!("the file name is too long for a VMDK descriptor");
    }
    let mut file = File::create(out)?;
    file.write_all(&h)?;
    file.write_all(desc.as_bytes())?;
    file.set_len(overhead * 512)?;
    file.seek(SeekFrom::Start(overhead * 512))?;
    let mut w = BufWriter::with_capacity(4 << 20, file);
    let mut gte = vec![0u32; (tables * PER_TABLE) as usize];
    let mut next = overhead;
    data_blocks(src, size, (GRAIN * 512) as usize, "Converting to VMDK", |i, data| {
        gte[i as usize] = u32::try_from(next).map_err(|_| anyhow!("the disk is too large for this kind of VMDK"))?;
        w.write_all(data)?;
        next += GRAIN;
        Ok(())
    })?;
    let file = w.into_inner().map_err(|e| e.into_error())?;
    let tables_raw: Vec<u8> = gte.iter().flat_map(|v| v.to_le_bytes()).collect();
    for dir_at in [rgd, gd] {
        let first_table = dir_at + gd_secs;
        let dir: Vec<u8> = (0..tables).flat_map(|t| ((first_table + t * 4) as u32).to_le_bytes()).collect();
        file.seek_write(&dir, dir_at * 512)?;
        file.seek_write(&tables_raw, first_table * 512)?;
    }
    file.sync_all()?;
    Ok(())
}

/// VirtualBox VDI (version 1.1, dynamically allocated).
fn write_vdi(src: &mut dyn Source, out: &Path) -> Result<()> {
    const MB: u64 = 1 << 20;
    let size = src.len().div_ceil(512) * 512;
    let blocks = size.div_ceil(MB);
    if size > crate::convert::VHDX_MAX {
        bail!("the disk is too large for VDI");
    }
    let map_at = MB;
    let data_at = map_at + (blocks * 4).div_ceil(MB).max(1) * MB;
    let mut map = vec![0xFFFF_FFFFu32; blocks as usize];
    let mut file = File::create(out)?;
    file.set_len(data_at)?;
    file.seek(SeekFrom::Start(data_at))?;
    let mut w = BufWriter::with_capacity(4 << 20, file);
    let mut used = 0u32;
    data_blocks(src, size, MB as usize, "Converting to VDI", |i, data| {
        map[i as usize] = used;
        used += 1;
        w.write_all(data)?;
        Ok(())
    })?;
    let file = w.into_inner().map_err(|e| e.into_error())?;

    let mut h = vec![0u8; 512];
    h[..40].copy_from_slice(b"<<< Oracle VM VirtualBox Disk Image >>>\n");
    let mut put = |at: usize, v: u32| h[at..at + 4].copy_from_slice(&v.to_le_bytes());
    put(64, 0xBEDA_107F); // signature
    put(68, 0x0001_0001); // version 1.1
    put(72, 400); // header size
    put(76, 1); // dynamically allocated
    put(340, map_at as u32);
    put(344, data_at as u32);
    put(360, 512); // sector size (legacy geometry)
    put(376, MB as u32); // block size
    put(384, blocks as u32);
    put(388, used);
    put(468, 512); // sector size (logical geometry)
    h[368..376].copy_from_slice(&size.to_le_bytes());
    h[392..408].copy_from_slice(&random_bytes::<16>()); // image UUID
    h[408..424].copy_from_slice(&random_bytes::<16>()); // last-modified UUID
    file.seek_write(&h, 0)?;
    let raw: Vec<u8> = map.iter().flat_map(|v| v.to_le_bytes()).collect();
    file.seek_write(&raw, map_at)?;
    file.sync_all()?;
    Ok(())
}

/// Count one more reference to every 64 KiB cluster in `from..to` of a QCOW2 file.
fn qcow2_touch(refs: &mut Vec<u16>, from: u64, to: u64) {
    let (a, b) = ((from >> 16) as usize, ((to - 1) >> 16) as usize);
    if refs.len() <= b {
        refs.resize(b + 1, 0);
    }
    for r in &mut refs[a..=b] {
        *r += 1;
    }
}

/// QEMU QCOW2 (version 2, 64 KiB clusters), optionally with deflated clusters.
fn write_qcow2(src: &mut dyn Source, out: &Path, compress: bool) -> Result<()> {
    const CLUSTER: u64 = 64 << 10;
    const L2_ENTRIES: u64 = CLUSTER / 8;
    const COPIED: u64 = 1 << 63; // "this cluster has exactly one reference"
    const COMPRESSED: u64 = 1 << 62;
    let size = src.len().div_ceil(512) * 512;
    if size > crate::convert::VHDX_MAX {
        bail!("the disk is too large to convert");
    }
    let l1_size = size.div_ceil(CLUSTER * L2_ENTRIES).max(1);
    let mut l1 = vec![0u64; l1_size as usize];
    // L2 tables are kept in memory and written after the data they point to.
    let mut l2: std::collections::BTreeMap<u64, Vec<u64>> = Default::default();
    // How many things point into each cluster of the file (the header is cluster 0).
    let mut refs: Vec<u16> = vec![1];
    let mut file = File::create(out)?;
    file.set_len(CLUSTER)?;
    file.seek(SeekFrom::Start(CLUSTER))?;
    let mut w = BufWriter::with_capacity(4 << 20, file);
    let mut pos = CLUSTER; // file offset written so far
    let mut packed = vec![0u8; CLUSTER as usize];
    fn align(w: &mut BufWriter<File>, pos: &mut u64) -> Result<()> {
        let padded = pos.div_ceil(64 << 10) * (64 << 10);
        w.write_all(&vec![0u8; (padded - *pos) as usize])?;
        *pos = padded;
        Ok(())
    }
    data_blocks(src, size, CLUSTER as usize, "Converting to QCOW2", |i, data| {
        let mut entry = None;
        if compress {
            // QEMU inflates with a 4 KiB window, so deflate with one.
            let mut z = flate2::Compress::new_with_window_bits(flate2::Compression::default(), false, 12);
            let ok = matches!(z.compress(data, &mut packed, flate2::FlushCompress::Finish), Ok(flate2::Status::StreamEnd));
            let len = z.total_out();
            if ok && len + 512 < CLUSTER {
                // Compressed clusters are packed back to back; the entry holds the offset
                // and how many more 512-byte sectors the data reaches into.
                let sectors = ((pos % 512) + len).div_ceil(512);
                entry = Some(COMPRESSED | pos | (sectors - 1) << 54);
                w.write_all(&packed[..len as usize])?;
                qcow2_touch(&mut refs, pos, pos + len);
                pos += len;
            }
        }
        let entry = match entry {
            Some(e) => e,
            None => {
                align(&mut w, &mut pos)?;
                w.write_all(data)?;
                qcow2_touch(&mut refs, pos, pos + CLUSTER);
                pos += CLUSTER;
                (pos - CLUSTER) | COPIED
            }
        };
        l2.entry(i / L2_ENTRIES).or_insert_with(|| vec![0u64; L2_ENTRIES as usize])[(i % L2_ENTRIES) as usize] = entry;
        Ok(())
    })?;
    align(&mut w, &mut pos)?;
    for (i, table) in &l2 {
        l1[*i as usize] = pos | COPIED;
        w.write_all(&table.iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<u8>>())?;
        qcow2_touch(&mut refs, pos, pos + CLUSTER);
        pos += CLUSTER;
    }
    let l1_at = pos;
    w.write_all(&l1.iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<u8>>())?;
    pos += l1_size * 8;
    align(&mut w, &mut pos)?;
    qcow2_touch(&mut refs, l1_at, pos);
    // Reference counts (16-bit, one per cluster in the file), including their own clusters.
    let used = pos / CLUSTER;
    let (mut rc_blocks, mut rc_table) = (1u64, 1u64);
    loop {
        let need_blocks = (used + rc_blocks + rc_table).div_ceil(CLUSTER / 2);
        let need_table = (need_blocks * 8).div_ceil(CLUSTER);
        if (need_blocks, need_table) == (rc_blocks, rc_table) {
            break;
        }
        (rc_blocks, rc_table) = (need_blocks, need_table);
    }
    let rc_at = pos;
    qcow2_touch(&mut refs, rc_at, rc_at + (rc_blocks + rc_table) * CLUSTER);
    refs.resize((rc_blocks * (CLUSTER / 2)) as usize, 0);
    w.write_all(&refs.iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<u8>>())?;
    let table_at = rc_at + rc_blocks * CLUSTER;
    w.write_all(&(0..rc_blocks).flat_map(|b| (rc_at + b * CLUSTER).to_be_bytes()).collect::<Vec<u8>>())?;
    let mut end = table_at + rc_blocks * 8;
    align(&mut w, &mut end)?;
    let file = w.into_inner().map_err(|e| e.into_error())?;

    let mut h = [0u8; 72];
    h[0..4].copy_from_slice(b"QFI\xfb");
    h[4..8].copy_from_slice(&2u32.to_be_bytes());
    h[20..24].copy_from_slice(&16u32.to_be_bytes()); // cluster bits
    h[24..32].copy_from_slice(&size.to_be_bytes());
    h[36..40].copy_from_slice(&(l1_size as u32).to_be_bytes());
    h[40..48].copy_from_slice(&l1_at.to_be_bytes());
    h[48..56].copy_from_slice(&table_at.to_be_bytes());
    h[56..60].copy_from_slice(&(rc_table as u32).to_be_bytes());
    file.seek_write(&h, 0)?;
    file.sync_all()?;
    Ok(())
}
