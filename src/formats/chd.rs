//! MAME CHD (Compressed Hunks of Data), versions 1-5, all codecs supported by the
//! `chd` crate (zlib, LZMA, Huffman, FLAC, zstd and their CD variants).
//! CD-ROM and GD-ROM CHDs become discs; hard disk / raw CHDs become disks.

use super::cd::{shared, Disc, TrackCand};
use crate::source::Source;
use anyhow::{anyhow, bail, Context, Result};
use chd::Chd;
use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;

pub const MAGIC: &[u8; 8] = b"MComprHD";
const CD_FRAME: u64 = 2448;

type ChdFile = Chd<BufReader<File>>;

/// SHA-1 of a CHD's data, from its header (v3+).
fn chd_sha1(path: &Path) -> Option<[u8; 20]> {
    let mut f = BufReader::new(File::open(path).ok()?);
    chd::header::Header::try_read_header(&mut f).ok()?.sha1()
}

/// Open a CHD; if it's a delta ("child") CHD, find its parent next to it by SHA-1.
fn open_chd(path: &Path, depth: u32) -> Result<ChdFile> {
    let f = BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
    let mut probe = BufReader::new(File::open(path)?);
    let header = chd::header::Header::try_read_header(&mut probe).map_err(|e| anyhow!("cannot read CHD: {e}"))?;
    let parent = match header.parent_sha1().filter(|_| header.has_parent()) {
        None => None,
        Some(_) if depth > 8 => bail!("CHD parent chain too long"),
        Some(want) => {
            let dir = path.parent().unwrap_or(Path::new("."));
            let found = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p != path && super::ext(p) == "chd")
                .find(|p| chd_sha1(p) == Some(want))
                .ok_or_else(|| anyhow!("this CHD depends on a parent CHD that isn't in the same folder; put the parent next to it"))?;
            Some(Box::new(open_chd(&found, depth + 1)?))
        }
    };
    Chd::open(f, parent).map_err(|e| anyhow!("cannot read CHD: {e}"))
}

pub struct ChdSource {
    chd: ChdFile,
    path: std::path::PathBuf,
    hunk_size: u64,
    hunk_count: u32,
    len: u64,
    cmp: Vec<u8>,
    hunk: Vec<u8>,
    cached: Option<u32>,
    /// Hunks decoded ahead of time by the parallel prefetcher.
    batch: std::collections::HashMap<u32, Vec<u8>>,
    last: Option<u32>,
}

fn hunk_err(n: u32, e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("CHD hunk {n}: {e}"))
}

impl ChdSource {
    pub fn open(path: &Path) -> Result<Self> {
        let chd = open_chd(path, 0)?;
        let hunk_size = chd.header().hunk_size() as u64;
        let hunk_count = chd.header().hunk_count();
        let len = chd.header().logical_bytes();
        let hunk = chd.get_hunksized_buffer();
        Ok(Self {
            chd,
            path: path.to_owned(),
            hunk_size,
            hunk_count,
            len,
            cmp: Vec::new(),
            hunk,
            cached: None,
            batch: Default::default(),
            last: None,
        })
    }

    /// Decode hunks `start..end` on all cores. Each worker opens its own reader (the
    /// decoder can't be shared between threads) and takes a contiguous slice.
    fn prefetch(&mut self, start: u32, end: u32) -> io::Result<()> {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(16) as u32;
        let per = (end - start).div_ceil(threads);
        let path = &self.path;
        let results: Vec<io::Result<Vec<(u32, Vec<u8>)>>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|t| {
                    let (a, b) = (start + t * per, (start + (t + 1) * per).min(end));
                    s.spawn(move || -> io::Result<Vec<(u32, Vec<u8>)>> {
                        if a >= b {
                            return Ok(Vec::new());
                        }
                        let mut chd = open_chd(path, 0).map_err(|e| io::Error::other(e.to_string()))?;
                        let (mut buf, mut cmp) = (chd.get_hunksized_buffer(), Vec::new());
                        let mut out = Vec::with_capacity((b - a) as usize);
                        for i in a..b {
                            chd.hunk(i).map_err(|e| hunk_err(i, e))?.read_hunk_in(&mut cmp, &mut buf).map_err(|e| hunk_err(i, e))?;
                            out.push((i, buf.clone()));
                        }
                        Ok(out)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(io::Error::other("CHD decoder thread failed")))).collect()
        });
        self.batch.clear();
        for r in results {
            self.batch.extend(r?);
        }
        Ok(())
    }

    fn load(&mut self, n: u32) -> io::Result<()> {
        if self.cached == Some(n) {
            return Ok(());
        }
        let sequential = self.last.is_some_and(|l| n == l + 1);
        self.last = Some(n);
        if !self.batch.contains_key(&n) && sequential && n + 1 < self.hunk_count {
            // Reading straight through (a conversion): decode the next stretch in parallel.
            let end = (n + 512).min(self.hunk_count);
            self.prefetch(n, end)?;
        }
        if let Some(h) = self.batch.remove(&n) {
            self.hunk = h;
        } else {
            let mut h = self.chd.hunk(n).map_err(|e| hunk_err(n, e))?;
            h.read_hunk_in(&mut self.cmp, &mut self.hunk).map_err(|e| hunk_err(n, e))?;
        }
        self.cached = Some(n);
        Ok(())
    }

    /// All metadata entries as (tag, bytes).
    fn metadata(&mut self) -> Vec<(u32, Vec<u8>)> {
        let mut out = Vec::new();
        let refs: Vec<_> = self.chd.metadata_refs().filter_map(|r| r.ok()).collect();
        for r in refs {
            if let Ok(m) = r.read(self.chd.inner()) {
                out.push((m.metatag, m.value));
            }
        }
        out
    }
}

impl Source for ChdSource {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let off = offset + done as u64;
            let within = (off % self.hunk_size) as usize;
            let n = (self.hunk_size as usize - within).min(buf.len() - done);
            if off >= self.len {
                buf[done..].fill(0);
                break;
            }
            self.load((off / self.hunk_size) as u32)?;
            buf[done..done + n].copy_from_slice(&self.hunk[within..within + n]);
            done += n;
        }
        Ok(())
    }
}

fn tag(t: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*t)
}

/// One track from CHT2/CHTR/CHGD/CHCD metadata.
#[derive(Debug, Default, Clone)]
struct ChdTrack {
    number: u32,
    kind: String,
    frames: u64,
    pregap: u64,
    pregap_stored: bool,
    postgap: u64,
    /// GD-ROM: empty frames at the end of `frames` (the gap up to the next track).
    pad: u64,
    /// Subchannel data kept in the frames: "RW" (channel by channel), "RW_RAW" (as on disc) or "NONE".
    subtype: String,
    /// Padding frames after the track in the CHD (explicit in CHCD, else to a multiple of 4).
    extra: Option<u64>,
}

fn parse_text_track(v: &[u8]) -> ChdTrack {
    let s = String::from_utf8_lossy(v);
    let mut t = ChdTrack::default();
    for field in s.trim_end_matches('\0').split_whitespace() {
        let Some((k, val)) = field.split_once(':') else { continue };
        match k {
            "TRACK" => t.number = val.parse().unwrap_or(0),
            "TYPE" => t.kind = val.to_string(),
            "FRAMES" => t.frames = val.parse().unwrap_or(0),
            "PREGAP" => t.pregap = val.parse().unwrap_or(0),
            "PGTYPE" => t.pregap_stored = val.starts_with('V'),
            "POSTGAP" => t.postgap = val.parse().unwrap_or(0),
            "PAD" => t.pad = val.parse().unwrap_or(0),
            "SUBTYPE" => t.subtype = val.to_string(),
            _ => {}
        }
    }
    t
}

/// Legacy binary CHCD metadata (CHD v3/v4).
fn parse_chcd(v: &[u8]) -> Vec<ChdTrack> {
    const KINDS: [&str; 8] = ["MODE1", "MODE1_RAW", "MODE2", "MODE2_FORM1", "MODE2_FORM2", "MODE2_FORM_MIX", "MODE2_RAW", "AUDIO"];
    let be = |o: usize| v.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).unwrap_or(0);
    let n = be(0) as usize;
    (0..n.min(99))
        .map(|i| {
            let o = 4 + i * 24;
            ChdTrack {
                number: i as u32 + 1,
                kind: KINDS.get(be(o) as usize).unwrap_or(&"MODE1").to_string(),
                frames: be(o + 16) as u64,
                extra: Some(be(o + 20) as u64),
                ..Default::default()
            }
        })
        .collect()
}

/// User-data offset within a 2448-byte CHD frame for each track type; None = audio.
fn frame_offset(kind: &str) -> Option<u32> {
    match kind {
        "AUDIO" => None,
        "MODE1" | "MODE2_FORM1" => Some(0),
        "MODE1_RAW" => Some(16),
        "MODE2_RAW" | "CDI/2352" => Some(24),
        "MODE2" | "MODE2_FORM_MIX" => Some(8),
        _ => Some(0),
    }
}

pub enum Opened {
    Cd(Disc),
    Disk(Box<dyn Source>),
}

/// CD/GD-ROM track list from the metadata (empty if this isn't a CD CHD).
fn cd_tracks(meta: &[(u32, Vec<u8>)]) -> Vec<ChdTrack> {
    let mut tracks = Vec::new();
    for (t, v) in meta {
        if *t == tag(b"CHT2") || *t == tag(b"CHTR") || *t == tag(b"CHGD") || *t == tag(b"CHGT") {
            tracks.push(parse_text_track(v));
        } else if *t == tag(b"CHCD") {
            tracks.extend(parse_chcd(v));
        }
    }
    tracks.sort_by_key(|t| t.number);
    tracks
}

/// Audio tracks of a CD CHD. Frames are 2448 bytes (sector + subcode) and CHD stores
/// CD audio big-endian, so samples need swapping.
pub fn audio(path: &Path) -> Result<Vec<super::audio::AudioTrack>> {
    let mut src = ChdSource::open(path)?;
    let tracks = cd_tracks(&src.metadata());
    let shared_src = shared(src);
    let mut out = Vec::new();
    let mut chd_frame = 0u64;
    for t in &tracks {
        let stored_pg = if t.pregap_stored { t.pregap } else { 0 };
        if t.kind == "AUDIO" {
            out.push(super::audio::AudioTrack {
                number: t.number,
                src: shared_src.clone(),
                base: (chd_frame + stored_pg) * CD_FRAME,
                stride: CD_FRAME as u32,
                frames: t.frames.saturating_sub(stored_pg),
                swap: true,
            });
        }
        chd_frame += t.frames + t.extra.unwrap_or((4 - t.frames % 4) % 4);
    }
    Ok(out)
}

/// Every track of a CD/GD-ROM CHD as stored (`None` for DVD and hard disk CHDs).
pub fn layout(path: &Path) -> Result<Option<super::layout::Layout>> {
    use super::layout::RawTrack;
    let mut src = ChdSource::open(path)?;
    let meta = src.metadata();
    let tracks = cd_tracks(&meta);
    if tracks.is_empty() {
        return Ok(None);
    }
    let gd = meta.iter().any(|(t, _)| *t == tag(b"CHGD") || *t == tag(b"CHGT"));
    let shared_src = shared(src);
    let mut out = Vec::new();
    let (mut chd_frame, mut lba) = (0u64, 0u64);
    for t in &tracks {
        let (size, mode) = match t.kind.as_str() {
            "AUDIO" => (2352, 0),
            "MODE1" => (2048, 1),
            "MODE1_RAW" => (2352, 1),
            "MODE2_FORM1" => (2048, 2),
            "MODE2" | "MODE2_FORM_MIX" => (2336, 2),
            "MODE2_RAW" | "CDI/2352" => (2352, 2),
            other => bail!("this CHD has a {other} track, which can't be converted"),
        };
        let pad = t.pad.min(t.frames);
        let gap = if t.pregap_stored { 0 } else { t.pregap };
        lba += gap;
        out.push(RawTrack {
            number: t.number,
            audio: mode == 0,
            src: shared_src.clone(),
            base: chd_frame * CD_FRAME,
            stride: CD_FRAME as u32,
            size,
            mode,
            frames: t.frames - pad,
            pregap: if t.pregap_stored { t.pregap.min(t.frames - pad) } else { 0 },
            gap,
            pad,
            swap: true,
            lba,
            sub: matches!(t.subtype.as_str(), "RW" | "RW_RAW").then(|| super::layout::SubSrc {
                src: shared_src.clone(),
                base: chd_frame * CD_FRAME + 2352,
                stride: CD_FRAME as u32,
                interleaved: t.subtype == "RW_RAW",
            }),
        });
        lba += t.frames + t.postgap;
        chd_frame += t.frames + t.extra.unwrap_or((4 - t.frames % 4) % 4);
    }
    Ok(Some(super::layout::Layout { tracks: out, gd }))
}

pub fn open(path: &Path) -> Result<Opened> {
    let mut src = ChdSource::open(path)?;
    let meta = src.metadata();
    let tracks = cd_tracks(&meta);
    let is_cd = !tracks.is_empty() || meta.iter().any(|(t, _)| *t == tag(b"CHCD"));
    let is_dvd = meta.iter().any(|(t, _)| *t == tag(b"DVD "));

    if !is_cd {
        let len = src.len();
        let shared_src = shared(src);
        // A DVD CHD, or a raw CHD that happens to hold an ISO: treat as a disc.
        let c = TrackCand::new(shared_src.clone(), 0, 2048, len / 2048).hint(Some(0)).label("data");
        if is_dvd || super::cd::Disc::assemble(vec![c], None).is_ok() {
            let c = TrackCand::new(shared_src, 0, 2048, len / 2048).hint(Some(0)).label("data");
            return Ok(Opened::Cd(Disc::assemble(vec![c], None)?));
        }
        // Hard disk: the logical bytes are the disk.
        return Ok(Opened::Disk(super::disk::raw_disk(Box::new(super::cd::SharedSrc(shared_src)), false)?));
    }

    if tracks.is_empty() {
        bail!("CHD has no track metadata");
    }
    let shared_src = shared(src);
    let mut cands = Vec::new();
    let (mut chd_frame, mut log_frame) = (0u64, 0u64);
    for t in &tracks {
        // Same bookkeeping as MAME's cdrom_file: pregap frames not stored in the CHD
        // still occupy logical addresses.
        if !t.pregap_stored {
            log_frame += t.pregap;
        }
        let stored_pg = if t.pregap_stored { t.pregap } else { 0 };
        if let Some(d) = frame_offset(&t.kind) {
            let index1 = log_frame + stored_pg;
            cands.push(
                TrackCand::new(shared_src.clone(), (chd_frame + stored_pg) * CD_FRAME, CD_FRAME as u32, t.frames.saturating_sub(stored_pg))
                    .hint(Some(d))
                    .lba(Some(index1 as i64))
                    .label(format!("track {} ({})", t.number, t.kind)),
            );
        }
        log_frame += t.frames + t.postgap;
        chd_frame += t.frames + t.extra.unwrap_or((4 - t.frames % 4) % 4);
    }
    Ok(Opened::Cd(Disc::assemble(cands, None)?))
}
