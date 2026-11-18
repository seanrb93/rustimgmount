//! CD audio tracks. Windows' ISO mount can only show a disc's data, so mixed-mode discs
//! (many PlayStation, Saturn and Sega CD games, CD-Extra) lose their music when mounted.
//! `rustimgmount audio` saves those tracks as WAV files instead.
//!
//! Supported: CUE/BIN (any number of files), GDI, CHD, Alcohol MDS/MDF (v1) and Nero NRG,
//! also inside zip/7z/gz/... archives.

use super::cd::{shared, shared_file, Shared};
use crate::source::{be32, be64, le16, le32, le64, open_ro, read_vec};
use anyhow::{bail, Context, Result};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// Bytes of audio per CD frame (1/75 s of 44.1 kHz, 16-bit stereo).
const FRAME: usize = 2352;

pub struct AudioTrack {
    pub number: u32,
    pub src: Shared,
    /// Byte offset of the track's INDEX 01 frame in `src`.
    pub base: u64,
    /// Bytes per stored frame (2352, or 2448 with subchannel data).
    pub stride: u32,
    pub frames: u64,
    /// Samples are big-endian (CHD) and must be swapped for WAV.
    pub swap: bool,
}

impl AudioTrack {
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / 75.0
    }
}

/// Audio tracks of a disc image (empty if it has none, or the format has no track list).
pub fn tracks(path: &Path) -> Result<Vec<AudioTrack>> {
    let ext = super::ext(path);
    let head = read_vec(&open_ro(path)?, 0, 16)?;
    match ext.as_str() {
        "cue" => cue(path),
        "gdi" => gdi(path),
        "mds" => mds(path),
        _ if &head[0..8] == super::chd::MAGIC => super::chd::audio(path),
        _ if ext == "nrg" => nrg(path),
        "mdf" => match ["mds", "MDS"].iter().map(|e| path.with_extension(e)).find(|p| p.exists()) {
            Some(m) => mds(&m),
            None => Ok(Vec::new()),
        },
        _ => Ok(Vec::new()),
    }
}

fn cue(path: &Path) -> Result<Vec<AudioTrack>> {
    let (tracks, pos, _) = super::cd::parse_cue(path)?;
    let mut out = Vec::new();
    for (i, t) in tracks.iter().enumerate() {
        if !t.mode.eq_ignore_ascii_case("AUDIO") {
            continue;
        }
        let start = t.index0.or(t.index1).unwrap_or(0);
        let base = pos[i] + t.index1.unwrap_or(start).saturating_sub(start) * FRAME as u64;
        // Up to the next track's first index (its pregap is not part of this track).
        let end = match tracks.get(i + 1) {
            Some(n) if n.file == t.file => pos[i + 1],
            _ => std::fs::metadata(&t.file)?.len(),
        };
        out.push(AudioTrack {
            number: i as u32 + 1,
            src: shared_file(&t.file)?,
            base,
            stride: FRAME as u32,
            frames: end.saturating_sub(base) / FRAME as u64,
            swap: false,
        });
    }
    Ok(out)
}

fn gdi(path: &Path) -> Result<Vec<AudioTrack>> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 5 || f[2] != "0" {
            continue; // not an audio track
        }
        let ss: u32 = f[3].parse().unwrap_or(2352);
        // "file name" may be quoted (spaces); otherwise it's the 5th field.
        let rest = line.splitn(5, char::is_whitespace).nth(4).unwrap_or("").trim();
        let name = match rest.strip_prefix('"') {
            Some(r) => r.split('"').next().unwrap_or("").to_string(),
            None => f[4].to_string(),
        };
        let Some(file) = super::cd::resolve_track_file(path, &name)? else { continue };
        let len = std::fs::metadata(&file)?.len();
        out.push(AudioTrack { number: f[0].parse().unwrap_or(0), src: shared_file(&file)?, base: 0, stride: ss, frames: len / ss as u64, swap: false });
    }
    Ok(out)
}

/// Alcohol MDS v1: track blocks with mode 0xA9 are audio.
fn mds(mds: &Path) -> Result<Vec<AudioTrack>> {
    let m = std::fs::read(mds)?;
    if m.len() < 0x58 || &m[0..16] != b"MEDIA DESCRIPTOR" || m[16] != 1 {
        return Ok(Vec::new());
    }
    let mdf = ["mdf", "MDF"].iter().map(|e| mds.with_extension(e)).find(|p| p.exists()).context("the .mdf file is missing")?;
    let src = shared(super::pieces::concat_files(&super::pieces::mdf_parts(&mdf))?);
    let total = src.borrow().len();
    let sessions = le16(&m, 0x14) as usize;
    let sess_off = le32(&m, 0x50) as usize;
    let mut entries = Vec::new();
    for s in 0..sessions {
        let so = sess_off + s * 24;
        let (Some(n), Some(t)) = (m.get(so + 0x0A), m.get(so + 0x14..so + 0x18)) else { break };
        let toff = u32::from_le_bytes(t.try_into().unwrap()) as usize;
        for b in 0..*n as usize {
            let Some(t) = m.get(toff + b * 80..toff + b * 80 + 80) else { break };
            if (1..=99).contains(&t[4]) {
                entries.push((t[4], t[0], le16(t, 0x10) as u32, le64(t, 0x28)));
            }
        }
    }
    entries.sort_by_key(|e| e.0);
    let mut out = Vec::new();
    for (i, &(point, mode, ss, off)) in entries.iter().enumerate() {
        if mode & 0x0F != 0x09 || ss < FRAME as u32 {
            continue;
        }
        let end = entries.get(i + 1).map(|e| e.3).filter(|&e| e > off).unwrap_or(total);
        out.push(AudioTrack { number: point as u32, src: src.clone(), base: off, stride: ss, frames: (end - off) / ss as u64, swap: false });
    }
    Ok(out)
}

/// Nero NRG: DAO track entries with mode code 7 are audio.
fn nrg(path: &Path) -> Result<Vec<AudioTrack>> {
    let file = open_ro(path)?;
    let len = file.metadata()?.len();
    let tail = read_vec(&file, len.saturating_sub(12), 12)?;
    let chunks_at = if &tail[0..4] == b"NER5" {
        be64(&tail, 4)
    } else if &tail[4..8] == b"NERO" {
        be32(&tail, 8) as u64
    } else {
        return Ok(Vec::new());
    };
    let src = shared_file(path)?;
    let mut out = Vec::new();
    let mut p = chunks_at;
    let mut number = 0;
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
                number += 1;
                let ss = u16::from_be_bytes([body[o + 12], body[o + 13]]) as u32;
                let mode = body[o + 14];
                let (start, end) = if wide {
                    (be64(&body, o + 26), be64(&body, o + 34))
                } else {
                    (be32(&body, o + 22) as u64, be32(&body, o + 26) as u64)
                };
                if mode == 7 && ss >= FRAME as u32 {
                    out.push(AudioTrack { number, src: src.clone(), base: start, stride: ss, frames: end.saturating_sub(start) / ss as u64, swap: false });
                }
                o += entry;
            }
        }
        p += 8 + size;
    }
    Ok(out)
}

fn wav_header(data_len: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&2u16.to_le_bytes()); // stereo
    h[24..28].copy_from_slice(&44_100u32.to_le_bytes());
    h[28..32].copy_from_slice(&176_400u32.to_le_bytes()); // bytes per second
    h[32..34].copy_from_slice(&4u16.to_le_bytes()); // block align
    h[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits per sample
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

/// Bytes of PCM in a track's WAV file.
fn pcm_len(t: &AudioTrack) -> u32 {
    (t.frames * FRAME as u64).min(u32::MAX as u64 - 36) as u32
}

/// Write a track as a complete WAV file (header + little-endian PCM).
fn write_wav(t: &AudioTrack, w: &mut impl Write, mut tick: impl FnMut(u64)) -> Result<()> {
    w.write_all(&wav_header(pcm_len(t)))?;
    let mut raw = Vec::new();
    let mut frame = 0u64;
    while frame < t.frames {
        let n = (t.frames - frame).min(1024);
        raw.resize(n as usize * t.stride as usize, 0);
        t.src.borrow_mut().read_at(t.base + frame * t.stride as u64, &mut raw)?;
        for k in 0..n as usize {
            let s = &mut raw[k * t.stride as usize..k * t.stride as usize + FRAME];
            if t.swap {
                for pair in s.chunks_exact_mut(2) {
                    pair.swap(0, 1);
                }
            }
            w.write_all(s)?;
        }
        frame += n;
        tick(n * FRAME as u64);
    }
    Ok(())
}

/// Write each track as "Track NN.wav" in `dir`. Returns the files written.
pub fn export(tracks: &[AudioTrack], dir: &Path) -> Result<Vec<PathBuf>> {
    if tracks.is_empty() {
        bail!("this disc has no audio tracks");
    }
    std::fs::create_dir_all(dir)?;
    let mut written = Vec::new();
    for t in tracks {
        let out = dir.join(format!("Track {:02}.wav", t.number));
        let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(&out).with_context(|| format!("creating {}", out.display()))?);
        write_wav(t, &mut w, |_| {})?;
        w.flush()?;
        written.push(out);
    }
    Ok(written)
}

// ---------------------------------------------------------------- audio companion disc

const SECTOR: u64 = 2048;

fn both16(v: u16) -> [u8; 4] {
    let (l, b) = (v.to_le_bytes(), v.to_be_bytes());
    [l[0], l[1], b[0], b[1]]
}

fn both32(v: u32) -> [u8; 8] {
    let (l, b) = (v.to_le_bytes(), v.to_be_bytes());
    [l[0], l[1], l[2], l[3], b[0], b[1], b[2], b[3]]
}

/// An ISO 9660 directory record.
fn dir_record(name: &[u8], lba: u32, size: u32, is_dir: bool) -> Vec<u8> {
    let n = name.len();
    let mut r = vec![0u8; 33 + n + (n + 1) % 2];
    r[0] = r.len() as u8;
    r[2..10].copy_from_slice(&both32(lba));
    r[10..18].copy_from_slice(&both32(size));
    r[18..25].copy_from_slice(&[100, 1, 1, 0, 0, 0, 0]); // 2000-01-01
    r[25] = if is_dir { 2 } else { 0 };
    r[28..32].copy_from_slice(&both16(1));
    r[32] = n as u8;
    r[33..33 + n].copy_from_slice(name);
    r
}

/// The CUE sheet that presents the WAV files as one album.
fn album_cue(tracks: &[AudioTrack], title: &str) -> String {
    let mut s = format!("REM Made by rustimgmount from the disc's CD audio tracks\r\nTITLE \"{}\"\r\n", title.replace('"', "'"));
    for t in tracks {
        s.push_str(&format!("FILE \"TRACK{0:02}.WAV\" WAVE\r\n  TRACK {0:02} AUDIO\r\n    INDEX 01 00:00:00\r\n", t.number));
    }
    s
}

/// Volume label: ISO 9660 allows A-Z, 0-9 and underscore, 32 characters.
fn volume_label(title: &str) -> String {
    let mut l: String = title.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    while l.contains("__") {
        l = l.replace("__", "_");
    }
    let l = l.trim_matches('_');
    format!("{}_AUDIO", &l[..l.len().min(26)])
}

/// Size in bytes of the ISO `write_iso` would produce.
pub fn iso_size(tracks: &[AudioTrack]) -> u64 {
    let audio: u64 = tracks.iter().map(|t| (44 + pcm_len(t) as u64).div_ceil(SECTOR)).sum();
    (24 + audio + 2) * SECTOR
}

/// Build a small data disc holding TRACKnn.WAV for every audio track plus ALBUM.CUE.
/// Mounted next to the game's data drive, it lets any audio player (foobar2000, ...)
/// play the disc's music, which a mounted data drive cannot show.
pub fn write_iso(tracks: &[AudioTrack], title: &str, out: &Path, quiet: bool) -> Result<()> {
    if tracks.is_empty() {
        bail!("this disc has no audio tracks");
    }
    let cue = album_cue(tracks, title).into_bytes();
    // Files in ISO 9660 name order: ALBUM.CUE, then TRACK01.WAV, TRACK02.WAV, ...
    let mut files: Vec<(String, u64)> = vec![("ALBUM.CUE;1".into(), cue.len() as u64)];
    let mut sorted: Vec<&AudioTrack> = tracks.iter().collect();
    sorted.sort_by_key(|t| t.number);
    for t in &sorted {
        files.push((format!("TRACK{:02}.WAV;1", t.number), 44 + pcm_len(t) as u64));
    }
    // Root directory: records never cross a sector boundary.
    let root_lba = 20u32;
    let mut probe = 34 * 2; // "." and ".."
    let mut root_sectors = 1u32;
    for (name, _) in &files {
        let len = 33 + name.len() + (name.len() + 1) % 2;
        if probe + len > SECTOR as usize {
            root_sectors += 1;
            probe = 0;
        }
        probe += len;
    }
    let mut lba = root_lba + root_sectors;
    let mut placed = Vec::new();
    for (name, size) in &files {
        placed.push((name.clone(), lba, *size));
        lba += size.div_ceil(SECTOR) as u32;
    }
    let total = lba;
    let root_size = root_sectors * SECTOR as u32;

    let mut root = vec![0u8; root_size as usize];
    let mut pos = 0usize;
    for rec in [dir_record(&[0], root_lba, root_size, true), dir_record(&[1], root_lba, root_size, true)]
        .into_iter()
        .chain(placed.iter().map(|(n, l, s)| dir_record(n.as_bytes(), *l, *s as u32, false)))
    {
        if pos % SECTOR as usize + rec.len() > SECTOR as usize {
            pos = pos.div_ceil(SECTOR as usize) * SECTOR as usize;
        }
        root[pos..pos + rec.len()].copy_from_slice(&rec);
        pos += rec.len();
    }

    let mut pvd = vec![0u8; SECTOR as usize];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    pvd[8..72].fill(b' ');
    let label = volume_label(title);
    pvd[40..40 + label.len()].copy_from_slice(label.as_bytes());
    pvd[80..88].copy_from_slice(&both32(total));
    pvd[120..124].copy_from_slice(&both16(1));
    pvd[124..128].copy_from_slice(&both16(1));
    pvd[128..132].copy_from_slice(&both16(SECTOR as u16));
    pvd[132..140].copy_from_slice(&both32(10));
    pvd[140..144].copy_from_slice(&18u32.to_le_bytes());
    pvd[148..152].copy_from_slice(&19u32.to_be_bytes());
    pvd[156..190].copy_from_slice(&dir_record(&[0], root_lba, root_size, true));
    pvd[190..813].fill(b' ');
    for o in [813, 830, 847, 864] {
        pvd[o..o + 16].fill(b'0');
        pvd[o + 16] = 0;
    }
    pvd[813..829].copy_from_slice(b"2000010100000000");
    pvd[881] = 1;
    let mut term = vec![0u8; SECTOR as usize];
    term[0] = 255;
    term[1..6].copy_from_slice(b"CD001");
    term[6] = 1;
    let path_table = |be: bool| {
        let mut p = vec![0u8; SECTOR as usize];
        p[0] = 1;
        p[2..6].copy_from_slice(&if be { root_lba.to_be_bytes() } else { root_lba.to_le_bytes() });
        p[6..8].copy_from_slice(&if be { 1u16.to_be_bytes() } else { 1u16.to_le_bytes() });
        p
    };

    let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?);
    let mut p = crate::util::Progress::new("Preparing audio drive", total as u64 * SECTOR);
    if quiet {
        p = p.quiet();
    }
    w.write_all(&vec![0u8; 16 * SECTOR as usize])?;
    w.write_all(&pvd)?;
    w.write_all(&term)?;
    w.write_all(&path_table(false))?;
    w.write_all(&path_table(true))?;
    w.write_all(&root)?;
    let pad = |w: &mut BufWriter<std::fs::File>, written: u64| -> Result<()> {
        w.write_all(&vec![0u8; ((SECTOR - written % SECTOR) % SECTOR) as usize])?;
        Ok(())
    };
    w.write_all(&cue)?;
    pad(&mut w, cue.len() as u64)?;
    let mut done = (root_lba + root_sectors) as u64 * SECTOR;
    for t in &sorted {
        write_wav(t, &mut w, |n| {
            done += n;
            p.update(done);
        })?;
        pad(&mut w, 44 + pcm_len(t) as u64)?;
    }
    w.flush()?;
    p.finish();
    Ok(())
}
