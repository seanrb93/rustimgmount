//! Format detection: decide how a given image file can be presented to Windows.

pub mod audio;
pub mod cd;
pub mod chd;
pub mod disk;
pub mod expand;
pub mod isz;
pub mod layout;
pub mod mdx;
pub mod packed;
pub mod pbp;
pub mod pieces;
pub mod qcow2;
pub mod vdi;
pub mod vhd;
pub mod vmdk;

use crate::source::{le16, le32, open_ro, read_vec, FileSlice, Source};
use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};

/// What Windows' built-in VirtDisk driver can attach directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NativeKind {
    Iso,
    Vhd,
    Vhdx,
}

impl NativeKind {
    pub fn from_ext(path: &Path) -> Option<Self> {
        match ext(path).as_str() {
            "iso" => Some(Self::Iso),
            "vhd" | "avhd" => Some(Self::Vhd),
            "vhdx" | "avhdx" => Some(Self::Vhdx),
            _ => None,
        }
    }
    pub fn ext(self) -> &'static str {
        match self {
            Self::Iso => "iso",
            Self::Vhd => "vhd",
            Self::Vhdx => "vhdx",
        }
    }
}

pub enum Plan {
    /// Attach this file as-is (it may need an alias with the right extension).
    Native(NativeKind),
    /// An optical disc that must be re-packed into a 2048-byte/sector ISO
    /// (or, if `plain_file` is set, merely aliased as .iso).
    Cd(cd::Disc),
    /// A hard disk / floppy that must be re-packed into a VHD.
    Disk(Box<dyn Source>),
}

pub struct Detected {
    /// The file that actually holds the data (e.g. a decompressed copy).
    pub data_path: PathBuf,
    pub format: String,
    pub plan: Plan,
}

pub fn ext(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

const CD_EXTS: &[&str] = &[
    "iso", "bin", "mdf", "nrg", "cdi", "img", "ccd", "cue", "mds", "b5t", "b6t", "bwt", "pdi", "gi", "c2d", "toast", "udf", "raw",
];
const RAW_DISK_EXTS: &[&str] = &["img", "ima", "raw", "dd", "bin", "flp", "vfd", "dsk", "image", "hdd", "hda", "001"];

fn sibling_with_ext(path: &Path, exts: &[&str]) -> Option<PathBuf> {
    for e in exts {
        for cand in [path.with_extension(e), path.with_extension(e.to_ascii_uppercase())] {
            if cand.exists() {
                return Some(cand);
            }
        }
    }
    None
}


/// `game.pbp#2` selects disc 2 of a multi-disc image. Returns the real path and the
/// zero-based disc index.
pub fn split_disc(path: &Path) -> (PathBuf, Option<usize>) {
    let s = path.to_string_lossy();
    if !path.exists() {
        if let Some((base, n)) = s.rsplit_once('#') {
            if let Ok(n) = n.parse::<usize>() {
                if n >= 1 && Path::new(base).exists() {
                    return (PathBuf::from(base), Some(n - 1));
                }
            }
        }
    }
    (path.to_owned(), None)
}

/// Images that hold several discs (M3U playlists, multi-disc EBOOTs): the entries to
/// mount, as paths (possibly with a `#N` disc selector). `None` for single images.
pub fn multi(path: &Path) -> Result<Option<Vec<PathBuf>>> {
    if split_disc(path).1.is_some() {
        return Ok(None);
    }
    if ext(path) == "m3u" || ext(path) == "m3u8" {
        let text = std::fs::read(path)?;
        let text = String::from_utf8_lossy(&text);
        let mut out = Vec::new();
        for line in text.lines() {
            let l = line.trim().trim_start_matches('\u{feff}');
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let p = cd::resolve_sibling(path, l).ok_or_else(|| anyhow!("playlist entry not found: {l}"))?;
            out.push(p);
        }
        if out.is_empty() {
            bail!("the playlist {} lists no discs", path.display());
        }
        return Ok(Some(out));
    }
    let f = open_ro(path)?;
    if read_vec(&f, 0, 4)? == pbp::MAGIC {
        let n = pbp::discs(path)?.len();
        if n > 1 {
            return Ok(Some((1..=n).map(|i| PathBuf::from(format!("{}#{i}", path.display()))).collect()));
        }
    }
    Ok(None)
}

pub fn detect(path: &Path) -> Result<Detected> {
    let (path, disc) = split_disc(path);
    let path = path.as_path();
    if let Some(d) = disc {
        let n = pbp::discs(path)?.len();
        let src = pbp::PbpDisc::open(path, d)?;
        return detect_source(path, &format!("PSP EBOOT, disc {} of {n}", d + 1), Box::new(src));
    }

    // Whole-file compression: decompress into the cache, then look inside.
    if let Some(k) = expand::kind(path)? {
        let inner = expand::expand(path)?;
        let mut d = detect(&inner)?;
        d.format = format!("{} ({}-compressed)", d.format, k.name());
        return Ok(d);
    }

    let e = ext(path);
    // Sheets/descriptors that point at the real data file(s).
    match e.as_str() {
        "cue" => return cd_plan(path, "CUE/BIN", cd::from_cue(path)?),
        "gdi" => return cd_plan(path, "Dreamcast GDI", cd::from_gdi(path)?),
        "m3u" | "m3u8" => bail!("{} is a playlist; `rustimgmount mount` mounts every disc in it", path.display()),
        "ccd" | "sub" => {
            let img = sibling_with_ext(path, &["img"]).ok_or_else(|| anyhow!("CloneCD .img not found next to {}", path.display()))?;
            return cd_file(&img, "CloneCD");
        }
        "mdx" => return cd_plan(path, "Alcohol MDX", mdx::open(path)?),
        "mds" if mdx::is_v2(&read_vec(&open_ro(path)?, 0, 32)?) => return cd_plan(path, "Alcohol MDS v2", mdx::open(path)?),
        "mds" => {
            let mdf = sibling_with_ext(path, &["mdf"]).ok_or_else(|| anyhow!("Alcohol .mdf not found next to {}", path.display()))?;
            return cd_plan(&mdf, "Alcohol MDS/MDF", cd::from_mds(path, &mdf)?);
        }
        "mdf" => {
            if let Some(mds) = sibling_with_ext(path, &["mds"]) {
                return cd_plan(path, "Alcohol MDS/MDF", cd::from_mds(&mds, path)?);
            }
            let parts = pieces::mdf_parts(path);
            if parts.len() > 1 {
                return detect_source(path, &format!("Alcohol MDF ({} parts)", parts.len()), pieces::concat_files(&parts)?);
            }
        }
        _ => {}
    }

    let file = open_ro(path)?;
    let len = file.metadata()?.len();
    let head = read_vec(&file, 0, 4096)?;
    let tail = if len >= 512 { read_vec(&file, len - 512, 512)? } else { vec![0; 512] };
    drop(file);

    if mdx::is_v2(&head) {
        return cd_plan(path, "Alcohol MDX", mdx::open(path)?);
    }

    // Native formats Windows attaches itself.
    if &head[0..8] == b"vhdxfile" {
        return native(path, "VHDX", NativeKind::Vhdx);
    }
    if &tail[0..8] == b"conectix" || &head[0..8] == b"conectix" {
        return native(path, "VHD", NativeKind::Vhd);
    }

    // Images split across numbered files, and forensic containers.
    if let Some(parts) = pieces::split_raw_parts(path) {
        return detect_source(path, &format!("split raw image ({} parts)", parts.len()), pieces::concat_files(&parts)?);
    }
    if pieces::is_ewf(&head) {
        return detect_source(path, "EnCase EWF", Box::new(pieces::Ewf::open(path)?));
    }

    // Emulation formats.
    if &head[0..8] == chd::MAGIC {
        return match chd::open(path)? {
            chd::Opened::Cd(disc) => cd_plan(path, "CHD (CD/DVD)", disc),
            chd::Opened::Disk(src) => disk(path, "CHD (hard disk)", src),
        };
    }
    if let Some(name) = packed::magic(&head) {
        let p = packed::Packed::open(path)?;
        let n = p.len();
        let c = cd::TrackCand::new(cd::shared(p), 0, 2048, n / 2048).hint(Some(0));
        return cd_plan(path, name, cd::Disc::assemble(vec![c], None)?);
    }
    if &head[0..4] == isz::MAGIC {
        return detect_source(path, "UltraISO ISZ", Box::new(isz::Isz::open(path)?));
    }
    if &head[0..4] == pbp::MAGIC {
        let n = pbp::discs(path)?.len();
        let fmt = if n > 1 { format!("PSP EBOOT, disc 1 of {n}") } else { "PSP EBOOT".to_string() };
        return detect_source(path, &fmt, Box::new(pbp::PbpDisc::open(path, 0)?));
    }
    if let Some(k) = disk::pc98_kind(path, &head, len) {
        let name = match k {
            disk::Pc98::Hdi => "PC-98 HDI",
            disk::Pc98::Fdi => "PC-98 FDI",
            disk::Pc98::Nhd => "PC-98 NHD",
        };
        return disk(path, name, disk::pc98(path, k, len)?);
    }
    if e == "msa" || (head[0..2] == [0x0E, 0x0F] && e != "iso") {
        return disk(path, "Atari ST MSA", disk::raw_disk(Box::new(disk::Mem(disk::unmsa(path)?)), true)?);
    }
    if disk::is_d88_ext(&e) {
        let raw = disk::d88(path)?;
        let mut boot = vec![0u8; 512];
        let n = raw.len().min(512);
        boot[..n].copy_from_slice(&raw[..n]);
        if disk::is_vbr(&boot, true).is_none() {
            bail!("this D88 floppy has no DOS (FAT) file system (it's probably a game boot disk), so Windows has nothing to show");
        }
        return disk(path, "D88 floppy", disk::raw_disk(Box::new(disk::Mem(raw)), true)?);
    }
    if e == "st" {
        return disk(path, "Atari ST", disk::raw_disk(Box::new(FileSlice::whole(path)?), true)?);
    }

    // Virtual machine disks we translate.
    if &head[0..4] == vmdk::MAGIC || head.starts_with(b"# Disk DescriptorFile") || e == "vmdk" {
        return disk(path, "VMDK", vmdk::open(path)?);
    }
    if &head[0..4] == qcow2::MAGIC {
        return disk(path, "QCOW2", Box::new(qcow2::Qcow2::open(path)?));
    }
    if vdi::is_vdi(&head) {
        return disk(path, "VDI", Box::new(vdi::Vdi::open(path)?));
    }
    if head.starts_with(b"WithoutFreeSpace") || head.starts_with(b"WithouFreSpacExt") {
        return disk(path, "Parallels HDD", parallels(path)?);
    }

    // Things we recognise but cannot mount.
    if &tail[0..4] == b"koly" || e == "dmg" {
        bail!("Apple DMG images use HFS+/APFS, which Windows cannot read");
    }
    if head.starts_with(b"MSWIM\0\0\0") {
        bail!("WIM images are file archives, not disks; use `dism /Mount-Image` instead");
    }
    if head.starts_with(b"QFI") {
        bail!("qcow (version 1) images are not supported");
    }
    if &head[0..4] == b"WBFS" || matches!(e.as_str(), "rvz" | "wia" | "gcm" | "gcz" | "wbfs" | "nsp" | "xci") {
        bail!("Nintendo disc/cartridge images use their own file systems, which Windows cannot read");
    }

    // Optical images.
    if let Some(d) = cd::from_nrg(path)? {
        return cd_plan(path, "Nero NRG", d);
    }
    if cd::probe_base0(path)? {
        return cd_file(path, if e == "cdi" { "DiscJuggler CDI" } else { "ISO 9660/UDF" });
    }

    // Hard disk / floppy images.
    let has_disk_sig = le16(&head, 510) == 0xAA55 || &head[512..520] == b"EFI PART";
    if has_disk_sig {
        return disk(path, "raw disk", disk::raw_disk(Box::new(FileSlice::whole(path)?), false)?);
    }

    // Anything else that might hide a CD track at an odd offset (CDI, B5T, ...).
    if CD_EXTS.contains(&e.as_str()) {
        if let Some(d) = cd::from_file(path)? {
            return cd_plan(path, if e == "cdi" { "DiscJuggler CDI" } else { "CD/DVD image" }, d);
        }
    }
    if &head[0..8] == b"XBOX_DVD" || e == "xiso" {
        bail!("Xbox (XDVDFS) images use a file system Windows cannot read");
    }
    if RAW_DISK_EXTS.contains(&e.as_str()) && len >= 512 {
        eprintln!("warning: no partition table or file system signature found; treating as a raw disk");
        return disk(path, "raw disk", disk::raw_disk(Box::new(FileSlice::whole(path)?), false)?);
    }
    bail!("unrecognised image format: {}", path.display())
}

/// Decide whether a reassembled/decoded byte stream is a disc or a disk.
fn detect_source(path: &Path, fmt: &str, src: Box<dyn Source>) -> Result<Detected> {
    let len = src.len();
    let sh = cd::shared(src);
    for ss in [2048u32, 2352, 2448, 2336] {
        let c = cd::TrackCand::new(sh.clone(), 0, ss, len / ss as u64);
        if let Ok(d) = cd::Disc::assemble(vec![c], None) {
            return cd_plan(path, fmt, d);
        }
    }
    disk(path, fmt, disk::raw_disk(Box::new(cd::SharedSrc(sh)), false)?)
}

fn native(path: &Path, fmt: &str, kind: NativeKind) -> Result<Detected> {
    Ok(Detected { data_path: path.to_owned(), format: fmt.into(), plan: Plan::Native(kind) })
}

fn disk(path: &Path, fmt: &str, src: Box<dyn Source>) -> Result<Detected> {
    // Every converted disk gets IDs Windows can bring online (see disk::fix_ids),
    // unless an exact copy was asked for (`convert` to a non-Windows format).
    let src = if disk::exact() { src } else { disk::fix_ids(src)? };
    Ok(Detected { data_path: path.to_owned(), format: fmt.into(), plan: Plan::Disk(src) })
}

fn cd_plan(path: &Path, fmt: &str, disc: cd::Disc) -> Result<Detected> {
    let plan = match &disc.plain_file {
        Some(p) if NativeKind::from_ext(p) == Some(NativeKind::Iso) => Plan::Native(NativeKind::Iso),
        _ => Plan::Cd(disc),
    };
    Ok(Detected { data_path: path.to_owned(), format: fmt.into(), plan })
}

fn cd_file(path: &Path, fmt: &str) -> Result<Detected> {
    let d = cd::from_file(path)?.ok_or_else(|| anyhow!("no data track found in {}", path.display()))?;
    cd_plan(path, fmt, d)
}

/// Parallels "expanding" HDD images (header + BAT of sector offsets).
fn parallels(path: &Path) -> Result<Box<dyn Source>> {
    struct Pd {
        file: std::fs::File,
        size: u64,
        cluster: u64,
        bat: Vec<u32>,
        ext: bool,
    }
    impl Source for Pd {
        fn len(&self) -> u64 {
            self.size
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
            let mut done = 0;
            while done < buf.len() {
                let off = offset + done as u64;
                let ci = (off / self.cluster) as usize;
                let within = off % self.cluster;
                let n = ((self.cluster - within) as usize).min(buf.len() - done);
                let out = &mut buf[done..done + n];
                match self.bat.get(ci) {
                    Some(&e) if e != 0 => {
                        let base = if self.ext { e as u64 * self.cluster } else { e as u64 * 512 };
                        crate::source::pread(&self.file, base + within, out)?;
                    }
                    _ => out.fill(0),
                }
                done += n;
            }
            Ok(())
        }
    }
    let file = open_ro(path)?;
    let h = read_vec(&file, 0, 64)?;
    let ext = &h[0..16] == b"WithouFreSpacExt";
    let tracks = le32(&h, 28) as u64;
    let bat_entries = le32(&h, 32) as usize;
    let size = crate::source::le64(&h, 36) * 512;
    let cluster = tracks * 512;
    if cluster == 0 {
        bail!("corrupt Parallels header");
    }
    let raw = read_vec(&file, 64, bat_entries * 4)?;
    let bat = raw.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    Ok(Box::new(Pd { file, size, cluster, bat, ext }))
}
