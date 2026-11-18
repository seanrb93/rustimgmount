//! UltraISO compressed ISO (.isz): chunked zlib/bzip2 with an obfuscated chunk table.

use crate::source::{le16, le32, open_ro, pread, read_vec, Source};
use anyhow::{bail, Result};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

pub const MAGIC: &[u8; 4] = b"IsZ!";
const KEY: [u8; 4] = [0xB6, 0x8C, 0xA5, 0xDE];

struct Chunk {
    offset: u64,
    size: u32,
    kind: u8, // 0 zeros, 1 stored, 2 zlib, 3 bzip2
}

pub struct Isz {
    file: File,
    len: u64,
    block: u64,
    chunks: Vec<Chunk>,
    cache: Option<(usize, Vec<u8>)>,
}

fn parse_table(raw: &[u8], ptr_len: usize, data_offs: u64, decode: impl Fn(u8, usize) -> u8) -> (Vec<Chunk>, u64) {
    let bits = ptr_len * 8;
    let mut chunks = Vec::with_capacity(raw.len() / ptr_len);
    let mut pos = data_offs;
    for (i, e) in raw.chunks_exact(ptr_len).enumerate() {
        let mut v: u64 = 0;
        for (j, &b) in e.iter().enumerate() {
            v |= (decode(b, i * ptr_len + j) as u64) << (8 * j);
        }
        let size = (v & ((1u64 << (bits - 2)) - 1)) as u32;
        let kind = (v >> (bits - 2)) as u8;
        chunks.push(Chunk { offset: pos, size, kind });
        if kind != 0 {
            pos += size as u64;
        }
    }
    (chunks, pos)
}

impl Isz {
    pub fn open(path: &Path) -> Result<Self> {
        let file = open_ro(path)?;
        let flen = file.metadata()?.len();
        let h = read_vec(&file, 0, 64)?;
        if &h[0..4] != MAGIC {
            bail!("not an ISZ image");
        }
        let sect = le16(&h, 10) as u64;
        let total = le32(&h, 12) as u64;
        let password = h[16];
        let nblocks = le32(&h, 25) as usize;
        let block = le32(&h, 29) as u64;
        let ptr_len = h[33] as usize;
        let ptr_offs = le32(&h, 35) as u64;
        let data_offs = le32(&h, 43) as u64;
        if password != 0 {
            bail!("this ISZ image is password-protected or encrypted");
        }
        if path.with_extension("i01").exists() {
            bail!("multi-part ISZ images (.isz + .i01 ...) are not supported; join them in UltraISO first");
        }
        let len = total * if sect == 0 { 2048 } else { sect };
        if ptr_offs == 0 {
            // Stored without compression.
            let chunks = vec![Chunk { offset: data_offs, size: len as u32, kind: 1 }];
            return Ok(Self { file, len, block: len.max(1), chunks, cache: None });
        }
        if block == 0 || block > 64 << 20 || !(1..=8).contains(&ptr_len) {
            bail!("corrupt ISZ header");
        }
        let raw = read_vec(&file, ptr_offs, nblocks * ptr_len)?;
        // The pointer table is obfuscated with a 4-byte key. Pick the decoding whose
        // chunk sizes add up to the file size.
        let candidates: [Box<dyn Fn(u8, usize) -> u8>; 3] = [
            Box::new(|b, i| b ^ KEY[i & 3]),
            Box::new(|b, i| !(b ^ KEY[i & 3])),
            Box::new(|b, _| b),
        ];
        let mut best = None;
        for dec in candidates.iter() {
            let (chunks, end) = parse_table(&raw, ptr_len, data_offs, dec);
            let ok = end <= flen && chunks.iter().all(|c| c.kind <= 3 && c.size as u64 <= block * 2 + 64);
            if ok && (end == flen || best.is_none()) {
                let exact = end == flen;
                best = Some(chunks);
                if exact {
                    break;
                }
            }
        }
        let chunks = best.ok_or_else(|| anyhow::anyhow!("cannot decode the ISZ chunk table"))?;
        Ok(Self { file, len, block, chunks, cache: None })
    }

    fn load(&mut self, i: usize) -> io::Result<()> {
        if self.cache.as_ref().map(|c| c.0) == Some(i) {
            return Ok(());
        }
        let c = &self.chunks[i];
        let mut out = vec![0u8; self.block as usize];
        match c.kind {
            0 => {}
            1 => pread(&self.file, c.offset, &mut out[..(c.size as usize).min(self.block as usize)])?,
            2 => {
                let comp = read_vec(&self.file, c.offset, c.size as usize)?;
                super::vmdk::inflate(&comp, &mut out)?;
            }
            3 => {
                let mut comp = read_vec(&self.file, c.offset, c.size as usize)?;
                if comp.len() >= 3 && &comp[0..3] != b"BZh" {
                    comp[0..3].copy_from_slice(b"BZh"); // stored with a disguised signature
                }
                let mut r = bzip2::read::BzDecoder::new(&comp[..]);
                let mut n = 0;
                while n < out.len() {
                    match r.read(&mut out[n..])? {
                        0 => break,
                        k => n += k,
                    }
                }
            }
            k => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unknown ISZ chunk type {k}"))),
        }
        self.cache = Some((i, out));
        Ok(())
    }
}

impl Source for Isz {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let off = offset + done as u64;
            let i = (off / self.block) as usize;
            if off >= self.len || i >= self.chunks.len() {
                buf[done..].fill(0);
                break;
            }
            let within = (off % self.block) as usize;
            let n = (self.block as usize - within).min(buf.len() - done);
            self.load(i)?;
            buf[done..done + n].copy_from_slice(&self.cache.as_ref().unwrap().1[within..within + n]);
            done += n;
        }
        Ok(())
    }
}
