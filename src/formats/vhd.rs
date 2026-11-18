//! Readers for VHD and VHDX. Windows mounts these itself, so they are only read here
//! when one is converted to another format.

use crate::source::{be32, be64, le32, le64, open_ro, pread, read_vec, FileSlice, Source};
use anyhow::{bail, Result};
use std::fs::File;
use std::io;
use std::path::Path;

pub fn open(path: &Path) -> Result<Box<dyn Source>> {
    let file = open_ro(path)?;
    if read_vec(&file, 0, 8)? == b"vhdxfile" {
        return Ok(Box::new(Vhdx::open(file)?));
    }
    let len = file.metadata()?.len();
    if len < 512 {
        bail!("not a VHD image");
    }
    let mut footer = read_vec(&file, len - 512, 512)?;
    if &footer[0..8] != b"conectix" {
        footer = read_vec(&file, 0, 512)?; // dynamic disks keep a copy at the start
    }
    if &footer[0..8] != b"conectix" {
        bail!("not a VHD image");
    }
    let size = be64(&footer, 48);
    match be32(&footer, 60) {
        2 => Ok(Box::new(FileSlice::new(file, 0, size.min(len - 512)))),
        3 => {
            let dh_off = be64(&footer, 16);
            let dh = read_vec(&file, dh_off, 1024)?;
            if &dh[0..8] != b"cxsparse" {
                bail!("corrupt VHD (no dynamic disk header)");
            }
            let (bat_off, entries, block) = (be64(&dh, 16), be32(&dh, 28) as usize, be32(&dh, 32) as u64);
            if block < 512 || !block.is_power_of_two() || (entries as u64) < size.div_ceil(block) {
                bail!("corrupt VHD header");
            }
            let raw = read_vec(&file, bat_off, entries * 4)?;
            let bat = raw.chunks_exact(4).map(|c| u32::from_be_bytes(c.try_into().unwrap())).collect();
            // Each block is preceded by a bitmap of one bit per sector, padded to a sector.
            let bitmap = (block / 512).div_ceil(8).div_ceil(512) * 512;
            Ok(Box::new(Table { file, size, block, table: bat, bitmap }))
        }
        4 => bail!("this is a differencing VHD (it depends on a parent disk); merge it in Disk Management or Hyper-V first"),
        t => bail!("unsupported VHD type {t}"),
    }
}

/// A disk stored as fixed-size blocks located through a table (dynamic VHD).
struct Table {
    file: File,
    size: u64,
    block: u64,
    /// Sector number of each block's bitmap, or 0xFFFFFFFF if the block is empty.
    table: Vec<u32>,
    bitmap: u64,
}

impl Source for Table {
    fn len(&self) -> u64 {
        self.size
    }
    fn is_zero(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let (a, b) = (offset / self.block, (offset + len.max(1) - 1) / self.block);
        Ok((a..=b).all(|i| self.table.get(i as usize).is_none_or(|&e| e == u32::MAX)))
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let off = offset + done as u64;
            let within = off % self.block;
            let n = ((self.block - within) as usize).min(buf.len() - done);
            match self.table.get((off / self.block) as usize) {
                Some(&e) if e != u32::MAX && off < self.size => pread(&self.file, e as u64 * 512 + self.bitmap + within, &mut buf[done..done + n])?,
                _ => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }
}

struct Vhdx {
    file: File,
    size: u64,
    block: u64,
    /// One payload-block entry follows every `chunk` entries' worth of blocks.
    chunk: u64,
    bat: Vec<u64>,
}

impl Vhdx {
    fn open(file: File) -> Result<Self> {
        // The current header is the valid one with the higher sequence number.
        let mut log_guid = None;
        let mut best = 0u64;
        for off in [64u64 << 10, 128 << 10] {
            let h = read_vec(&file, off, 4096)?;
            if &h[0..4] == b"head" && (log_guid.is_none() || le64(&h, 8) >= best) {
                best = le64(&h, 8);
                log_guid = Some(h[48..64].to_vec());
            }
        }
        match log_guid {
            None => bail!("corrupt VHDX (no valid header)"),
            Some(g) if g.iter().any(|&b| b != 0) => {
                bail!("this VHDX wasn't closed cleanly (it has pending changes in its log); mount it once in Windows, then convert it")
            }
            _ => {}
        }
        let rt = read_vec(&file, 192 << 10, 64 << 10)?;
        if &rt[0..4] != b"regi" {
            bail!("corrupt VHDX (no region table)");
        }
        let (bat_id, meta_id) = (crate::convert::guid("2DC27766-F623-4200-9D64-115E9BFD4A08"), crate::convert::guid("8B7CA206-4790-4B9A-B8FE-575F050F886E"));
        let (mut bat_at, mut meta_at) = (None, None);
        for i in 0..(le32(&rt, 8) as usize).min(2047) {
            let e = &rt[16 + i * 32..48 + i * 32];
            let place = (le64(e, 16), le32(e, 24) as u64);
            if e[0..16] == bat_id {
                bat_at = Some(place);
            } else if e[0..16] == meta_id {
                meta_at = Some(place);
            }
        }
        let (Some((bat_off, bat_len)), Some((meta_off, meta_len))) = (bat_at, meta_at) else { bail!("corrupt VHDX (regions missing)") };
        let md = read_vec(&file, meta_off, meta_len.min(4 << 20) as usize)?;
        if &md[0..8] != b"metadata" {
            bail!("corrupt VHDX (no metadata table)");
        }
        let item = |id: &str, len: usize| -> Option<&[u8]> {
            let want = crate::convert::guid(id);
            (0..u16::from_le_bytes([md[10], md[11]]) as usize).find_map(|i| {
                let e = md.get(32 + i * 32..64 + i * 32)?;
                (e[0..16] == want).then(|| md.get(le32(e, 16) as usize..le32(e, 16) as usize + len)).flatten()
            })
        };
        let params = item("CAA16737-FA36-4D43-B3B6-33F0AA44E76B", 8);
        let size = item("2FA54224-CD1B-4876-B211-5DBED83BF4B8", 8);
        let sector = item("8141BF1D-A96F-4709-BA47-F233A8FAAB5F", 4);
        let (Some(params), Some(size), Some(sector)) = (params, size, sector) else { bail!("corrupt VHDX (metadata missing)") };
        if le32(params, 4) & 2 != 0 {
            bail!("this is a differencing VHDX (it depends on a parent disk); merge it in Hyper-V first");
        }
        let (block, size, sector) = (le32(params, 0) as u64, le64(size, 0), le32(sector, 0) as u64);
        if !(1 << 20..=256 << 20).contains(&block) || !block.is_power_of_two() || !matches!(sector, 512 | 4096) {
            bail!("corrupt VHDX metadata");
        }
        if sector != 512 {
            bail!("VHDX images with 4096-byte sectors can't be converted");
        }
        let chunk = (1u64 << 23) * sector / block;
        let blocks = size.div_ceil(block);
        let entries = blocks + blocks.saturating_sub(1) / chunk;
        if entries * 8 > bat_len {
            bail!("corrupt VHDX (block table too small)");
        }
        let raw = read_vec(&file, bat_off, (entries * 8) as usize)?;
        let bat = raw.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
        Ok(Self { file, size, block, chunk, bat })
    }

    /// File offset of block `b`, if it holds data.
    fn place(&self, b: u64) -> Option<u64> {
        let e = *self.bat.get((b + b / self.chunk) as usize)?;
        (e & 7 == 6).then_some((e >> 20) << 20)
    }
}

impl Source for Vhdx {
    fn len(&self) -> u64 {
        self.size
    }
    fn is_zero(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let (a, b) = (offset / self.block, (offset + len.max(1) - 1) / self.block);
        Ok((a..=b).all(|i| self.place(i).is_none()))
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let off = offset + done as u64;
            let within = off % self.block;
            let n = ((self.block - within) as usize).min(buf.len() - done);
            match self.place(off / self.block) {
                Some(at) if off < self.size => pread(&self.file, at + within, &mut buf[done..done + n])?,
                _ => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }
}
