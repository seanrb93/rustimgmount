//! Images split across several files, and forensic containers:
//! raw split images (.001, .002, ...), Alcohol split MDF (.mdf, .md0/.md1, ...),
//! and EnCase Expert Witness (.E01 ... and .Ex01) via the `ewf` crate.

use crate::source::{FileSlice, Segment, Concat, Source};
use anyhow::{anyhow, Result};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Next numbered sibling: name.001 -> name.002, keeping the digit count.
fn numbered(path: &Path, n: u32, width: usize) -> PathBuf {
    path.with_extension(format!("{:0width$}", n, width = width))
}

/// Parts of a raw split image, if `path` is the first (`*.001`) of two or more.
pub fn split_raw_parts(path: &Path) -> Option<Vec<PathBuf>> {
    let ext = path.extension()?.to_str()?;
    if !ext.chars().all(|c| c.is_ascii_digit()) || ext.parse::<u32>().ok()? != 1 {
        return None;
    }
    let width = ext.len();
    let mut parts = vec![path.to_owned()];
    for n in 2..100_000 {
        let p = numbered(path, n, width);
        if !p.exists() {
            break;
        }
        parts.push(p);
    }
    (parts.len() > 1).then_some(parts)
}

/// Alcohol splits large images into name.mdf, name.md0/.md1, name.md2, ...
pub fn mdf_parts(mdf: &Path) -> Vec<PathBuf> {
    let mut parts = vec![mdf.to_owned()];
    for n in 0..100 {
        let p = mdf.with_extension(format!("md{n}"));
        if p.exists() {
            parts.push(p);
        } else if n > 0 {
            break;
        }
    }
    parts
}

/// The given files laid end to end.
pub fn concat_files(parts: &[PathBuf]) -> Result<Box<dyn Source>> {
    if parts.len() == 1 {
        return Ok(Box::new(FileSlice::whole(&parts[0])?));
    }
    let segs = parts
        .iter()
        .map(|p| Ok(Segment::Src(Box::new(FileSlice::whole(p).map_err(|e| anyhow!("{}: {e}", p.display()))?) as Box<dyn Source>)))
        .collect::<Result<Vec<_>>>()?;
    Ok(Box::new(Concat::new(segs)))
}

pub fn is_ewf(head: &[u8]) -> bool {
    head.starts_with(b"EVF\x09\x0D\x0A\xFF\x00") || head.starts_with(b"EVF2\x0D\x0A\x81\x00")
}

/// EnCase / FTK evidence image. Segment files (.E02, ...) are found automatically.
pub struct Ewf {
    r: Mutex<ewf::EwfReader>,
    len: u64,
}

impl Ewf {
    pub fn open(path: &Path) -> Result<Self> {
        let r = ewf::EwfReader::open(path).map_err(|e| anyhow!("cannot read EWF image: {e}"))?;
        let len = r.total_size();
        Ok(Self { r: Mutex::new(r), len })
    }
}

impl Source for Ewf {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let r = self.r.lock().unwrap();
        let mut done = 0;
        while done < buf.len() {
            let off = offset + done as u64;
            if off >= self.len {
                buf[done..].fill(0);
                break;
            }
            let n = r.read_at(&mut buf[done..], off).map_err(|e| io::Error::other(e.to_string()))?;
            if n == 0 {
                buf[done..].fill(0);
                break;
            }
            done += n;
        }
        Ok(())
    }
}
