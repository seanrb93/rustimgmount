//! The complete track list of a CD image: every data *and* audio track as stored, so a
//! disc can be converted to another whole-disc format (CHD, BIN/CUE, GDI) without losing
//! its music or its raw sectors. (Mounting only needs the file system; see `cd.rs`.)

use super::cd::{self, shared, shared_file, Shared};
use crate::source::{be32, be64, le16, le32, le64, open_ro, read_vec, Source};
use anyhow::{bail, Context, Result};
use std::path::Path;

/// Where a track's subchannel data (96 bytes per sector) is stored.
#[derive(Clone)]
pub struct SubSrc {
    pub src: Shared,
    pub base: u64,
    pub stride: u32,
    /// As on the disc (one bit of each channel P..W per byte); otherwise channel by
    /// channel, 12 bytes each (CloneCD .sub, CHD "RW").
    pub interleaved: bool,
}

/// Convert 96-byte subchannel blocks between the two orders.
pub fn reorder_sub(block: &[u8], to_interleaved: bool) -> [u8; 96] {
    let mut out = [0u8; 96];
    for ch in 0..8 {
        for i in 0..96 {
            let bit = if to_interleaved { block[ch * 12 + i / 8] >> (7 - i % 8) & 1 } else { block[i] >> (7 - ch) & 1 };
            if bit != 0 {
                if to_interleaved {
                    out[i] |= 1 << (7 - ch);
                } else {
                    out[ch * 12 + i / 8] |= 1 << (7 - i % 8);
                }
            }
        }
    }
    out
}

pub struct RawTrack {
    pub number: u32,
    pub audio: bool,
    pub src: Shared,
    /// Byte offset of the track's first stored sector in `src`.
    pub base: u64,
    /// Bytes from one stored sector to the next.
    pub stride: u32,
    /// Meaningful bytes per sector: 2352 (raw or audio), 2336 or 2048.
    pub size: u32,
    /// CD-ROM mode of a data track (1 or 2); 0 for audio.
    pub mode: u8,
    /// Stored sectors, including `pregap`.
    pub frames: u64,
    /// Stored sectors before the track's INDEX 01.
    pub pregap: u64,
    /// Silent sectors before the track that are not stored (a cue sheet's PREGAP).
    pub gap: u64,
    /// GD-ROM: empty sectors between this track's data and the next track.
    pub pad: u64,
    /// Audio samples are stored big-endian.
    pub swap: bool,
    /// Address of the first stored sector on the disc.
    pub lba: u64,
    /// Subchannel data, when the image has it.
    pub sub: Option<SubSrc>,
}

impl RawTrack {
    /// Read `n` sectors from stored sector `frame`: `n * size` bytes, audio little-endian.
    pub fn read(&self, frame: u64, n: usize, out: &mut Vec<u8>) -> std::io::Result<()> {
        let (stride, size) = (self.stride as usize, self.size as usize);
        out.resize(n * stride, 0);
        self.src.borrow_mut().read_at(self.base + frame * stride as u64, out)?;
        if stride != size {
            for k in 1..n {
                out.copy_within(k * stride..k * stride + size, k * size);
            }
            out.truncate(n * size);
        }
        if self.audio && self.swap {
            for pair in out.chunks_exact_mut(2) {
                pair.swap(0, 1);
            }
        }
        Ok(())
    }

    /// Read `n` sectors' subchannel data (96 bytes each) in the wanted order.
    /// Returns false if the image has none for this track.
    pub fn read_sub(&self, frame: u64, n: usize, interleaved: bool, out: &mut Vec<u8>) -> std::io::Result<bool> {
        let Some(s) = &self.sub else { return Ok(false) };
        let stride = s.stride as usize;
        let mut raw = vec![0u8; n * stride];
        s.src.borrow_mut().read_at(s.base + frame * stride as u64, &mut raw)?;
        out.clear();
        for k in 0..n {
            let block = &raw[k * stride..k * stride + 96];
            if s.interleaved == interleaved {
                out.extend_from_slice(block);
            } else {
                out.extend_from_slice(&reorder_sub(block, interleaved));
            }
        }
        Ok(true)
    }

    /// Track type as CHD metadata spells it.
    pub fn chd_type(&self) -> &'static str {
        match (self.audio, self.size, self.mode) {
            (true, ..) => "AUDIO",
            (_, 2048, 2) => "MODE2_FORM1",
            (_, 2048, _) => "MODE1",
            (_, 2336, _) => "MODE2",
            (_, _, 2) => "MODE2_RAW",
            _ => "MODE1_RAW",
        }
    }

    /// Track type as a cue sheet spells it.
    pub fn cue_type(&self) -> String {
        if self.audio {
            "AUDIO".into()
        } else {
            format!("MODE{}/{}", if self.size == 2336 { 2 } else { self.mode.max(1) }, self.size)
        }
    }
}

pub struct Layout {
    pub tracks: Vec<RawTrack>,
    /// A Dreamcast GD-ROM (tracks sit at fixed addresses; described by .gdi, not .cue).
    pub gd: bool,
}

/// Mode of a raw data sector, from its header.
fn sniff_mode(src: &Shared, base: u64, size: u32) -> u8 {
    if size == 2336 {
        return 2;
    }
    let mut h = [0u8; 16];
    match src.borrow_mut().read_at(base, &mut h) {
        Ok(()) if size >= 2352 && h[1..11] == [0xFF; 10] && h[15] == 2 => 2,
        _ => 1,
    }
}

fn data_track(number: u32, src: Shared, base: u64, stride: u32, frames: u64) -> RawTrack {
    let size = stride.min(2352);
    let mode = sniff_mode(&src, base, size);
    let sub = sub_after(&src, base, stride);
    RawTrack { number, audio: false, src, base, stride, size, mode, frames, pregap: 0, gap: 0, pad: 0, swap: false, lba: 0, sub }
}

/// 2448-byte sectors carry their subchannel data after the 2352 bytes of the sector.
fn sub_after(src: &Shared, base: u64, stride: u32) -> Option<SubSrc> {
    (stride == 2448).then(|| SubSrc { src: src.clone(), base: base + 2352, stride, interleaved: true })
}

fn audio_track(number: u32, src: Shared, base: u64, stride: u32, frames: u64) -> RawTrack {
    let sub = sub_after(&src, base, stride);
    RawTrack { number, audio: true, src, base, stride, size: 2352, mode: 0, frames, pregap: 0, gap: 0, pad: 0, swap: false, lba: 0, sub }
}

/// The track list of a container that has one (CUE, GDI, CHD, MDS/MDF, NRG).
/// `None` for everything else, and for sheets this module can't describe exactly.
pub fn parse(path: &Path) -> Result<Option<Layout>> {
    let ext = super::ext(path);
    let head = read_vec(&open_ro(path)?, 0, 16)?;
    let mut l = match ext.as_str() {
        "cue" => from_cue(path)?,
        "gdi" => from_gdi(path)?,
        "mds" => from_mds(path)?,
        "mdf" => match ["mds", "MDS"].iter().map(|e| path.with_extension(e)).find(|p| p.exists()) {
            Some(m) => from_mds(&m)?,
            None => return Ok(None),
        },
        "ccd" | "img" | "sub" => match ["ccd", "CCD"].iter().map(|e| path.with_extension(e)).find(|p| p.exists()) {
            Some(c) => from_ccd(&c)?,
            None => return Ok(None),
        },
        _ if &head[0..8] == super::chd::MAGIC => return super::chd::layout(path),
        "nrg" => from_nrg(path)?,
        _ => return Ok(None),
    };
    l.tracks.retain(|t| t.frames > 0);
    if l.tracks.is_empty() || l.tracks.iter().any(|t| !matches!(t.size, 2048 | 2336 | 2352)) {
        return Ok(None);
    }
    if !l.gd {
        let mut lba = 0;
        for t in &mut l.tracks {
            t.lba = lba + t.gap;
            lba = t.lba + t.frames;
        }
    }
    Ok(Some(l))
}

/// A disc without a track list: one track holding the data.
pub fn single(disc: cd::Disc) -> Layout {
    let track = match disc.single_raw() {
        Some((src, base, ss, sectors)) => data_track(1, src, base, ss, sectors),
        None => {
            let sectors = disc.len() / 2048;
            data_track(1, shared(disc), 0, 2048, sectors)
        }
    };
    Layout { tracks: vec![track], gd: false }
}

/// A plain 2048-byte/sector image as a one-track disc.
pub fn cooked(src: impl Source + 'static) -> Layout {
    let sectors = src.len() / 2048;
    Layout { tracks: vec![data_track(1, shared(src), 0, 2048, sectors)], gd: false }
}

fn from_cue(path: &Path) -> Result<Layout> {
    let (tracks, pos, _) = cd::parse_cue(path)?;
    let mut out = Vec::new();
    for (i, t) in tracks.iter().enumerate() {
        if matches!(super::ext(&t.file).as_str(), "wav" | "flac" | "mp3" | "ogg" | "ape" | "wv") {
            bail!("this cue sheet uses {} audio files; only raw (BINARY) tracks can be converted", super::ext(&t.file).to_uppercase());
        }
        let mode = t.mode.to_ascii_uppercase();
        let ss = cd::cue_mode(&t.mode).0;
        let end = match tracks.get(i + 1) {
            Some(n) if n.file == t.file => pos[i + 1],
            _ => std::fs::metadata(&t.file)?.len(),
        };
        let frames = end.saturating_sub(pos[i]) / ss as u64;
        let src = shared_file(&t.file)?;
        let mut r = if mode == "AUDIO" { audio_track(i as u32 + 1, src, pos[i], ss, frames) } else { data_track(i as u32 + 1, src, pos[i], ss, frames) };
        if mode.starts_with("MODE2") || mode.starts_with("CDI") {
            r.mode = 2;
        }
        r.pregap = match (t.index0, t.index1) {
            (Some(a), Some(b)) if b > a => (b - a).min(frames),
            _ => 0,
        };
        r.gap = t.pregap;
        out.push(r);
    }
    Ok(Layout { tracks: out, gd: false })
}

fn from_gdi(path: &Path) -> Result<Layout> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out: Vec<RawTrack> = Vec::new();
    for line in text.lines().skip(1) {
        // num lba type sector_size "file name" offset
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 5 {
            continue;
        }
        let rest = line.trim().splitn(5, char::is_whitespace).nth(4).unwrap_or("").trim();
        let name = match rest.strip_prefix('"') {
            Some(r) => r.split('"').next().unwrap_or("").to_string(),
            None => f[4].to_string(),
        };
        let (number, lba, ss): (u32, u64, u32) = (f[0].parse()?, f[1].parse()?, f[3].parse()?);
        let file = cd::resolve_track_file(path, &name)?.with_context(|| format!("GDI references missing file '{name}'"))?;
        let frames = std::fs::metadata(&file)?.len() / ss.max(1) as u64;
        let src = shared_file(&file)?;
        let mut t = if f[2] == "0" { audio_track(number, src, 0, ss, frames) } else { data_track(number, src, 0, ss, frames) };
        t.lba = lba;
        if let Some(p) = out.last_mut() {
            p.pad = lba.saturating_sub(p.lba + p.frames);
        }
        out.push(t);
    }
    Ok(Layout { tracks: out, gd: true })
}

/// CloneCD: a .ccd sheet, the sectors in .img (2352 bytes each) and, optionally, the
/// subchannel data in .sub (96 bytes per sector, channel by channel).
fn from_ccd(ccd: &Path) -> Result<Layout> {
    let none = Layout { tracks: Vec::new(), gd: false };
    let text = String::from_utf8_lossy(&std::fs::read(ccd)?).into_owned();
    let Some(img) = ["img", "IMG"].iter().map(|e| ccd.with_extension(e)).find(|p| p.exists()) else { return Ok(none) };
    let total = std::fs::metadata(&img)?.len() / 2352;
    // (track number, mode, INDEX 0, INDEX 1)
    let mut tracks: Vec<(u32, u8, Option<u64>, Option<u64>)> = Vec::new();
    let mut in_track = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(h) = line.strip_prefix('[') {
            in_track = false;
            if let Some(n) = h.trim_end_matches(']').strip_prefix("TRACK ").and_then(|n| n.trim().parse().ok()) {
                tracks.push((n, 1, None, None));
                in_track = true;
            }
            continue;
        }
        let (Some((k, v)), Some(t), true) = (line.split_once('='), tracks.last_mut(), in_track) else { continue };
        let v: Option<u64> = v.trim().parse().ok();
        match k.trim().to_ascii_uppercase().as_str() {
            "MODE" => t.1 = v.unwrap_or(1) as u8,
            "INDEX 0" => t.2 = v,
            "INDEX 1" => t.3 = v,
            _ => {}
        }
    }
    if tracks.is_empty() {
        return Ok(none);
    }
    let src = shared_file(&img)?;
    let sub = ["sub", "SUB"]
        .iter()
        .map(|e| ccd.with_extension(e))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.len() >= total * 96))
        .map(|p| shared_file(&p))
        .transpose()?;
    let start = |t: &(u32, u8, Option<u64>, Option<u64>)| t.2.or(t.3).unwrap_or(0);
    let mut out = Vec::new();
    for (i, t) in tracks.iter().enumerate() {
        let first = start(t);
        let end = tracks.get(i + 1).map(start).unwrap_or(total).min(total);
        let frames = end.saturating_sub(first);
        let mut r = if t.1 == 0 { audio_track(t.0, src.clone(), first * 2352, 2352, frames) } else { data_track(t.0, src.clone(), first * 2352, 2352, frames) };
        r.pregap = t.3.unwrap_or(first).saturating_sub(first).min(frames);
        r.sub = sub.as_ref().map(|s| SubSrc { src: s.clone(), base: first * 96, stride: 96, interleaved: false });
        out.push(r);
    }
    Ok(Layout { tracks: out, gd: false })
}

/// Alcohol 120% MDS (version 1): track blocks point into the .mdf.
fn from_mds(mds: &Path) -> Result<Layout> {
    let m = std::fs::read(mds)?;
    if m.len() < 0x58 || &m[0..16] != b"MEDIA DESCRIPTOR" || m[16] != 1 {
        return Ok(Layout { tracks: Vec::new(), gd: false });
    }
    let mdf = ["mdf", "MDF"].iter().map(|e| mds.with_extension(e)).find(|p| p.exists()).context("the .mdf file is missing")?;
    let src = shared(super::pieces::concat_files(&super::pieces::mdf_parts(&mdf))?);
    let total = src.borrow().len();
    let sess_off = le32(&m, 0x50) as usize;
    let mut entries = Vec::new();
    for s in 0..le16(&m, 0x14) as usize {
        let so = sess_off + s * 24;
        let (Some(n), Some(t)) = (m.get(so + 0x0A), m.get(so + 0x14..so + 0x18)) else { break };
        let toff = u32::from_le_bytes(t.try_into().unwrap()) as usize;
        for b in 0..*n as usize {
            let Some(t) = m.get(toff + b * 80..toff + b * 80 + 80) else { break };
            if (1..=99).contains(&t[4]) {
                entries.push((t[4] as u32, t[0] & 0x0F, le16(t, 0x10) as u32, le64(t, 0x28)));
            }
        }
    }
    entries.sort_by_key(|e| e.0);
    let mut out = Vec::new();
    for (i, &(point, mode, ss, off)) in entries.iter().enumerate() {
        if ss == 0 || off >= total {
            continue;
        }
        let end = entries.get(i + 1).map(|e| e.3).filter(|&e| e > off).unwrap_or(total);
        let frames = (end - off) / ss as u64;
        out.push(if mode == 9 { audio_track(point, src.clone(), off, ss, frames) } else { data_track(point, src.clone(), off, ss, frames) });
    }
    Ok(Layout { tracks: out, gd: false })
}

/// Nero NRG disc-at-once images (DAOI/DAOX chunks).
fn from_nrg(path: &Path) -> Result<Layout> {
    let none = Layout { tracks: Vec::new(), gd: false };
    let file = open_ro(path)?;
    let len = file.metadata()?.len();
    let tail = read_vec(&file, len.saturating_sub(12), 12)?;
    let chunks_at = if &tail[0..4] == b"NER5" {
        be64(&tail, 4)
    } else if &tail[4..8] == b"NERO" {
        be32(&tail, 8) as u64
    } else {
        return Ok(none);
    };
    let src = shared_file(path)?;
    let mut out = Vec::new();
    let mut p = chunks_at;
    while p + 8 <= len {
        let h = read_vec(&file, p, 8)?;
        let size = be32(&h, 4) as u64;
        if &h[0..4] == b"END!" || size > len {
            break;
        }
        if &h[0..4] == b"DAOX" || &h[0..4] == b"DAOI" {
            let wide = &h[0..4] == b"DAOX";
            let body = read_vec(&file, p + 8, size as usize)?;
            let entry = if wide { 42 } else { 30 };
            let mut o = 22;
            while o + entry <= body.len() {
                let ss = u16::from_be_bytes([body[o + 12], body[o + 13]]) as u32;
                let mode = body[o + 14];
                let (start, end) = if wide {
                    (be64(&body, o + 26), be64(&body, o + 34))
                } else {
                    (be32(&body, o + 22) as u64, be32(&body, o + 26) as u64)
                };
                let (n, frames) = (out.len() as u32 + 1, end.min(chunks_at).saturating_sub(start) / ss.max(1) as u64);
                out.push(if mode == 7 || mode == 0x10 { audio_track(n, src.clone(), start, ss, frames) } else { data_track(n, src.clone(), start, ss, frames) });
                o += entry;
            }
        }
        p += 8 + size;
    }
    Ok(Layout { tracks: out, gd: false })
}
