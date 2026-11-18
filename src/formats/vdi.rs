//! VirtualBox VDI (format version 1.1, dynamic and fixed).

use crate::source::{le32, le64, open_ro, pread, read_vec, Source};
use anyhow::{bail, Result};
use std::fs::File;
use std::io;
use std::path::Path;

pub const SIGNATURE: u32 = 0xBEDA107F;
const UNALLOCATED: u32 = 0xFFFF_FFFF;
const ZERO: u32 = 0xFFFF_FFFE;

pub struct Vdi {
    file: File,
    size: u64,
    block_size: u64,
    extra: u64,
    data_offset: u64,
    map: Vec<u32>,
}

pub fn is_vdi(head: &[u8]) -> bool {
    head.len() >= 0x48 && le32(head, 0x40) == SIGNATURE
}

impl Vdi {
    pub fn open(path: &Path) -> Result<Self> {
        let file = open_ro(path)?;
        let h = read_vec(&file, 0, 512)?;
        if !is_vdi(&h) {
            bail!("not a VDI image");
        }
        let version = le32(&h, 0x44);
        if version >> 16 != 1 {
            bail!("unsupported VDI version {}.{}", version >> 16, version & 0xffff);
        }
        let blocks_offset = le32(&h, 0x154) as u64;
        let data_offset = le32(&h, 0x158) as u64;
        let size = le64(&h, 0x170);
        let block_size = le32(&h, 0x178) as u64;
        let extra = le32(&h, 0x17C) as u64;
        let blocks = le32(&h, 0x180) as usize;
        if block_size == 0 || blocks == 0 {
            bail!("corrupt VDI header");
        }
        let raw = read_vec(&file, blocks_offset, blocks * 4)?;
        let map = raw.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        Ok(Self { file, size, block_size, extra, data_offset, map })
    }
}

impl Source for Vdi {
    fn len(&self) -> u64 {
        self.size
    }
    fn is_zero(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let end = (offset + len).min(self.size);
        let mut b = offset / self.block_size;
        while b * self.block_size < end {
            match self.map.get(b as usize) {
                Some(&e) if e != UNALLOCATED && e != ZERO => return Ok(false),
                _ => {}
            }
            b += 1;
        }
        Ok(true)
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let off = offset + done as u64;
            let blk = (off / self.block_size) as usize;
            let within = off % self.block_size;
            let n = ((self.block_size - within) as usize).min(buf.len() - done);
            let out = &mut buf[done..done + n];
            match self.map.get(blk) {
                Some(&e) if e != UNALLOCATED && e != ZERO && off < self.size => {
                    let pos = self.data_offset + e as u64 * (self.block_size + self.extra) + self.extra + within;
                    pread(&self.file, pos, out)?;
                }
                _ => out.fill(0),
            }
            done += n;
        }
        Ok(())
    }
}
