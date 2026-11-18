//! Raw hard-disk / floppy handling shared by many formats: recognising volume boot
//! records, wrapping partitionless volumes in an MBR, making FAT volumes from other
//! platforms (Atari ST, PC-98) acceptable to Windows' FAT driver, and PC-98 HDI/FDI/NHD.

use crate::source::{le16, le32, open_ro, read_vec, Concat, FileSlice, Segment, Source};
use anyhow::{bail, Result};
use std::io;
use std::path::Path;

/// Windows refuses very small VHDs.
const MIN_DISK: u64 = 3 << 20;

/// A window onto part of another source.
pub struct Window {
    inner: Box<dyn Source>,
    start: u64,
    len: u64,
}
impl Window {
    pub fn new(inner: Box<dyn Source>, start: u64, len: u64) -> Self {
        Self { inner, start, len }
    }
}
impl Source for Window {
    fn len(&self) -> u64 {
        self.len
    }
    fn is_zero(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let n = self.len.saturating_sub(offset).min(len);
        if n == 0 {
            return Ok(true);
        }
        self.inner.is_zero(self.start + offset, n)
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let avail = self.len.saturating_sub(offset).min(buf.len() as u64) as usize;
        let (h, t) = buf.split_at_mut(avail);
        self.inner.read_at(self.start + offset, h)?;
        t.fill(0);
        Ok(())
    }
}

/// Facts about a FAT/NTFS/exFAT boot sector.
pub struct Vbr {
    pub ptype: u8,
    /// Bytes per logical sector according to the BPB (FAT only).
    pub bps: u16,
    /// Volume size in bytes according to the BPB, if known.
    pub size: Option<u64>,
}

/// Recognise a volume boot record. `lenient` also accepts FAT boot sectors without an
/// x86 jump instruction (Atari ST and some other platforms).
pub fn is_vbr(b: &[u8], lenient: bool) -> Option<Vbr> {
    if b.len() < 512 {
        return None;
    }
    let oem = &b[3..11];
    if oem == b"NTFS    " {
        return Some(Vbr { ptype: 0x07, bps: le16(b, 11), size: Some(crate::source::le64(b, 40) * le16(b, 11) as u64) });
    }
    if oem == b"EXFAT   " {
        return Some(Vbr { ptype: 0x07, bps: 512, size: Some(crate::source::le64(b, 72) << b[108]) });
    }
    let jump = b[0] == 0xEB || b[0] == 0xE9;
    let bps = le16(b, 11);
    let spc = b[13];
    let reserved = le16(b, 14);
    let fats = b[16];
    if !matches!(bps, 256 | 512 | 1024 | 2048 | 4096) || spc == 0 || !spc.is_power_of_two() || fats == 0 || fats > 2 || reserved == 0 {
        return None;
    }
    if !jump {
        // Without a jump instruction, insist on a fully plausible small FAT12/16 BPB.
        let total16 = le16(b, 19);
        if !lenient || total16 == 0 || le16(b, 17) == 0 || le16(b, 22) == 0 {
            return None;
        }
    }
    let total16 = le16(b, 19) as u64;
    let total32 = le32(b, 32) as u64;
    let fat16_size = le16(b, 22);
    let total = if total16 != 0 { total16 } else { total32 } * bps as u64;
    let ptype = if fat16_size == 0 {
        0x0C // FAT32 LBA
    } else if total <= 16 << 20 {
        0x01 // FAT12
    } else {
        0x0E // FAT16 LBA
    };
    Some(Vbr { ptype, bps, size: (total > 0).then_some(total) })
}

/// Rewrite a FAT boot sector so Windows accepts it: an x86 jump instruction, a valid
/// media byte, and 512-byte logical sectors. Changing the sector size only rescales the
/// BPB's counts, so every structure stays at the same byte offset. Returns None if no
/// change is needed (or the sector is not FAT).
fn normalise_fat_boot(b: &[u8]) -> Option<Vec<u8>> {
    let v = is_vbr(b, true)?;
    if v.ptype == 0x07 {
        return None; // NTFS / exFAT
    }
    let mut s = b[..512.min(b.len())].to_vec();
    let mut changed = false;
    if !(s[0] == 0xEB || s[0] == 0xE9) {
        s[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
        changed = true;
    }
    if s[21] != 0xF0 && s[21] < 0xF8 {
        s[21] = 0xF8;
        changed = true;
    }
    let bps = v.bps as u32;
    if bps > 512 {
        let k = bps / 512;
        let spc = s[13] as u32 * k;
        if spc > 128 {
            return changed.then_some(s); // clusters over 64 KiB: leave as is
        }
        let set16 = |s: &mut Vec<u8>, o: usize, v: u32| s[o..o + 2].copy_from_slice(&(v as u16).to_le_bytes());
        let set32 = |s: &mut Vec<u8>, o: usize, v: u32| s[o..o + 4].copy_from_slice(&v.to_le_bytes());
        let reserved = le16(&s, 14) as u32 * k;
        let root = le16(&s, 17) as u32;
        // Root directory size is rounded to whole old sectors; keep that padding.
        let root_bytes = (root * 32).div_ceil(bps) * bps;
        let total = if le16(&s, 19) != 0 { le16(&s, 19) as u32 } else { le32(&s, 32) } * k;
        let fat16 = le16(&s, 22) as u32 * k;
        let hidden = le32(&s, 28) * k;
        set16(&mut s, 11, 512);
        s[13] = spc as u8;
        set16(&mut s, 14, reserved);
        set16(&mut s, 17, root_bytes / 32);
        if total < 0x10000 && le16(&s, 19) != 0 {
            set16(&mut s, 19, total);
        } else {
            set16(&mut s, 19, 0);
            set32(&mut s, 32, total);
        }
        if fat16 != 0 {
            set16(&mut s, 22, fat16);
        } else {
            let fat32 = le32(&s, 36) * k;
            set32(&mut s, 36, fat32);
        }
        set32(&mut s, 28, hidden);
        changed = true;
    }
    if s.len() >= 512 {
        s[510] = 0x55;
        s[511] = 0xAA;
    }
    changed.then_some(s)
}

/// Present a volume with a Windows-friendly boot sector.
fn fixed_volume(mut vol: Box<dyn Source>) -> io::Result<Box<dyn Source>> {
    let mut head = vec![0u8; 512];
    vol.read_at(0, &mut head)?;
    Ok(match normalise_fat_boot(&head) {
        Some(boot) => {
            let len = vol.len();
            Box::new(Concat::new(vec![Segment::Bytes(boot), Segment::Src(Box::new(Window::new(vol, 512, len.saturating_sub(512))))]))
        }
        None => vol,
    })
}

/// Build a disk with an MBR holding up to four volumes, each at a 1 MiB boundary.
pub fn wrap_volumes(vols: Vec<(Box<dyn Source>, u8)>) -> Result<Box<dyn Source>> {
    if vols.is_empty() || vols.len() > 4 {
        bail!("can only wrap 1-4 volumes");
    }
    const ALIGN: u64 = 1 << 20;
    let mut mbr = vec![0u8; 512];
    let sig = crate::util::random_u64() as u32 | 1;
    mbr[440..444].copy_from_slice(&sig.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    let mut segs = Vec::new();
    let mut pos = ALIGN;
    let mut body = Vec::new();
    for (i, (vol, ptype)) in vols.into_iter().enumerate() {
        let vol = fixed_volume(vol)?;
        let len = vol.len().div_ceil(512) * 512;
        let pe = &mut mbr[446 + i * 16..462 + i * 16];
        pe[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        pe[4] = ptype;
        pe[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        pe[8..12].copy_from_slice(&((pos / 512) as u32).to_le_bytes());
        pe[12..16].copy_from_slice(&((len / 512) as u32).to_le_bytes());
        body.push(Segment::Src(Box::new(Window::new(vol, 0, len))));
        let next = (pos + len).div_ceil(ALIGN) * ALIGN;
        body.push(Segment::Zero(next - pos - len));
        pos = next;
    }
    segs.push(Segment::Bytes(mbr));
    segs.push(Segment::Zero(ALIGN - 512));
    segs.extend(body);
    segs.push(Segment::Zero(MIN_DISK.saturating_sub(pos)));
    Ok(Box::new(Concat::new(segs)))
}

/// Set by `convert` when the output isn't meant for Windows to mount: disks then keep
/// their exact contents (no MBR added around bare volumes, no padding, no new disk IDs).
pub static EXACT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn exact() -> bool {
    EXACT.load(std::sync::atomic::Ordering::Relaxed)
}

/// A raw disk image (or a bare volume, which gets an MBR around it).
pub fn raw_disk(mut src: Box<dyn Source>, lenient_vbr: bool) -> Result<Box<dyn Source>> {
    if exact() {
        return Ok(src);
    }
    let mut head = vec![0u8; 512];
    src.read_at(0, &mut head)?;
    if let Some(v) = is_vbr(&head, lenient_vbr) {
        return wrap_volumes(vec![(src, v.ptype)]);
    }
    let len = src.len().div_ceil(512) * 512;
    Ok(Box::new(Concat::new(vec![
        Segment::Src(Box::new(Window::new(src, 0, len))),
        Segment::Zero(MIN_DISK.saturating_sub(len)),
    ])))
}

// ---------------------------------------------------------------- disk IDs

/// A source with some byte ranges replaced.
pub struct Patched {
    inner: Box<dyn Source>,
    patches: Vec<(u64, Vec<u8>)>,
}

impl Source for Patched {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn is_zero(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let end = offset + len;
        if self.patches.iter().any(|(at, d)| *at < end && at + d.len() as u64 > offset) {
            return Ok(false);
        }
        self.inner.is_zero(offset, len)
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.inner.read_at(offset, buf)?;
        let end = offset + buf.len() as u64;
        for (at, data) in &self.patches {
            let (a, b) = ((*at).max(offset), (*at + data.len() as u64).min(end));
            if a < b {
                buf[(a - offset) as usize..(b - offset) as usize].copy_from_slice(&data[(a - at) as usize..(b - at) as usize]);
            }
        }
        Ok(())
    }
}

fn new_guid() -> [u8; 16] {
    let mut g = crate::util::random_bytes::<16>();
    g[7] = (g[7] & 0x0F) | 0x40; // version 4 (data3 is little-endian)
    g[8] = (g[8] & 0x3F) | 0x80; // RFC 4122 variant
    g
}

/// Give the disk fresh identifiers when Windows could not bring it online as is:
/// an MBR disk without a signature, or an MBR signature / GPT GUID that is already used
/// by a disk in this PC (e.g. an image of the system drive). Windows resolves such
/// clashes by rewriting the ID, which it cannot do on our read-only copies.
/// Only the copy changes; the original image is never written.
pub fn fix_ids(mut src: Box<dyn Source>) -> Result<Box<dyn Source>> {
    let mut head = vec![0u8; 1024];
    src.read_at(0, &mut head)?;
    if head[510..512] != [0x55, 0xAA] {
        return Ok(src);
    }
    let ids = crate::vdisk::disk_ids();
    let protective = (0..4).any(|i| head[446 + i * 16 + 4] == 0xEE);
    let mut patches = Vec::new();
    if protective && &head[512..520] == b"EFI PART" {
        gpt_patches(src.as_mut(), &head[512..1024], ids, &mut patches)?;
    } else if !protective {
        let sig = le32(&head, 440);
        if sig == 0 || ids.mbr.contains(&sig) {
            let mut fresh = 0;
            while fresh == 0 || ids.mbr.contains(&fresh) {
                fresh = crate::util::random_u64() as u32;
            }
            let mut mbr = head[..512].to_vec();
            mbr[440..444].copy_from_slice(&fresh.to_le_bytes());
            patches.push((0, mbr));
        }
    }
    Ok(if patches.is_empty() { src } else { Box::new(Patched { inner: src, patches }) })
}

fn gpt_patches(src: &mut dyn Source, primary: &[u8], ids: &crate::vdisk::DiskIds, out: &mut Vec<(u64, Vec<u8>)>) -> Result<()> {
    let hsize = le32(primary, 12) as usize;
    let entries_lba = crate::source::le64(primary, 72);
    let (count, esize) = (le32(primary, 80) as usize, le32(primary, 84) as usize);
    if !(92..=512).contains(&hsize) || esize < 128 || count * esize > (4 << 20) {
        return Ok(()); // unusual layout: leave it alone
    }
    let mut entries = vec![0u8; count * esize];
    src.read_at(entries_lba * 512, &mut entries)?;
    let disk_guid: [u8; 16] = primary[56..72].try_into().unwrap();
    let part_clash = entries.chunks_exact(esize).any(|e| e[0..16] != [0; 16] && ids.gpt_partitions.iter().any(|p| p[..] == e[16..32]));
    if !ids.gpt_disks.contains(&disk_guid) && !part_clash {
        return Ok(());
    }
    // New disk GUID, and new unique GUIDs for every partition.
    let guid = new_guid();
    for e in entries.chunks_exact_mut(esize) {
        if e[0..16] != [0; 16] {
            e[16..32].copy_from_slice(&new_guid());
        }
    }
    let array_crc = crc32fast::hash(&entries);
    let fix_header = |h: &[u8]| -> Vec<u8> {
        let mut h = h[..512].to_vec();
        h[56..72].copy_from_slice(&guid);
        h[88..92].copy_from_slice(&array_crc.to_le_bytes());
        h[16..20].fill(0);
        let crc = crc32fast::hash(&h[..hsize]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    };
    out.push((512, fix_header(primary)));
    out.push((entries_lba * 512, entries.clone()));
    // Backup header (normally the last sector) and its copy of the partition array.
    let alt = crate::source::le64(primary, 32);
    let mut backup = vec![0u8; 512];
    src.read_at(alt * 512, &mut backup)?;
    if &backup[0..8] == b"EFI PART" {
        out.push((crate::source::le64(&backup, 72) * 512, entries));
        out.push((alt * 512, fix_header(&backup)));
    }
    Ok(())
}

// ---------------------------------------------------------------- PC-98

pub enum Pc98 {
    Hdi,
    Fdi,
    Nhd,
}

/// Recognise PC-98 emulator images by header (HDI/FDI have no magic; check sanity).
pub fn pc98_kind(path: &Path, head: &[u8], len: u64) -> Option<Pc98> {
    if head.starts_with(b"T98HDDIMAGE.R0\0") {
        return Some(Pc98::Nhd);
    }
    let ext = super::ext(path);
    if ext != "hdi" && ext != "fdi" {
        return None;
    }
    let hsize = le32(head, 8) as u64;
    let dsize = le32(head, 12) as u64;
    let ss = le32(head, 16);
    if !matches!(ss, 256 | 512 | 1024 | 2048) || hsize == 0 || hsize > len || dsize == 0 || hsize + dsize > len + 4096 {
        return None;
    }
    Some(if ext == "hdi" { Pc98::Hdi } else { Pc98::Fdi })
}

/// PC-98 disk geometry and data location from an emulator header.
struct Pc98Geo {
    data: u64,
    size: u64,
    sector: u64,
    sectors: u64,
    heads: u64,
}

fn pc98_geo(kind: &Pc98, path: &Path, len: u64) -> Result<Pc98Geo> {
    let f = open_ro(path)?;
    let h = read_vec(&f, 0, 512)?;
    Ok(match kind {
        Pc98::Nhd => {
            let data = le32(&h, 0x110) as u64;
            Pc98Geo {
                data,
                size: len - data,
                heads: le16(&h, 0x118) as u64,
                sectors: le16(&h, 0x11A) as u64,
                sector: le16(&h, 0x11C) as u64,
            }
        }
        _ => {
            let data = le32(&h, 8) as u64;
            Pc98Geo {
                data,
                size: (le32(&h, 12) as u64).min(len - data),
                sector: le32(&h, 16) as u64,
                sectors: le32(&h, 20) as u64,
                heads: le32(&h, 24) as u64,
            }
        }
    })
}

/// Turn a PC-98 HDI/FDI/NHD image into a Windows-readable disk.
pub fn pc98(path: &Path, kind: Pc98, len: u64) -> Result<Box<dyn Source>> {
    let g = pc98_geo(&kind, path, len)?;
    let disk = || -> Result<Box<dyn Source>> { Ok(Box::new(FileSlice::new(open_ro(path)?, g.data, g.size))) };
    let mut d = disk()?;
    if exact() {
        return Ok(d); // the disk as the PC-98 sees it
    }
    let mut first = vec![0u8; 512];
    d.read_at(0, &mut first)?;
    // Floppies (and some hard disks) hold a single FAT volume from sector 0.
    if let Some(v) = is_vbr(&first, true) {
        return wrap_volumes(vec![(d, v.ptype)]);
    }
    if matches!(kind, Pc98::Fdi) {
        bail!("this PC-98 floppy has no FAT boot sector (it may be a non-DOS or copy-protected disk)");
    }
    // Hard disk: PC-98 partition table in the second sector, 16 entries of 32 bytes.
    let ss = g.sector.max(256);
    let mut table = vec![0u8; 512];
    d.read_at(ss, &mut table)?;
    let spc = g.sectors * g.heads; // sectors per cylinder
    let mut vols: Vec<(Box<dyn Source>, u8)> = Vec::new();
    let mut starts: Vec<u64> = Vec::new();
    for e in table.chunks_exact(32) {
        let (mid, sid) = (e[0], e[1]);
        if mid == 0 && sid == 0 {
            continue;
        }
        let sec = e[8] as u64;
        let head = e[9] as u64;
        let cyl = le16(e, 10) as u64;
        starts.push((cyl * spc + head * g.sectors + sec) * ss);
    }
    // Fall back to looking for boot sectors at cylinder boundaries.
    if starts.is_empty() && spc > 0 {
        starts.extend((1..256).map(|c| c * spc * ss).filter(|&o| o < g.size));
    }
    for off in starts {
        if vols.len() == 4 {
            break;
        }
        let mut b = vec![0u8; 512];
        d.read_at(off, &mut b)?;
        if let Some(v) = is_vbr(&b, true) {
            let size = v.size.unwrap_or(g.size - off).min(g.size - off);
            vols.push((Box::new(Window::new(disk()?, off, size)), v.ptype));
        }
    }
    if vols.is_empty() {
        bail!("no FAT partitions found on this PC-98 disk");
    }
    wrap_volumes(vols)
}

// ---------------------------------------------------------------- D88 (PC-98 / PC-88 / FM-7 / X1 floppies)

pub fn is_d88_ext(ext: &str) -> bool {
    matches!(ext, "d88" | "d77" | "d68" | "88d" | "d98" | "98d" | "1dd")
}

/// Decode a D88 floppy into plain sectors: tracks in order, sectors sorted by number.
/// Only the first disk of a multi-disk D88 file is used.
pub fn d88(path: &Path) -> Result<Vec<u8>> {
    let d = std::fs::read(path)?;
    if d.len() < 0x2B0 || d.len() > (64 << 20) {
        bail!("not a D88 floppy image");
    }
    let disk = &d[..(le32(&d, 0x1C) as usize).clamp(0x2B0, d.len())];
    let track_at = |t: usize| le32(&d, 0x20 + t * 4) as usize;
    let read_track = |off: usize| -> Vec<(u8, &[u8])> {
        let mut sectors = Vec::new();
        let count = disk.get(off + 4..off + 6).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize).unwrap_or(0);
        let mut p = off;
        for _ in 0..count.min(64) {
            let Some(h) = disk.get(p..p + 16) else { break };
            let size = le16(h, 14) as usize;
            let Some(data) = disk.get(p + 16..p + 16 + size) else { break };
            sectors.push((h[2], data));
            p += 16 + size;
        }
        sectors.sort_by_key(|s| s.0);
        sectors
    };
    let tracks: Vec<usize> = (0..164).map(track_at).collect();
    let last = tracks.iter().rposition(|&o| o >= 0x2B0 && o < disk.len()).ok_or_else(|| anyhow::anyhow!("this D88 image has no tracks"))?;
    // Size of a normal track, for filling in tracks that are missing from the file.
    let track_bytes = tracks
        .iter()
        .filter(|&&o| o >= 0x2B0 && o < disk.len())
        .map(|&o| read_track(o).iter().map(|s| s.1.len()).sum::<usize>())
        .max()
        .unwrap_or(0);
    let mut out = Vec::with_capacity((last + 1) * track_bytes);
    for &o in &tracks[..=last] {
        let start = out.len();
        if o >= 0x2B0 && o < disk.len() {
            for (_, data) in read_track(o) {
                out.extend_from_slice(data);
            }
        }
        out.resize(start + track_bytes.max(out.len() - start), 0);
    }
    Ok(out)
}

// ---------------------------------------------------------------- Atari ST

/// Decode a Magic Shadow Archiver (.msa) Atari ST floppy into raw sectors.
pub fn unmsa(path: &Path) -> Result<Vec<u8>> {
    let m = std::fs::read(path)?;
    let be = |o: usize| u16::from_be_bytes([m[o], m[o + 1]]) as usize;
    if m.len() < 10 || be(0) != 0x0E0F {
        bail!("not an MSA image");
    }
    let (spt, sides, first, last) = (be(2), be(4) + 1, be(6), be(8));
    if spt == 0 || spt > 64 || last < first || last > 255 {
        bail!("corrupt MSA header");
    }
    let track = spt * 512;
    let mut out = vec![0u8; (last + 1) * sides * track];
    let mut p = 10;
    for t in first..=last {
        for s in 0..sides {
            if p + 2 > m.len() {
                bail!("truncated MSA image");
            }
            let n = be(p);
            p += 2;
            let data = m.get(p..p + n).ok_or_else(|| anyhow::anyhow!("truncated MSA image"))?;
            p += n;
            let dst = &mut out[(t * sides + s) * track..(t * sides + s + 1) * track];
            if n == track {
                dst.copy_from_slice(data);
                continue;
            }
            // RLE: E5 <byte> <count:u16 BE> repeats; anything else is literal.
            let (mut i, mut o) = (0, 0);
            while i < data.len() && o < track {
                if data[i] == 0xE5 && i + 3 < data.len() {
                    let (b, c) = (data[i + 1], u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize);
                    let c = c.min(track - o);
                    dst[o..o + c].fill(b);
                    o += c;
                    i += 4;
                } else {
                    dst[o] = data[i];
                    o += 1;
                    i += 1;
                }
            }
        }
    }
    Ok(out)
}

/// In-memory source (decoded floppies).
pub struct Mem(pub Vec<u8>);
impl Source for Mem {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let o = offset.min(self.0.len() as u64) as usize;
        let n = (self.0.len() - o).min(buf.len());
        buf[..n].copy_from_slice(&self.0[o..o + n]);
        buf[n..].fill(0);
        Ok(())
    }
}
