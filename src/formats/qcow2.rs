//! QEMU QCOW2 (versions 2 and 3, zlib-compressed clusters, no encryption,
//! backing files are followed when present).

use crate::source::{be32, be64, open_ro, pread, read_vec, Source};
use anyhow::{anyhow, bail, Result};
use flate2::read::DeflateDecoder;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

pub const MAGIC: &[u8; 4] = b"QFI\xfb";
const OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;
const COMPRESSED: u64 = 1 << 62;
const ZERO_FLAG: u64 = 1;

pub struct Qcow2 {
    file: File,
    size: u64,
    cluster_bits: u32,
    l1: Vec<u64>,
    l2_cache: HashMap<u64, Vec<u64>>,
    cluster_cache: Option<(u64, Vec<u8>)>,
    /// Compressed clusters use zstd instead of deflate (qcow2 v3 compression type 1).
    zstd: bool,
    backing: Option<Box<dyn Source>>,
}

impl Qcow2 {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_depth(path, 0)
    }

    fn open_depth(path: &Path, depth: u32) -> Result<Self> {
        if depth > 16 {
            bail!("qcow2 backing chain too deep");
        }
        let file = open_ro(path)?;
        let h = read_vec(&file, 0, 112)?;
        if &h[0..4] != MAGIC {
            bail!("not a qcow2 image");
        }
        let version = be32(&h, 4);
        if version != 2 && version != 3 {
            bail!("unsupported qcow2 version {version}");
        }
        let backing_off = be64(&h, 8);
        let backing_len = be32(&h, 16) as usize;
        let cluster_bits = be32(&h, 20);
        let size = be64(&h, 24);
        let crypt = be32(&h, 32);
        let l1_size = be32(&h, 36) as usize;
        let l1_off = be64(&h, 40);
        if crypt != 0 {
            bail!("encrypted qcow2 images are not supported");
        }
        if !(9..=21).contains(&cluster_bits) {
            bail!("corrupt qcow2 header (cluster_bits={cluster_bits})");
        }
        let mut zstd = false;
        if version == 3 {
            let incompat = be64(&h, 72);
            let header_len = be32(&h, 100);
            if incompat & (1 << 2) != 0 {
                bail!("qcow2 images with an external data file are not supported");
            }
            if incompat & (1 << 4) != 0 {
                bail!("qcow2 images with extended L2 entries (subclusters) are not supported");
            }
            if incompat & (1 << 3) != 0 && header_len > 104 {
                zstd = match h[104] {
                    0 => false,
                    1 => true,
                    t => bail!("unknown qcow2 compression type {t}"),
                };
            }
        }
        let raw = read_vec(&file, l1_off, l1_size * 8)?;
        let l1 = raw.chunks_exact(8).map(|c| u64::from_be_bytes(c.try_into().unwrap())).collect();
        let backing: Option<Box<dyn Source>> = if backing_off != 0 && backing_len > 0 {
            let name = read_vec(&file, backing_off, backing_len)?;
            let name = String::from_utf8_lossy(&name).to_string();
            let bp = super::cd::resolve_sibling(path, &name)
                .ok_or_else(|| anyhow!("qcow2 backing file '{name}' not found"))?;
            let mut head = [0u8; 4];
            pread(&open_ro(&bp)?, 0, &mut head)?;
            if &head == MAGIC {
                Some(Box::new(Qcow2::open_depth(&bp, depth + 1)?))
            } else {
                Some(Box::new(crate::source::FileSlice::whole(&bp)?))
            }
        } else {
            None
        };
        Ok(Self { file, size, cluster_bits, l1, l2_cache: HashMap::new(), cluster_cache: None, backing, zstd })
    }

    fn l2_entry(&mut self, cluster_index: u64) -> io::Result<u64> {
        let l2_bits = self.cluster_bits - 3;
        let l1i = (cluster_index >> l2_bits) as usize;
        let l2i = (cluster_index & ((1 << l2_bits) - 1)) as usize;
        let l2_off = match self.l1.get(l1i) {
            Some(&e) => e & OFFSET_MASK,
            None => 0,
        };
        if l2_off == 0 {
            return Ok(0);
        }
        if !self.l2_cache.contains_key(&l2_off) {
            let raw = read_vec(&self.file, l2_off, 1usize << self.cluster_bits)?;
            let t = raw.chunks_exact(8).map(|c| u64::from_be_bytes(c.try_into().unwrap())).collect();
            self.l2_cache.insert(l2_off, t);
        }
        Ok(self.l2_cache[&l2_off][l2i])
    }
}

/// Decode a zstd-compressed cluster: one or more zstd frames, then zero padding.
fn unzstd(comp: &[u8], out: &mut [u8]) -> io::Result<()> {
    const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
    let mut input = comp;
    let mut got = 0;
    while got < out.len() && input.len() >= 4 && input[..4] == MAGIC {
        let mut dec = ruzstd::decoding::StreamingDecoder::new(&mut input).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("zstd: {e}")))?;
        loop {
            match dec.read(&mut out[got..])? {
                0 => break,
                k => got += k,
            }
            if got == out.len() {
                break;
            }
        }
    }
    Ok(())
}

impl Source for Qcow2 {
    fn len(&self) -> u64 {
        self.size
    }
    fn is_zero(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let cs = 1u64 << self.cluster_bits;
        let l2_span = cs * (cs / 8); // bytes covered by one L2 table
        let end = (offset + len).min(self.size);
        let mut c = offset / cs;
        while c * cs < end {
            // A whole L2 table that is absent: skip it in one step.
            let l1i = ((c * cs) / l2_span) as usize;
            if self.l1.get(l1i).map(|e| e & OFFSET_MASK).unwrap_or(0) == 0 {
                if let Some(b) = self.backing.as_mut() {
                    let (a, z) = (c * cs, ((l1i as u64 + 1) * l2_span).min(end));
                    if !b.is_zero(a, z - a)? {
                        return Ok(false);
                    }
                }
                c = (l1i as u64 + 1) * l2_span / cs;
                continue;
            }
            let e = self.l2_entry(c)?;
            if e & COMPRESSED != 0 || (e & OFFSET_MASK != 0 && e & ZERO_FLAG == 0) {
                return Ok(false);
            }
            if e & ZERO_FLAG == 0 {
                if let Some(b) = self.backing.as_mut() {
                    if !b.is_zero(c * cs, cs)? {
                        return Ok(false);
                    }
                }
            }
            c += 1;
        }
        Ok(true)
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let cs = 1u64 << self.cluster_bits;
        let mut done = 0;
        while done < buf.len() {
            let off = offset + done as u64;
            let ci = off / cs;
            let within = off % cs;
            let n = ((cs - within) as usize).min(buf.len() - done);
            let out = &mut buf[done..done + n];
            if off >= self.size {
                out.fill(0);
                done += n;
                continue;
            }
            let e = self.l2_entry(ci)?;
            if e & COMPRESSED != 0 {
                if self.cluster_cache.as_ref().map(|(c, _)| *c) != Some(ci) {
                    let x = 62 - (self.cluster_bits - 8);
                    let host = e & ((1u64 << x) - 1);
                    let nsect = ((e >> x) & ((1u64 << (self.cluster_bits - 8)) - 1)) + 1;
                    let clen = (nsect * 512 - (host & 511)) as usize;
                    let comp = read_vec(&self.file, host, clen)?;
                    let mut dec = vec![0u8; cs as usize];
                    if self.zstd {
                        unzstd(&comp, &mut dec)?;
                    } else {
                        let mut r = DeflateDecoder::new(&comp[..]);
                        let mut got = 0;
                        while got < dec.len() {
                            match r.read(&mut dec[got..]) {
                                Ok(0) => break,
                                Ok(k) => got += k,
                                Err(e) => return Err(e),
                            }
                        }
                    }
                    self.cluster_cache = Some((ci, dec));
                }
                let c = &self.cluster_cache.as_ref().unwrap().1;
                out.copy_from_slice(&c[within as usize..within as usize + n]);
            } else if e & ZERO_FLAG != 0 {
                out.fill(0);
            } else if e & OFFSET_MASK != 0 {
                pread(&self.file, (e & OFFSET_MASK) + within, out)?;
            } else if let Some(b) = self.backing.as_mut() {
                b.read_at(off, out)?;
            } else {
                out.fill(0);
            }
            done += n;
        }
        Ok(())
    }
}
