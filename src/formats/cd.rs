//! Optical disc images.
//!
//! Every container (plain ISO, raw BIN, CUE, NRG, MDS/MDF, GDI, CDI, CHD, CSO/ZSO/DAX, ...)
//! is reduced to a list of candidate data tracks. `Disc::assemble` then picks the track
//! holding the file system the OS would use (the last session's), works out at which LBA
//! that file system expects to live (multi-session discs, Dreamcast CDI and GD-ROM use
//! absolute sector numbers), and presents the result as one 2048-byte/sector ISO.

use crate::source::{be32, be64, le32, open_ro, read_vec, FileSlice, Source};
use anyhow::{anyhow, bail, Context, Result};
use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Volume descriptor identifiers for ISO 9660 and UDF (ECMA-167) volume recognition sequences.
const VD_IDS: [&[u8; 5]; 5] = [b"CD001", b"BEA01", b"NSR02", b"NSR03", b"TEA01"];
/// Raw sector sizes: cooked, raw, mode2 headerless, raw + subchannel, mode2 + subheader.
pub const SECTOR_SIZES: [u32; 5] = [2048, 2352, 2336, 2448, 2056];
/// Offset of user data inside a sector: none, mode1, mode2 form1, mode2 headerless.
const DATA_OFFSETS: [u32; 4] = [0, 16, 24, 8];
const SYNC: [u8; 12] = [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0];

pub type Shared = Rc<RefCell<Box<dyn Source>>>;

pub fn shared(s: impl Source + 'static) -> Shared {
    Rc::new(RefCell::new(Box::new(s)))
}

/// A shared source used as an ordinary one.
pub struct SharedSrc(pub Shared);
impl Source for SharedSrc {
    fn len(&self) -> u64 {
        self.0.borrow().len()
    }
    fn read_at(&mut self, o: u64, b: &mut [u8]) -> io::Result<()> {
        self.0.borrow_mut().read_at(o, b)
    }
    fn is_zero(&mut self, o: u64, n: u64) -> io::Result<bool> {
        self.0.borrow_mut().is_zero(o, n)
    }
}

pub fn shared_file(path: &Path) -> Result<Shared> {
    Ok(shared(FileSlice::whole(path).with_context(|| format!("opening {}", path.display()))?))
}

/// A possible data track, as described by the container.
pub struct TrackCand {
    pub src: Shared,
    /// Byte offset of the track's first (INDEX 01) sector in `src`.
    pub base: u64,
    pub sector_size: u32,
    /// Where the container says the user data sits inside each sector, if it says.
    pub data_offset_hint: Option<u32>,
    pub sectors: u64,
    /// The track's INDEX 01 address according to the container, if known.
    pub meta_lba: Option<i64>,
    pub label: String,
}

impl TrackCand {
    pub fn new(src: Shared, base: u64, sector_size: u32, sectors: u64) -> Self {
        Self { src, base, sector_size, data_offset_hint: None, sectors, meta_lba: None, label: String::new() }
    }
    pub fn hint(mut self, d: Option<u32>) -> Self {
        self.data_offset_hint = d;
        self
    }
    pub fn lba(mut self, lba: Option<i64>) -> Self {
        self.meta_lba = lba;
        self
    }
    pub fn label(mut self, l: impl Into<String>) -> Self {
        self.label = l.into();
        self
    }
}

/// A placed data track.
pub struct Track {
    src: Shared,
    pub base: u64,
    pub sector_size: u32,
    pub data_offset: u32,
    pub sectors: u64,
    /// Position of the track's first sector in the output image.
    pub lba: u64,
    pub label: String,
}

impl Track {
    /// Read `out.len() / 2048` user-data sectors starting at track-relative sector `rel`.
    fn read(&self, rel: u64, out: &mut [u8]) -> io::Result<()> {
        let ss = self.sector_size as u64;
        let mut src = self.src.borrow_mut();
        if ss == 2048 && self.data_offset == 0 {
            return src.read_at(self.base + rel * 2048, out);
        }
        let total = out.len() / 2048;
        let mut raw = Vec::new();
        let mut done = 0;
        while done < total {
            let n = (total - done).min(512);
            raw.resize(n * ss as usize, 0);
            src.read_at(self.base + (rel + done as u64) * ss, &mut raw)?;
            for k in 0..n {
                let s = k * ss as usize + self.data_offset as usize;
                out[(done + k) * 2048..(done + k + 1) * 2048].copy_from_slice(&raw[s..s + 2048]);
            }
            done += n;
        }
        Ok(())
    }

    fn sector(&self, rel: u64) -> io::Result<Vec<u8>> {
        let mut b = vec![0u8; 2048];
        if rel < self.sectors {
            self.read(rel, &mut b)?;
        }
        Ok(b)
    }
}

fn is_vd(b: &[u8]) -> bool {
    b.len() >= 6 && VD_IDS.iter().any(|id| &b[1..6] == *id)
}

/// Read 6 bytes of user data at the start of a sector, given a geometry guess.
fn peek(src: &Shared, base: u64, ss: u32, d: u32, sector: u64) -> io::Result<[u8; 6]> {
    let mut b = [0u8; 6];
    src.borrow_mut().read_at(base + sector * ss as u64 + d as u64, &mut b)?;
    Ok(b)
}

/// Do sectors 16 and 17 of this geometry hold volume descriptors?
fn has_vds(src: &Shared, base: u64, ss: u32, d: u32) -> io::Result<bool> {
    Ok(is_vd(&peek(src, base, ss, d, 16)?) && is_vd(&peek(src, base, ss, d, 17)?))
}

/// Find the user-data offset at which this track has a file system, if any.
fn probe_offset(c: &TrackCand) -> io::Result<Option<u32>> {
    let mut order: Vec<u32> = c.data_offset_hint.into_iter().collect();
    order.extend(DATA_OFFSETS.iter().copied().filter(|d| Some(*d) != c.data_offset_hint));
    for d in order {
        if d + 2048 <= c.sector_size && has_vds(&c.src, c.base, c.sector_size, d)? {
            return Ok(Some(d));
        }
    }
    Ok(None)
}

/// Data offset for a track without a file system of its own, from the sector header.
fn guess_offset(c: &TrackCand) -> io::Result<u32> {
    if let Some(d) = c.data_offset_hint {
        return Ok(d);
    }
    Ok(match c.sector_size {
        2048 => 0,
        2336 => 8,
        2056 => 8,
        _ => {
            let mut h = [0u8; 16];
            c.src.borrow_mut().read_at(c.base, &mut h)?;
            if h[..12] == SYNC && h[15] == 2 {
                24
            } else {
                16
            }
        }
    })
}

/// A disc ready to be presented as a 2048-byte/sector image.
pub struct Disc {
    pub tracks: Vec<Track>,
    /// Number of volume descriptor sectors mirrored to LBA 16.. when the file system
    /// lives further into the disc (see module docs).
    vd_mirror: u64,
    /// Set when the image file is already a plain ISO that can be attached as-is.
    pub plain_file: Option<PathBuf>,
}

impl Disc {
    /// Choose and place data tracks. `plain` is the image path when the candidates came
    /// straight from a single file (so a plain ISO can be used without conversion).
    pub fn assemble(cands: Vec<TrackCand>, plain: Option<&Path>) -> Result<Disc> {
        // Which candidates carry a file system?
        let mut probed = Vec::with_capacity(cands.len());
        for c in cands {
            // Sector sizes come from sheets and headers; real ones are 2048..2448 bytes.
            if !(2048..=4096).contains(&c.sector_size) {
                continue;
            }
            let fs = probe_offset(&c)?;
            probed.push((c, fs));
        }
        // The OS mounts the last session's file system: take the last track that has one.
        let primary = probed
            .iter()
            .rposition(|(_, fs)| fs.is_some())
            .ok_or_else(|| anyhow!("no data track with a readable file system (ISO 9660 / UDF) found"))?;

        let mut tracks: Vec<Track> = Vec::new();
        let mut primary_meta = None;
        for (i, (c, fs)) in probed.into_iter().enumerate().skip(primary) {
            let d = match fs {
                Some(d) => d,
                None => guess_offset(&c)?,
            };
            let mut t = Track {
                src: c.src,
                base: c.base,
                sector_size: c.sector_size,
                data_offset: d,
                sectors: c.sectors,
                lba: 0,
                label: c.label,
            };
            if i == primary {
                t.lba = fs_lba(&t, c.meta_lba)?;
                primary_meta = c.meta_lba.map(|m| (m, t.lba));
            } else {
                // Later data tracks sit at their distance from the primary track.
                match (primary_meta, c.meta_lba) {
                    (Some((pm, plba)), Some(m)) if m > pm => t.lba = plba + (m - pm) as u64,
                    _ => continue,
                }
            }
            tracks.push(t);
        }

        let p = &tracks[0];
        let mut vd_mirror = 0;
        if p.lba > 16 {
            // Mirror the volume descriptor set to the start, where the OS looks for it.
            for k in 16..(16 + 64).min(p.lba).min(p.sectors) {
                let b = p.sector(k)?;
                if !is_vd(&b) {
                    break;
                }
                vd_mirror = k - 15;
                if b[0] == 255 {
                    break;
                }
            }
        }

        let mut plain_file = None;
        if let Some(path) = plain {
            let len = std::fs::metadata(path)?.len();
            if tracks.len() == 1 && p.base == 0 && p.sector_size == 2048 && p.data_offset == 0 && p.lba == 0 && len % 2048 == 0 {
                plain_file = Some(path.to_owned());
            }
        }
        Ok(Disc { tracks, vd_mirror, plain_file })
    }

    /// The whole image as one raw track (source, byte offset, sector size, sectors), when
    /// it is a single track of raw sectors that starts the disc.
    pub fn single_raw(&self) -> Option<(Shared, u64, u32, u64)> {
        let t = self.tracks.first()?;
        (self.tracks.len() == 1 && t.lba == 0 && self.vd_mirror == 0 && matches!(t.sector_size, 2336 | 2352 | 2448))
            .then(|| (t.src.clone(), t.base, t.sector_size, t.sectors))
    }

    pub fn describe(&self) -> Vec<String> {
        self.tracks
            .iter()
            .map(|t| {
                format!(
                    "{}{} sectors of {} bytes (data at +{}), file offset {}, placed at LBA {}",
                    if t.label.is_empty() { String::new() } else { format!("{}: ", t.label) },
                    t.sectors,
                    t.sector_size,
                    t.data_offset,
                    t.base,
                    t.lba
                )
            })
            .collect()
    }
}

/// Where the file system on this track expects the track to start, found by locating
/// the root directory the primary volume descriptor points to.
fn fs_lba(t: &Track, guess: Option<i64>) -> Result<u64> {
    let pvd = t.sector(16)?;
    if &pvd[1..6] != b"CD001" || pvd[0] != 1 {
        return Ok(0); // UDF-only: always relative in practice
    }
    let root = le32(&pvd, 158) as u64;
    let is_root = |rel: u64| -> io::Result<bool> {
        if rel >= t.sectors {
            return Ok(false);
        }
        let b = t.sector(rel)?;
        Ok(b[0] >= 34 && b[25] & 2 != 0 && b[32] == 1 && b[33] == 0 && le32(&b, 2) as u64 == root)
    };
    if is_root(root)? {
        return Ok(0);
    }
    if let Some(g) = guess.filter(|g| *g > 0).map(|g| g as u64) {
        if root >= g && is_root(root - g)? {
            return Ok(g);
        }
    }
    for rel in 0..t.sectors.min(root + 1).min(40_000) {
        if is_root(rel)? {
            return Ok(root - rel);
        }
    }
    Ok(0)
}

impl Source for Disc {
    fn len(&self) -> u64 {
        self.tracks.iter().map(|t| t.lba + t.sectors).max().unwrap_or(0) * 2048
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let first = offset / 2048;
        let last = (offset + buf.len() as u64 - 1) / 2048;
        let n = last - first + 1;
        let mut tmp = vec![0u8; (n * 2048) as usize];
        let range = |a: u64, b: u64| (a.max(first), b.min(last + 1));
        for t in &self.tracks {
            let (a, b) = range(t.lba, t.lba + t.sectors);
            if a < b {
                let out = &mut tmp[((a - first) * 2048) as usize..((b - first) * 2048) as usize];
                t.read(a - t.lba, out)?;
            }
        }
        if self.vd_mirror > 0 {
            let p = &self.tracks[0];
            let (a, b) = range(16, (16 + self.vd_mirror).min(p.lba));
            if a < b {
                let out = &mut tmp[((a - first) * 2048) as usize..((b - first) * 2048) as usize];
                p.read(a, out)?;
            }
        }
        let skip = (offset - first * 2048) as usize;
        buf.copy_from_slice(&tmp[skip..skip + buf.len()]);
        Ok(())
    }
}

// ---------------------------------------------------------------- plain files

/// A single file holding one track somewhere inside (ISO, BIN, IMG, MDF, CDI, ...).
pub fn scan(path: &Path) -> Result<Option<TrackCand>> {
    let src = shared_file(path)?;
    let len = std::fs::metadata(path)?.len();
    // Fast path: a track starting at offset 0.
    for ss in [2048u32, 2352, 2448, 2336] {
        let c = TrackCand::new(src.clone(), 0, ss, len / ss as u64);
        if probe_offset(&c)?.is_some() {
            return Ok(Some(c));
        }
    }
    // Search for a volume descriptor signature anywhere (e.g. CDI, multi-session dumps).
    let file = open_ro(path)?;
    const CHUNK: usize = 4 << 20;
    let mut buf = vec![0u8; CHUNK + 8];
    let mut pos = 0u64;
    while pos < len {
        let n = ((len - pos) as usize).min(CHUNK + 8);
        crate::source::pread(&file, pos, &mut buf[..n])?;
        let hay = &buf[..n];
        for i in 1..n.saturating_sub(5) {
            if !matches!(hay[i], b'C' | b'B' | b'N' | b'T') || !VD_IDS.iter().any(|id| &hay[i..i + 5] == *id) {
                continue;
            }
            let q = pos + i as u64 - 1; // start of descriptor
            for ss in SECTOR_SIZES {
                for d in DATA_OFFSETS {
                    let back = 16 * ss as u64 + d as u64;
                    if d + 2048 > ss || q < back {
                        continue;
                    }
                    let base = q - back;
                    if has_vds(&src, base, ss, d)? {
                        return Ok(Some(TrackCand::new(src.clone(), base, ss, (len - base) / ss as u64).hint(Some(d))));
                    }
                }
            }
        }
        pos += CHUNK as u64;
    }
    Ok(None)
}

pub fn from_file(path: &Path) -> Result<Option<Disc>> {
    match scan(path)? {
        Some(c) => Ok(Some(Disc::assemble(vec![c], Some(path))?)),
        None => Ok(None),
    }
}

/// Cheap check: does the file have a track with a file system starting at offset 0?
pub fn probe_base0(path: &Path) -> Result<bool> {
    let src = shared_file(path)?;
    let len = std::fs::metadata(path)?.len();
    for ss in [2048u32, 2352, 2448, 2336] {
        if probe_offset(&TrackCand::new(src.clone(), 0, ss, len / ss as u64))?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Resolve a file referenced by a cue/gdi/descriptor: as written, next to the sheet,
/// or by bare file name next to the sheet.
pub fn resolve_sibling(sheet: &Path, name: &str) -> Option<PathBuf> {
    let dir = sheet.parent().unwrap_or(Path::new("."));
    let p = Path::new(name);
    let cand = if p.is_absolute() { p.to_owned() } else { dir.join(p) };
    if cand.exists() {
        return Some(cand);
    }
    let bare = name.replace('\\', "/");
    let bare = bare.rsplit('/').next().unwrap_or(name);
    let cand = dir.join(bare);
    cand.exists().then_some(cand)
}

/// Like `resolve_sibling`, but also accepts an ECM-compressed copy (`name.ecm`),
/// which is decoded into the cache.
pub fn resolve_track_file(sheet: &Path, name: &str) -> Result<Option<PathBuf>> {
    if let Some(p) = resolve_sibling(sheet, name) {
        return Ok(Some(p));
    }
    if let Some(ecm) = resolve_sibling(sheet, &format!("{name}.ecm")) {
        return Ok(Some(super::expand::expand(&ecm)?));
    }
    Ok(None)
}

// ---------------------------------------------------------------- CUE sheets

pub(crate) struct CueTrack {
    pub(crate) file: PathBuf,
    pub(crate) mode: String,
    pub(crate) index0: Option<u64>,
    pub(crate) index1: Option<u64>,
    /// Frames of silence a PREGAP line puts before the track (not stored in the file).
    pub(crate) pregap: u64,
}

fn parse_msf(s: &str) -> Option<u64> {
    let p: Vec<u64> = s.split(':').map(|x| x.trim().parse().ok()).collect::<Option<_>>()?;
    (p.len() == 3).then(|| (p[0] * 60 + p[1]) * 75 + p[2])
}

/// Sector size and user-data offset for a CUE/CDRWIN track mode.
pub(crate) fn cue_mode(mode: &str) -> (u32, Option<u32>) {
    match mode.to_ascii_uppercase().as_str() {
        "MODE1/2048" => (2048, Some(0)),
        "MODE1/2352" => (2352, Some(16)),
        "MODE2/2336" => (2336, Some(8)),
        "MODE2/2352" => (2352, Some(24)),
        "MODE2/2048" => (2048, Some(0)),
        "CDI/2336" => (2336, Some(8)),
        "CDI/2352" => (2352, Some(24)),
        "CDG" => (2448, None),
        _ => (2352, None),
    }
}

pub(crate) fn unquote(rest: &str) -> String {
    let rest = rest.trim();
    if let Some(r) = rest.strip_prefix('"') {
        if let Some(end) = r.find('"') {
            return r[..end].to_string();
        }
    }
    // FILE name.bin BINARY -> drop the trailing type
    match rest.rsplit_once(char::is_whitespace) {
        Some((name, _)) => name.trim().to_string(),
        None => rest.to_string(),
    }
}

pub fn from_cue(cue: &Path) -> Result<Disc> {
    let (tracks, pos, abs_index1) = parse_cue(cue)?;
    disc_from_cue(&tracks, &pos, &abs_index1)
}

/// Parse a cue sheet: its tracks, the byte position of each track's first index in its
/// file, and each track's absolute INDEX 01 frame across all files.
pub(crate) fn parse_cue(cue: &Path) -> Result<(Vec<CueTrack>, Vec<u64>, Vec<i64>)> {
    let text = std::fs::read(cue).with_context(|| format!("reading {}", cue.display()))?;
    let text = String::from_utf8_lossy(&text);
    let mut tracks: Vec<CueTrack> = Vec::new();
    let mut cur_file: Option<PathBuf> = None;
    for line in text.lines() {
        let line = line.trim();
        let (kw, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        match kw.to_ascii_uppercase().as_str() {
            "FILE" => {
                let name = unquote(rest);
                let f = match resolve_track_file(cue, &name)? {
                    Some(f) => Some(f),
                    // Cue sheets are often renamed without editing: try <cue stem>.bin
                    None => ["bin", "img", "BIN", "IMG"].iter().map(|e| cue.with_extension(e)).find(|p| p.exists()),
                };
                cur_file = Some(f.ok_or_else(|| anyhow!("CUE references missing file '{name}'"))?);
            }
            "TRACK" => {
                let mode = rest.split_whitespace().nth(1).unwrap_or("MODE1/2352").to_string();
                let file = cur_file.clone().ok_or_else(|| anyhow!("TRACK before FILE in cue sheet"))?;
                tracks.push(CueTrack { file, mode, index0: None, index1: None, pregap: 0 });
            }
            "PREGAP" => {
                if let (Some(tr), Some(n)) = (tracks.last_mut(), parse_msf(rest)) {
                    tr.pregap = n;
                }
            }
            "INDEX" => {
                let mut it = rest.split_whitespace();
                let n: u32 = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
                let t = it.next().and_then(parse_msf);
                if let Some(tr) = tracks.last_mut() {
                    match n {
                        0 => tr.index0 = t,
                        1 => tr.index1 = t,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if tracks.is_empty() {
        bail!("cue sheet has no tracks");
    }

    // Byte position of each track's first index within its file, and the absolute
    // frame address of every track across all files.
    let mut pos = vec![0u64; tracks.len()];
    let mut file_start_frame = 0u64; // absolute frame where the current file begins
    let mut abs_index1 = vec![0i64; tracks.len()];
    for i in 0..tracks.len() {
        let t = &tracks[i];
        let start = t.index0.or(t.index1).unwrap_or(0);
        if i > 0 && tracks[i - 1].file == t.file {
            let p = &tracks[i - 1];
            let pf = p.index0.or(p.index1).unwrap_or(0);
            pos[i] = pos[i - 1] + start.saturating_sub(pf) * cue_mode(&p.mode).0 as u64;
        } else {
            if i > 0 {
                // Previous file ended: advance by its length in frames.
                let prev_file = &tracks[i - 1].file;
                let first_in_prev = (0..i).rev().take_while(|&j| &tracks[j].file == prev_file).last().unwrap();
                let mut frames = 0;
                for j in first_in_prev..i {
                    let ss = cue_mode(&tracks[j].mode).0 as u64;
                    let end = if j + 1 < i { pos[j + 1] } else { std::fs::metadata(prev_file)?.len() };
                    frames += end.saturating_sub(pos[j]) / ss;
                }
                file_start_frame += frames;
            }
            pos[i] = start * cue_mode(&t.mode).0 as u64;
        }
        abs_index1[i] = (file_start_frame + t.index1.unwrap_or(start)) as i64;
    }
    Ok((tracks, pos, abs_index1))
}

fn disc_from_cue(tracks: &[CueTrack], pos: &[u64], abs_index1: &[i64]) -> Result<Disc> {
    let mut cands = Vec::new();
    for (i, t) in tracks.iter().enumerate() {
        if t.mode.eq_ignore_ascii_case("AUDIO") {
            continue;
        }
        let (ss, d) = cue_mode(&t.mode);
        let start = t.index0.or(t.index1).unwrap_or(0);
        let base = pos[i] + t.index1.unwrap_or(start).saturating_sub(start) * ss as u64;
        let end = match tracks.get(i + 1) {
            Some(n) if n.file == t.file => pos[i + 1],
            _ => std::fs::metadata(&t.file)?.len(),
        };
        let src = shared_file(&t.file)?;
        cands.push(
            TrackCand::new(src, base, ss, end.saturating_sub(base) / ss as u64)
                .hint(d)
                .lba(Some(abs_index1[i]))
                .label(format!("track {}", i + 1)),
        );
    }
    match Disc::assemble(cands, None) {
        Ok(d) => Ok(d),
        // The sheet may be wrong about modes; fall back to scanning the first file.
        Err(e) => from_file(&tracks[0].file)?.ok_or(e),
    }
}

// ---------------------------------------------------------------- Dreamcast GDI

pub fn from_gdi(gdi: &Path) -> Result<Disc> {
    let text = std::fs::read_to_string(gdi).with_context(|| format!("reading {}", gdi.display()))?;
    let mut cands = Vec::new();
    for line in text.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // num lba type sector_size "file name" offset
        let mut it = line.splitn(5, char::is_whitespace).filter(|s| !s.is_empty());
        let (Some(num), Some(lba), Some(ty), Some(ss)) = (it.next(), it.next(), it.next(), it.next()) else { continue };
        let rest = line.splitn(5, char::is_whitespace).nth(4).unwrap_or("").trim();
        let name = if let Some(r) = rest.strip_prefix('"') {
            r.split('"').next().unwrap_or("").to_string()
        } else {
            rest.split_whitespace().next().unwrap_or("").to_string()
        };
        if ty.trim() != "4" {
            continue; // audio
        }
        let lba: i64 = lba.parse().context("bad LBA in GDI")?;
        let ss: u32 = ss.parse().context("bad sector size in GDI")?;
        let file = resolve_track_file(gdi, &name)?.ok_or_else(|| anyhow!("GDI references missing file '{name}'"))?;
        let len = std::fs::metadata(&file)?.len();
        cands.push(
            TrackCand::new(shared_file(&file)?, 0, ss, len / ss as u64)
                .hint(if ss == 2048 { Some(0) } else { None })
                .lba(Some(lba))
                .label(format!("track {num}")),
        );
    }
    Disc::assemble(cands, None)
}

// ---------------------------------------------------------------- Nero NRG

/// Returns `Ok(None)` if the file has no NRG footer.
pub fn from_nrg(path: &Path) -> Result<Option<Disc>> {
    let file = open_ro(path)?;
    let len = file.metadata()?.len();
    if len < 12 {
        return Ok(None);
    }
    let tail = read_vec(&file, len - 12, 12)?;
    let chunks_at = if &tail[0..4] == b"NER5" {
        be64(&tail, 4)
    } else if &tail[4..8] == b"NERO" {
        be32(&tail, 8) as u64
    } else {
        return Ok(None);
    };
    if chunks_at >= len {
        bail!("corrupt NRG footer");
    }
    let src = shared_file(path)?;
    // (pregap offset, start offset, end offset, sector size or 0 if unknown)
    let mut entries: Vec<(u64, u64, u64, u32)> = Vec::new();
    let mut p = chunks_at;
    while p + 8 <= len {
        let h = read_vec(&file, p, 8)?;
        let id = &h[0..4];
        let size = be32(&h, 4) as u64;
        if id == b"END!" || size > len {
            break;
        }
        let body = read_vec(&file, p + 8, size as usize)?;
        match id {
            b"DAOX" | b"DAOI" => {
                let wide = id == b"DAOX";
                let entry = if wide { 42 } else { 30 };
                let mut o = 22;
                while o + entry <= body.len() {
                    let ss = u16::from_be_bytes([body[o + 12], body[o + 13]]) as u32;
                    let (pg, start, end) = if wide {
                        (be64(&body, o + 18), be64(&body, o + 26), be64(&body, o + 34))
                    } else {
                        (be32(&body, o + 18) as u64, be32(&body, o + 22) as u64, be32(&body, o + 26) as u64)
                    };
                    entries.push((pg, start, end, ss));
                    o += entry;
                }
            }
            b"ETN2" | b"ETNF" => {
                let wide = id == b"ETN2";
                let entry = if wide { 32 } else { 20 };
                let mut o = 0;
                while o + entry <= body.len() {
                    let (start, sz, mode) = if wide {
                        (be64(&body, o), be64(&body, o + 8), be32(&body, o + 16))
                    } else {
                        (be32(&body, o) as u64, be32(&body, o + 4) as u64, be32(&body, o + 8))
                    };
                    let ss = match mode {
                        3 => 2336,
                        6 | 7 => 2352,
                        _ => 0,
                    };
                    entries.push((start, start, start + sz, ss));
                    o += entry;
                }
            }
            _ => {}
        }
        p += 8 + size;
    }
    let mut cands = Vec::new();
    let mut lba = 0i64;
    for (i, (pg, start, end, ss)) in entries.into_iter().enumerate() {
        let end = end.min(chunks_at);
        // Unknown sector size: pick the one where a file system shows up, else 2048.
        let ss = if ss != 0 {
            ss
        } else {
            SECTOR_SIZES
                .into_iter()
                .find(|&s| probe_offset(&TrackCand::new(src.clone(), start, s, (end - start) / s as u64)).ok().flatten().is_some())
                .unwrap_or(2048)
        };
        lba += (start.saturating_sub(pg) / ss as u64) as i64;
        let sectors = end.saturating_sub(start) / ss as u64;
        cands.push(TrackCand::new(src.clone(), start, ss, sectors).lba(Some(lba)).label(format!("track {}", i + 1)));
        lba += sectors as i64;
    }
    if !cands.is_empty() {
        if let Ok(d) = Disc::assemble(cands, None) {
            return Ok(Some(d));
        }
    }
    from_file(path)
}

// ---------------------------------------------------------------- Alcohol MDS

/// Parse an Alcohol 120% .mds descriptor; tracks point into the .mdf file.
pub fn from_mds(mds: &Path, mdf: &Path) -> Result<Disc> {
    let parsed = (|| -> Result<Disc> {
        let m = std::fs::read(mds)?;
        if m.len() < 0x58 || &m[0..16] != b"MEDIA DESCRIPTOR" {
            bail!("not an MDS file");
        }
        let sessions = u16::from_le_bytes([m[0x14], m[0x15]]) as usize;
        let sess_off = le32(&m, 0x50) as usize;
        // Large images are split into .mdf, .md0/.md1, ...; offsets span all parts.
        let src = shared(super::pieces::concat_files(&super::pieces::mdf_parts(mdf))?);
        let mdf_len = src.borrow().len();
        let mut entries = Vec::new();
        for s in 0..sessions {
            let so = sess_off + s * 24;
            if so + 24 > m.len() {
                bail!("truncated MDS");
            }
            let nblocks = m[so + 0x0A] as usize;
            let toff = le32(&m, so + 0x14) as usize;
            for b in 0..nblocks {
                let t = toff + b * 80;
                if t + 80 > m.len() {
                    bail!("truncated MDS");
                }
                let point = m[t + 4];
                if point == 0 || point > 99 {
                    continue; // lead-in / TOC entries
                }
                let mode = m[t];
                let ss = u16::from_le_bytes([m[t + 0x10], m[t + 0x11]]) as u32;
                let start_lba = le32(&m, t + 0x24) as i32 as i64;
                let start_off = crate::source::le64(&m, t + 0x28);
                entries.push((point, mode, ss, start_lba, start_off));
            }
        }
        entries.sort_by_key(|e| e.0);
        let mut cands = Vec::new();
        for (i, &(point, mode, ss, lba, off)) in entries.iter().enumerate() {
            if mode & 0x0F == 0x09 || ss == 0 {
                continue; // audio
            }
            let end = entries.get(i + 1).map(|e| e.4).filter(|&e| e > off).unwrap_or(mdf_len);
            let hint = match (ss, mode & 0x0F) {
                (2048, _) => Some(0),
                (2352 | 2448, 0x0A) => Some(16),
                (2352 | 2448, _) => Some(24),
                _ => None,
            };
            cands.push(
                TrackCand::new(src.clone(), off, ss, (end - off) / ss as u64)
                    .hint(hint)
                    .lba(Some(lba))
                    .label(format!("track {point}")),
            );
        }
        Disc::assemble(cands, Some(mdf))
    })();
    match parsed {
        Ok(d) => Ok(d),
        Err(_) => from_file(mdf)?.ok_or_else(|| anyhow!("no data track found in {}", mdf.display())),
    }
}
