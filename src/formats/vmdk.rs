//! VMware VMDK: monolithic/split sparse ("KDMV" hosted sparse extents, including
//! streamOptimized with compressed grains), and descriptor files that reference
//! FLAT / VMFS / SPARSE / ZERO extents.

use crate::source::{le32, le64, open_ro, pread, read_vec, Concat, FileSlice, Segment, Source};
use anyhow::{anyhow, bail, Context, Result};
use flate2::read::{DeflateDecoder, ZlibDecoder};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

pub const MAGIC: &[u8; 4] = b"KDMV";
const GD_AT_END: u64 = u64::MAX;

pub struct SparseExtent {
    file: File,
    capacity: u64, // bytes
    grain_bytes: u64,
    gtes_per_gt: u64,
    gd: Vec<u32>,
    compressed: bool,
    zero_gte: bool,
    gt_cache: HashMap<u32, Vec<u32>>,
    grain_cache: Option<(u64, Vec<u8>)>,
}

impl SparseExtent {
    pub fn open(path: &Path) -> Result<Self> {
        let file = open_ro(path)?;
        let len = file.metadata()?.len();
        let mut h = read_vec(&file, 0, 512)?;
        if &h[0..4] != MAGIC {
            if &h[0..4] == b"COWD" {
                bail!("{}: ESX COWD (VMFSSPARSE) extents are not supported", path.display());
            }
            bail!("{}: not a VMDK sparse extent", path.display());
        }
        if le64(&h, 56) == GD_AT_END {
            // streamOptimized: the real header is in the footer, 1024 bytes before EOF.
            if len < 1536 {
                bail!("truncated streamOptimized VMDK");
            }
            let f = read_vec(&file, len - 1024, 512)?;
            if &f[0..4] != MAGIC {
                bail!("streamOptimized VMDK footer not found");
            }
            h = f;
        }
        let flags = le32(&h, 8);
        let capacity = le64(&h, 12) * 512;
        let grain_sectors = le64(&h, 20);
        let gtes_per_gt = le32(&h, 44) as u64;
        let gd_offset = le64(&h, 56);
        let compress_alg = u16::from_le_bytes([h[77], h[78]]);
        // Real grains are 64 KiB; grain tables hold 512 entries.
        if grain_sectors == 0 || grain_sectors > 2048 || gtes_per_gt == 0 || gtes_per_gt > 4096 || gd_offset == GD_AT_END {
            bail!("corrupt VMDK sparse header");
        }
        let compressed = flags & (1 << 16) != 0 && compress_alg != 0;
        let grains = capacity.div_ceil(grain_sectors * 512);
        let gd_entries = grains.div_ceil(gtes_per_gt) as usize;
        let raw = read_vec(&file, gd_offset * 512, gd_entries * 4)?;
        let gd = raw.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        Ok(Self {
            file,
            capacity,
            grain_bytes: grain_sectors * 512,
            gtes_per_gt,
            gd,
            compressed,
            zero_gte: flags & 4 != 0,
            gt_cache: HashMap::new(),
            grain_cache: None,
        })
    }

    fn gte(&mut self, grain: u64) -> io::Result<u32> {
        let gdi = (grain / self.gtes_per_gt) as usize;
        let gt_sector = match self.gd.get(gdi) {
            Some(&s) if s != 0 => s,
            _ => return Ok(0),
        };
        if !self.gt_cache.contains_key(&gt_sector) {
            let raw = read_vec(&self.file, gt_sector as u64 * 512, self.gtes_per_gt as usize * 4)?;
            let gt = raw.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
            self.gt_cache.insert(gt_sector, gt);
        }
        Ok(self.gt_cache[&gt_sector][(grain % self.gtes_per_gt) as usize])
    }

    fn compressed_grain(&mut self, grain: u64, sector: u32) -> io::Result<&[u8]> {
        if self.grain_cache.as_ref().map(|(g, _)| *g) != Some(grain) {
            let off = sector as u64 * 512;
            let hdr = read_vec(&self.file, off, 12)?;
            let size = le32(&hdr, 8) as usize;
            let comp = read_vec(&self.file, off + 12, size)?;
            let mut out = vec![0u8; self.grain_bytes as usize];
            let n = inflate(&comp, &mut out)?;
            out[n..].fill(0);
            self.grain_cache = Some((grain, out));
        }
        Ok(&self.grain_cache.as_ref().unwrap().1)
    }
}

/// Decompress zlib (or raw deflate) data into `out`, returning bytes produced.
pub fn inflate(comp: &[u8], out: &mut [u8]) -> io::Result<usize> {
    fn fill<R: Read>(mut r: R, out: &mut [u8]) -> io::Result<usize> {
        let mut n = 0;
        while n < out.len() {
            match r.read(&mut out[n..])? {
                0 => break,
                k => n += k,
            }
        }
        Ok(n)
    }
    match fill(ZlibDecoder::new(comp), out) {
        Ok(n) if n > 0 => Ok(n),
        _ => fill(DeflateDecoder::new(comp), out),
    }
}

impl Source for SparseExtent {
    fn len(&self) -> u64 {
        self.capacity
    }
    fn is_zero(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let end = (offset + len).min(self.capacity);
        let gt_span = self.grain_bytes * self.gtes_per_gt;
        let mut g = offset / self.grain_bytes;
        while g * self.grain_bytes < end {
            // A missing grain table covers many grains at once.
            let gdi = (g / self.gtes_per_gt) as usize;
            if self.gd.get(gdi).copied().unwrap_or(0) == 0 {
                g = (gdi as u64 + 1) * gt_span / self.grain_bytes;
                continue;
            }
            let gte = self.gte(g)?;
            if !(gte == 0 || (gte == 1 && self.zero_gte)) {
                return Ok(false);
            }
            g += 1;
        }
        Ok(true)
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let off = offset + done as u64;
            let grain = off / self.grain_bytes;
            let within = off % self.grain_bytes;
            let n = ((self.grain_bytes - within) as usize).min(buf.len() - done);
            let out = &mut buf[done..done + n];
            if off >= self.capacity {
                out.fill(0);
            } else {
                let gte = self.gte(grain)?;
                if gte == 0 || (gte == 1 && self.zero_gte) {
                    out.fill(0);
                } else if self.compressed {
                    let g = self.compressed_grain(grain, gte)?;
                    out.copy_from_slice(&g[within as usize..within as usize + n]);
                } else {
                    pread(&self.file, gte as u64 * 512 + within, out)?;
                }
            }
            done += n;
        }
        Ok(())
    }
}

/// Open a VMDK: either a binary sparse file, or a text descriptor.
pub fn open(path: &Path) -> Result<Box<dyn Source>> {
    let file = open_ro(path)?;
    let head = read_vec(&file, 0, 4)?;
    drop(file);
    if &head[..] == MAGIC {
        // Monolithic sparse. Its embedded descriptor may reference no other files,
        // so the header capacity is authoritative.
        let e = SparseExtent::open(path)?;
        check_embedded_parent(path)?;
        return Ok(Box::new(e));
    }
    let text = std::fs::read(path)?;
    if text.len() > 1 << 20 {
        bail!("{}: not a VMDK descriptor (too large)", path.display());
    }
    open_descriptor(path, &String::from_utf8_lossy(&text))
}

fn check_parent(text: &str) -> Result<()> {
    for line in text.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("parentCID") {
            let v = v.trim_start_matches(|c: char| c == '=' || c.is_whitespace()).trim();
            if !v.eq_ignore_ascii_case("ffffffff") {
                bail!("this VMDK is a snapshot (differencing) disk; mount its base disk instead");
            }
        }
    }
    Ok(())
}

fn check_embedded_parent(path: &Path) -> Result<()> {
    let file = open_ro(path)?;
    let h = read_vec(&file, 0, 512)?;
    let (doff, dsize) = (le64(&h, 28), le64(&h, 36));
    if doff > 0 && dsize > 0 && dsize < 2048 {
        let d = read_vec(&file, doff * 512, (dsize * 512) as usize)?;
        let end = d.iter().position(|&b| b == 0).unwrap_or(d.len());
        check_parent(&String::from_utf8_lossy(&d[..end]))?;
    }
    Ok(())
}

fn open_descriptor(path: &Path, text: &str) -> Result<Box<dyn Source>> {
    if !text.contains("# Disk DescriptorFile") && !text.to_ascii_lowercase().contains("createtype") {
        bail!("{}: not a VMDK descriptor", path.display());
    }
    check_parent(text)?;
    let mut segs = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        let mut it = l.splitn(3, char::is_whitespace);
        let access = it.next().unwrap_or("");
        if !matches!(access, "RW" | "RDONLY" | "NOACCESS") {
            continue;
        }
        let sectors: u64 = it.next().and_then(|s| s.trim().parse().ok()).ok_or_else(|| anyhow!("bad extent line: {l}"))?;
        let rest = it.next().unwrap_or("").trim();
        let (kind, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let bytes = sectors * 512;
        match kind {
            "ZERO" => segs.push(Segment::Zero(bytes)),
            "FLAT" | "VMFS" | "VMFSRAW" | "VMFSRDM" | "SPARSE" | "VMFSSPARSE" => {
                let rest = rest.trim();
                let (name, after) = if let Some(r) = rest.strip_prefix('"') {
                    let end = r.find('"').ok_or_else(|| anyhow!("bad extent line: {l}"))?;
                    (&r[..end], &r[end + 1..])
                } else {
                    rest.split_once(char::is_whitespace).unwrap_or((rest, ""))
                };
                let ext = super::cd::resolve_sibling(path, name)
                    .ok_or_else(|| anyhow!("VMDK extent file '{name}' not found"))?;
                if kind == "SPARSE" || kind == "VMFSSPARSE" {
                    let e = SparseExtent::open(&ext)?;
                    segs.push(Segment::Src(Box::new(Clamp::new(Box::new(e), bytes))));
                } else {
                    let off: u64 = after.trim().parse().unwrap_or(0);
                    let f = open_ro(&ext).with_context(|| format!("opening {}", ext.display()))?;
                    segs.push(Segment::Src(Box::new(FileSlice::new(f, off * 512, bytes))));
                }
            }
            other => bail!("unsupported VMDK extent type {other}"),
        }
    }
    if segs.is_empty() {
        bail!("VMDK descriptor lists no extents");
    }
    Ok(Box::new(Concat::new(segs)))
}

/// Forces a source to an exact length (zero-padding or truncating).
pub struct Clamp {
    inner: Box<dyn Source>,
    len: u64,
}
impl Clamp {
    pub fn new(inner: Box<dyn Source>, len: u64) -> Self {
        Self { inner, len }
    }
}
impl Source for Clamp {
    fn len(&self) -> u64 {
        self.len
    }
    fn is_zero(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let n = self.len.saturating_sub(offset).min(len);
        if n == 0 {
            return Ok(true);
        }
        self.inner.is_zero(offset, n)
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let avail = self.len.saturating_sub(offset).min(buf.len() as u64) as usize;
        let (h, t) = buf.split_at_mut(avail);
        self.inner.read_at(offset, h)?;
        t.fill(0);
        Ok(())
    }
}
