//! A `Source` is a random-access, read-only view of a virtual disk's (or disc's)
//! logical contents, regardless of how the bytes are stored in the image file.

use std::fs::File;
use std::io;
use std::os::windows::fs::FileExt;
use std::path::Path;

pub trait Source {
    /// Logical size in bytes.
    fn len(&self) -> u64;
    /// Fill `buf` with the logical bytes at `offset`. Bytes past the end read as zero.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    /// True if `len` bytes at `offset` are known to be zero without reading them
    /// (unallocated space in a sparse VM disk). `false` means "unknown", not "non-zero".
    fn is_zero(&mut self, _offset: u64, _len: u64) -> io::Result<bool> {
        Ok(false)
    }
}

impl Source for Box<dyn Source> {
    fn len(&self) -> u64 {
        (**self).len()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(offset, buf)
    }
    fn is_zero(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        (**self).is_zero(offset, len)
    }
}

/// Positioned read that zero-fills anything past EOF.
pub fn pread(file: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => {
                buf.fill(0);
                return Ok(());
            }
            Ok(n) => {
                offset += n as u64;
                buf = &mut buf[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.raw_os_error() == Some(38) => {
                // ERROR_HANDLE_EOF
                buf.fill(0);
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Largest single table or block read from an image. Sizes come from file headers, so
/// a corrupt or hostile image must not be able to make us allocate unbounded memory.
const MAX_STRUCT: usize = 1 << 30;

pub fn corrupt(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("corrupt image: {}", msg.into()))
}

pub fn read_vec(file: &File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    if len > MAX_STRUCT {
        return Err(corrupt(format!("a {len}-byte structure is implausibly large")));
    }
    // Large tables must actually be in the file (small reads may run past EOF on purpose).
    if len > (16 << 20) && offset.saturating_add(len as u64) > file.metadata()?.len() {
        return Err(corrupt("a table extends past the end of the file"));
    }
    let mut v = vec![0u8; len];
    pread(file, offset, &mut v)?;
    Ok(v)
}

pub fn open_ro(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
    std::fs::OpenOptions::new().read(true).share_mode(7).open(path)
}

/// A byte range of a plain file.
pub struct FileSlice {
    file: File,
    start: u64,
    len: u64,
}

impl FileSlice {
    pub fn new(file: File, start: u64, len: u64) -> Self {
        Self { file, start, len }
    }
    pub fn whole(path: &Path) -> io::Result<Self> {
        let file = open_ro(path)?;
        let len = file.metadata()?.len();
        Ok(Self::new(file, 0, len))
    }
}

impl Source for FileSlice {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if offset >= self.len {
            buf.fill(0);
            return Ok(());
        }
        let avail = ((self.len - offset) as usize).min(buf.len());
        let (head, tail) = buf.split_at_mut(avail);
        pread(&self.file, self.start + offset, head)?;
        tail.fill(0);
        Ok(())
    }
}

/// One piece of a concatenated source.
pub enum Segment {
    Bytes(Vec<u8>),
    Zero(u64),
    Src(Box<dyn Source>),
}

impl Segment {
    fn len(&self) -> u64 {
        match self {
            Segment::Bytes(b) => b.len() as u64,
            Segment::Zero(n) => *n,
            Segment::Src(s) => s.len(),
        }
    }
}

/// Several sources laid end to end (VMDK extents, synthesized MBR + volume, ...).
pub struct Concat {
    segs: Vec<(u64, Segment)>, // (start offset, segment)
    len: u64,
}

impl Concat {
    pub fn new(segments: Vec<Segment>) -> Self {
        let mut segs = Vec::new();
        let mut pos = 0;
        for s in segments {
            let l = s.len();
            if l > 0 {
                segs.push((pos, s));
                pos += l;
            }
        }
        Self { segs, len: pos }
    }
}

impl Source for Concat {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&mut self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            if offset >= self.len {
                buf.fill(0);
                return Ok(());
            }
            let idx = match self.segs.binary_search_by(|(s, _)| s.cmp(&offset)) {
                Ok(i) => i,
                Err(i) => i - 1,
            };
            let (start, seg) = &mut self.segs[idx];
            let rel = offset - *start;
            let n = ((seg.len() - rel) as usize).min(buf.len());
            let (head, tail) = buf.split_at_mut(n);
            match seg {
                Segment::Bytes(b) => head.copy_from_slice(&b[rel as usize..rel as usize + n]),
                Segment::Zero(_) => head.fill(0),
                Segment::Src(s) => s.read_at(rel, head)?,
            }
            offset += n as u64;
            buf = tail;
        }
        Ok(())
    }
    fn is_zero(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        let end = offset + len;
        for (start, seg) in self.segs.iter_mut() {
            let s_end = *start + seg.len();
            if s_end <= offset || *start >= end {
                continue;
            }
            let (a, b) = (offset.max(*start) - *start, end.min(s_end) - *start);
            let zero = match seg {
                Segment::Zero(_) => true,
                Segment::Bytes(v) => v[a as usize..b as usize].iter().all(|&x| x == 0),
                Segment::Src(s) => s.is_zero(a, b - a)?,
            };
            if !zero {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

pub fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
pub fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
pub fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
pub fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
pub fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}
