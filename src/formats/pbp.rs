//! PSP EBOOT.PBP files holding PlayStation discs ("PSISOIMG"), as made by popstation,
//! PSX2PSP or psxpackager. Each disc is stored as 0x9300-byte blocks (16 raw sectors),
//! raw-deflate compressed. Official PSN releases are encrypted and are not supported.

use crate::source::{le16, le32, open_ro, pread, read_vec, Source};
use anyhow::{bail, Result};
use flate2::read::DeflateDecoder;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

pub const MAGIC: &[u8; 4] = b"\0PBP";
const BLOCK: u64 = 0x9300;
const INDEX: u64 = 0x4000;
const DATA: u64 = 0x10_0000;

/// File offsets of each disc's PSISOIMG header.
pub fn discs(path: &Path) -> Result<Vec<u64>> {
    let f = open_ro(path)?;
    let h = read_vec(&f, 0, 0x28)?;
    if &h[0..4] != MAGIC {
        bail!("not a PBP file");
    }
    let psar = le32(&h, 0x24) as u64;
    let m = read_vec(&f, psar, 16)?;
    if m.starts_with(b"PSISOIMG0000") {
        return Ok(vec![psar]);
    }
    if m.starts_with(b"PSTITLEIMG000000") {
        let t = read_vec(&f, psar + 0x200, 5 * 4)?;
        let v: Vec<u64> = (0..5).map(|i| le32(&t, i * 4) as u64).filter(|&o| o != 0).map(|o| psar + o).collect();
        if !v.is_empty() {
            return Ok(v);
        }
    }
    if m.starts_with(b"NPUMDIMG") {
        bail!("this EBOOT holds a PSP game (NPUMDIMG), not a PlayStation disc");
    }
    bail!("this EBOOT does not contain a PlayStation disc image")
}

pub struct PbpDisc {
    file: File,
    base: u64,
    /// (offset, length) of each block, relative to the data area.
    index: Vec<(u64, u16)>,
    cache: Option<(usize, Vec<u8>)>,
}

impl PbpDisc {
    pub fn open(path: &Path, disc: usize) -> Result<Self> {
        let all = discs(path)?;
        let base = *all.get(disc).ok_or_else(|| anyhow::anyhow!("this EBOOT has {} disc(s); disc {} does not exist", all.len(), disc + 1))?;
        let file = open_ro(path)?;
        if !read_vec(&file, base, 12)?.starts_with(b"PSISOIMG0000") {
            bail!("corrupt EBOOT disc header");
        }
        let raw = read_vec(&file, base + INDEX, (DATA - INDEX) as usize)?;
        let flen = file.metadata()?.len();
        let mut index = Vec::new();
        let mut expect = 0u64;
        for e in raw.chunks_exact(32) {
            let (off, len) = (le32(e, 0) as u64, le16(e, 4));
            if len == 0 {
                break;
            }
            // Blocks are stored back to back; anything else means an encrypted table.
            if off != expect || base + DATA + off + len as u64 > flen || len as u64 > BLOCK {
                bail!("this EBOOT's disc index is encrypted (an official PSN release?); only EBOOTs made with popstation, PSX2PSP or psxpackager can be mounted");
            }
            expect = off + len as u64;
            index.push((off, len));
        }
        if index.is_empty() {
            bail!("this EBOOT's disc image is empty");
        }
        Ok(Self { file, base, index, cache: None })
    }

    fn load(&mut self, i: usize) -> io::Result<()> {
        if self.cache.as_ref().map(|c| c.0) == Some(i) {
            return Ok(());
        }
        let (off, len) = self.index[i];
        let mut out = vec![0u8; BLOCK as usize];
        let pos = self.base + DATA + off;
        if len as u64 == BLOCK {
            pread(&self.file, pos, &mut out)?;
        } else {
            let comp = read_vec(&self.file, pos, len as usize)?;
            let mut r = DeflateDecoder::new(&comp[..]);
            let mut n = 0;
            while n < out.len() {
                match r.read(&mut out[n..])? {
                    0 => break,
                    k => n += k,
                }
            }
        }
        self.cache = Some((i, out));
        Ok(())
    }
}

impl Source for PbpDisc {
    fn len(&self) -> u64 {
        self.index.len() as u64 * BLOCK
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let off = offset + done as u64;
            let i = (off / BLOCK) as usize;
            if i >= self.index.len() {
                buf[done..].fill(0);
                break;
            }
            let within = (off % BLOCK) as usize;
            let n = (BLOCK as usize - within).min(buf.len() - done);
            self.load(i)?;
            buf[done..done + n].copy_from_slice(&self.cache.as_ref().unwrap().1[within..within + n]);
            done += n;
        }
        Ok(())
    }
}
