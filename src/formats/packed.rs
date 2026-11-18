//! Block-compressed ISOs used by PSP/PS2 emulators and loaders:
//! CSO (CISO v1 deflate, v2 deflate+LZ4), ZSO (LZ4) and DAX (zlib).

use crate::source::{le16, le32, le64, open_ro, read_vec, Source};
use anyhow::{bail, Result};
use flate2::read::{DeflateDecoder, ZlibDecoder};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    CsoV1,
    CsoV2,
    Zso,
    Dax,
}

pub struct Packed {
    file: File,
    kind: Kind,
    size: u64,
    block: u64,
    align: u32,
    /// Block index: file offset (and flag bit) per block, plus one end entry (CSO/ZSO).
    index: Vec<u32>,
    /// DAX only: compressed length per block and uncompressed (NC) flags.
    dax_len: Vec<u16>,
    dax_nc: Vec<bool>,
    cache: Option<(u64, Vec<u8>)>,
    /// Blocks decoded ahead of time by the parallel prefetcher.
    batch: std::collections::HashMap<u64, Vec<u8>>,
    last: Option<u64>,
}

pub fn magic(head: &[u8]) -> Option<&'static str> {
    match &head[0..4] {
        b"CISO" => Some("CSO"),
        b"ZISO" => Some("ZSO"),
        b"DAX\0" => Some("DAX"),
        _ => None,
    }
}

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

impl Packed {
    pub fn open(path: &Path) -> Result<Self> {
        let file = open_ro(path)?;
        let h = read_vec(&file, 0, 32)?;
        match &h[0..4] {
            b"CISO" | b"ZISO" => {
                let size = le64(&h, 8);
                let block = le32(&h, 16) as u64;
                let ver = h[20];
                let align = h[21] as u32;
                if block == 0 || block > 1 << 24 {
                    bail!("corrupt CSO/ZSO header");
                }
                let kind = match (&h[0..4], ver) {
                    (b"ZISO", _) => Kind::Zso,
                    (_, 2) => Kind::CsoV2,
                    _ => Kind::CsoV1,
                };
                let blocks = size.div_ceil(block) as usize;
                let raw = read_vec(&file, 24, (blocks + 1) * 4)?;
                let index = raw.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
                Ok(Self { file, kind, size, block, align, index, dax_len: vec![], dax_nc: vec![], cache: None, batch: Default::default(), last: None })
            }
            b"DAX\0" => {
                let size = le32(&h, 4) as u64;
                let version = le32(&h, 8);
                let nc_areas = le32(&h, 12) as usize;
                const BLOCK: u64 = 8192;
                let blocks = size.div_ceil(BLOCK) as usize;
                let mut pos = 32u64;
                let raw = read_vec(&file, pos, blocks * 4)?;
                pos += blocks as u64 * 4;
                let index = raw.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
                let raw = read_vec(&file, pos, blocks * 2)?;
                pos += blocks as u64 * 2;
                let dax_len = (0..blocks).map(|i| le16(&raw, i * 2)).collect();
                let mut dax_nc = vec![false; blocks];
                if version >= 1 && nc_areas > 0 {
                    let raw = read_vec(&file, pos, nc_areas * 8)?;
                    for a in 0..nc_areas {
                        let (start, count) = (le32(&raw, a * 8) as usize, le32(&raw, a * 8 + 4) as usize);
                        for b in start..(start + count).min(blocks) {
                            dax_nc[b] = true;
                        }
                    }
                }
                Ok(Self { file, kind: Kind::Dax, size, block: BLOCK, align: 0, index, dax_len, dax_nc, cache: None, batch: Default::default(), last: None })
            }
            _ => bail!("not a CSO/ZSO/DAX image"),
        }
    }

    /// Read block `b`'s stored bytes and how to decode them.
    fn fetch(&self, b: u64) -> io::Result<(Codec, Vec<u8>)> {
        let bs = self.block as usize;
        let i = b as usize;
        let bad = || io::Error::new(io::ErrorKind::InvalidData, format!("block {b} out of range"));
        Ok(match self.kind {
            Kind::Dax => {
                let off = *self.index.get(i).ok_or_else(bad)? as u64;
                if self.dax_nc[i] {
                    (Codec::Raw, read_vec(&self.file, off, bs)?)
                } else {
                    (Codec::Zlib, read_vec(&self.file, off, self.dax_len[i] as usize)?)
                }
            }
            _ => {
                let cur = *self.index.get(i).ok_or_else(bad)?;
                let next = *self.index.get(i + 1).ok_or_else(bad)?;
                let flag = cur & 0x8000_0000 != 0;
                let pos = ((cur & 0x7FFF_FFFF) as u64) << self.align;
                let end = ((next & 0x7FFF_FFFF) as u64) << self.align;
                let clen = end.saturating_sub(pos) as usize;
                // With alignment, the stored length may include padding past the data.
                let comp = read_vec(&self.file, pos, clen.clamp(1, bs * 2 + 64))?;
                let codec = match self.kind {
                    Kind::CsoV1 | Kind::Zso if flag => Codec::Raw,
                    Kind::CsoV2 if clen >= bs => Codec::Raw,
                    Kind::Zso => Codec::Lz4,
                    Kind::CsoV2 if flag => Codec::Lz4,
                    _ => Codec::Deflate,
                };
                (codec, comp)
            }
        })
    }

    fn load(&mut self, b: u64) -> io::Result<()> {
        if self.cache.as_ref().map(|c| c.0) == Some(b) {
            return Ok(());
        }
        if let Some(out) = self.batch.remove(&b) {
            self.cache = Some((b, out));
            self.last = Some(b);
            return Ok(());
        }
        let blocks = self.size.div_ceil(self.block);
        let sequential = self.last.is_some_and(|l| b == l + 1);
        self.last = Some(b);
        let bs = self.block as usize;
        if sequential && b + 1 < blocks {
            // Reading straight through (a conversion): read the next stretch in order,
            // then decompress it on all cores.
            let end = (b + (8 << 20) / self.block.max(1)).clamp(b + 1, blocks);
            let stored = (b..end).map(|i| self.fetch(i).map(|f| (i, f))).collect::<io::Result<Vec<_>>>()?;
            let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(16);
            let per = stored.len().div_ceil(threads);
            let decoded: Vec<io::Result<Vec<(u64, Vec<u8>)>>> = std::thread::scope(|s| {
                let hs: Vec<_> = stored
                    .chunks(per.max(1))
                    .map(|part| s.spawn(move || part.iter().map(|(i, (c, data))| decode(*c, data, bs, *i).map(|o| (*i, o))).collect()))
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(io::Error::other("decoder thread failed")))).collect()
            });
            self.batch.clear();
            for d in decoded {
                self.batch.extend(d?);
            }
            let out = self.batch.remove(&b).ok_or_else(|| io::Error::other("block missing from batch"))?;
            self.cache = Some((b, out));
            return Ok(());
        }
        let (c, data) = self.fetch(b)?;
        self.cache = Some((b, decode(c, &data, bs, b)?));
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Codec {
    Raw,
    Deflate,
    Zlib,
    Lz4,
}

fn decode(codec: Codec, comp: &[u8], bs: usize, b: u64) -> io::Result<Vec<u8>> {
    let mut out = vec![0u8; bs];
    match codec {
        Codec::Raw => {
            let n = bs.min(comp.len());
            out[..n].copy_from_slice(&comp[..n]);
        }
        Codec::Zlib => {
            fill(ZlibDecoder::new(comp), &mut out)?;
        }
        Codec::Deflate => {
            fill(DeflateDecoder::new(comp), &mut out)?;
        }
        Codec::Lz4 => {
            lz4_flex::block::decompress_into(comp, &mut out)
                .or_else(|_| {
                    // Trailing alignment padding confuses strict LZ4 parsing; trim zeros.
                    let t = comp.iter().rposition(|&x| x != 0).map(|p| p + 1).unwrap_or(0);
                    lz4_flex::block::decompress_into(&comp[..t], &mut out)
                })
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("LZ4 block {b}: {e}")))?;
        }
    }
    Ok(out)
}

impl Source for Packed {
    fn len(&self) -> u64 {
        self.size
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let off = offset + done as u64;
            if off >= self.size {
                buf[done..].fill(0);
                break;
            }
            let within = (off % self.block) as usize;
            let n = (self.block as usize - within).min(buf.len() - done);
            self.load(off / self.block)?;
            let c = &self.cache.as_ref().unwrap().1;
            buf[done..done + n].copy_from_slice(&c[within..within + n]);
            done += n;
        }
        Ok(())
    }
}
