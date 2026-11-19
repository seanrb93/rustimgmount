//! Make an ISO image from the files in a folder.
//!
//! The image is ISO 9660 with a Joliet tree, which is what Windows, macOS and Linux all
//! read: Joliet carries the real names (Unicode, up to 103 characters), and the plain
//! ISO 9660 tree beside it carries shortened 8.3 names for anything older. Both trees
//! point at the same file data. Files of 4 GiB and more are recorded in several extents
//! (ISO 9660 "level 3"), which Windows has read since Vista.

use crate::export::Job;
use crate::util::Progress;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SECTOR: usize = 2048;
/// The longest name Joliet readers accept, in UTF-16 units.
const MAX_NAME: usize = 103;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// The most one extent of a file can hold: the largest whole number of sectors that
/// fits a 32-bit length. (RUSTIMGMOUNT_TEST_EXTENT, in sectors, shrinks it for the tests.)
fn max_extent() -> u64 {
    match std::env::var("RUSTIMGMOUNT_TEST_EXTENT").ok().and_then(|v| v.parse::<u64>().ok()) {
        Some(n) if n > 0 => n * SECTOR as u64,
        _ => 0xFFFF_F800,
    }
}

struct Node {
    /// The name in the Joliet tree.
    name: String,
    /// The name in the plain ISO 9660 tree ("README.TXT;1").
    short: String,
    path: PathBuf,
    dir: bool,
    hidden: bool,
    size: u64,
    time: [u8; 7],
    parent: usize,
    children: Vec<usize>,
    /// First sector of a file's data.
    lba: u32,
    /// Per tree (0 = ISO 9660, 1 = Joliet): where a folder's listing is, and its length.
    dir_lba: [u32; 2],
    dir_len: [u32; 2],
}

impl Node {
    /// The identifier recorded in tree `t`.
    fn id(&self, t: usize) -> Vec<u8> {
        if t == 0 {
            return self.short.as_bytes().to_vec();
        }
        let mut v: Vec<u8> = self.name.encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        if !self.dir {
            v.extend_from_slice(&[0, b';', 0, b'1']);
        }
        v
    }
}

/// What `create` did, for the summary.
pub struct Made {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    pub notes: Vec<String>,
}

/// Seconds since 1970 as (year, month, day, hour, minute, second), UTC.
fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

fn unix(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// The 7-byte time of a directory record (UTC).
fn record_time(t: SystemTime) -> [u8; 7] {
    let (y, m, d, h, mi, s) = civil(unix(t));
    [(y.clamp(1900, 2155) - 1900) as u8, m as u8, d as u8, h as u8, mi as u8, s as u8, 0]
}

/// Upper-case letters, digits and underscores only, as plain ISO 9660 wants.
fn clean(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| {
            let u = c.to_ascii_uppercase();
            if u.is_ascii_uppercase() || u.is_ascii_digit() || u == '_' {
                u
            } else {
                '_'
            }
        })
        .take(max)
        .collect()
}

/// An 8.3 name for the plain ISO 9660 tree, different from every other in its folder.
fn short_name(name: &str, dir: bool, used: &mut HashSet<String>, next: &mut HashMap<String, u32>) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && !dir => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    };
    let mut base = clean(stem, 8);
    if base.is_empty() {
        base.push('_');
    }
    let ext = clean(ext, 3);
    let make = |b: &str| if dir { b.to_string() } else { format!("{b}.{ext};1") };
    let first = make(&base);
    if used.insert(first.clone()) {
        return first;
    }
    let n = next.entry(first).or_insert(0);
    loop {
        *n += 1;
        let tail = format!("_{n}");
        let keep = base.len().min(8usize.saturating_sub(tail.len()));
        let cand = make(&format!("{}{tail}", &base[..keep]));
        if used.insert(cand.clone()) {
            return cand;
        }
    }
}

/// Read the folder into a flat list; entry 0 is the folder itself. Folders are listed
/// level by level, which is the order the path tables need.
fn walk(folder: &Path, skip: &[PathBuf], notes: &mut Vec<String>) -> Result<Vec<Node>> {
    let meta = std::fs::metadata(folder).with_context(|| format!("reading {}", folder.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a folder", folder.display());
    }
    let node = |name: String, short: String, path: PathBuf, dir: bool, hidden: bool, size: u64, time: SystemTime, parent: usize| Node {
        name,
        short,
        path,
        dir,
        hidden,
        size,
        time: record_time(time),
        parent,
        children: Vec::new(),
        lba: 0,
        dir_lba: [0; 2],
        dir_len: [0; 2],
    };
    let mut nodes = vec![node(String::new(), String::new(), folder.to_owned(), true, false, 0, meta.modified().unwrap_or(UNIX_EPOCH), 0)];
    let mut queue = VecDeque::from([0usize]);
    let (mut too_long, mut renamed, mut links) = (Vec::new(), 0u64, 0u64);
    while let Some(d) = queue.pop_front() {
        let dir_path = nodes[d].path.clone();
        let mut entries: Vec<_> = std::fs::read_dir(&dir_path)
            .with_context(|| format!("reading {}", dir_path.display()))?
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("reading {}", dir_path.display()))?;
        entries.sort_by_key(|e| e.file_name());
        let (mut shorts, mut next, mut names) = (HashSet::new(), HashMap::new(), HashSet::new());
        for e in entries {
            let path = e.path();
            // The image being written, when it is put inside the folder it is made from.
            if skip.iter().any(|s| s.as_os_str().eq_ignore_ascii_case(path.as_os_str())) {
                continue;
            }
            let own = e.metadata().with_context(|| format!("reading {}", path.display()))?;
            let link = own.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
            // A link is followed to the file it points to. Linked folders are left out:
            // they can lead back into the folder and never end.
            let meta = if link { std::fs::metadata(&path).with_context(|| format!("reading {}", path.display()))? } else { own.clone() };
            if link && meta.is_dir() {
                links += 1;
                continue;
            }
            let real = e.file_name().to_string_lossy().into_owned();
            // ';' ends a name on an ISO (what follows is the version number).
            let name = real.replace(';', "_");
            if name != real {
                renamed += 1;
            }
            if name.encode_utf16().count() > MAX_NAME {
                too_long.push(path.display().to_string());
                continue;
            }
            if !names.insert(name.to_lowercase()) {
                bail!("{} holds two names that would be the same on the disc: {name}", dir_path.display());
            }
            let dir = meta.is_dir();
            let short = short_name(&name, dir, &mut shorts, &mut next);
            let hidden = own.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0;
            let i = nodes.len();
            nodes.push(node(name, short, path, dir, hidden, if dir { 0 } else { meta.len() }, meta.modified().unwrap_or(UNIX_EPOCH), d));
            nodes[d].children.push(i);
            if dir {
                queue.push_back(i);
            }
        }
    }
    if !too_long.is_empty() {
        let more = if too_long.len() > 5 { format!("\n  ...and {} more", too_long.len() - 5) } else { String::new() };
        bail!(
            "{} name(s) are longer than {MAX_NAME} characters, the most an ISO can hold. Shorten them and try again:\n  {}{more}",
            too_long.len(),
            too_long[..too_long.len().min(5)].join("\n  ")
        );
    }
    if renamed > 0 {
        notes.push(format!("{renamed} name(s) contain ';', which an ISO can't hold; it was written as '_'."));
    }
    if links > 0 {
        notes.push(format!("{links} linked folder(s) (junctions or symbolic links) were left out."));
    }
    Ok(nodes)
}

/// Append one directory record. A record never crosses a sector boundary.
fn record(out: &mut Vec<u8>, id: &[u8], lba: u32, len: u32, time: &[u8; 7], flags: u8) {
    let l = 33 + id.len() + (id.len() + 1) % 2;
    if out.len() % SECTOR + l > SECTOR {
        out.resize(out.len().next_multiple_of(SECTOR), 0);
    }
    let s = out.len();
    out.resize(s + l, 0);
    let r = &mut out[s..];
    r[0] = l as u8;
    both32(&mut r[2..10], lba);
    both32(&mut r[10..18], len);
    r[18..25].copy_from_slice(time);
    r[25] = flags;
    both16(&mut r[28..32], 1);
    r[32] = id.len() as u8;
    r[33..33 + id.len()].copy_from_slice(id);
}

fn both32(b: &mut [u8], v: u32) {
    b[..4].copy_from_slice(&v.to_le_bytes());
    b[4..8].copy_from_slice(&v.to_be_bytes());
}

fn both16(b: &mut [u8], v: u16) {
    b[..2].copy_from_slice(&v.to_le_bytes());
    b[2..4].copy_from_slice(&v.to_be_bytes());
}

/// The listing of folder `d` in tree `t`, padded to whole sectors. `sorted` is its
/// children in the order of their identifiers, as the standard asks.
fn listing(nodes: &[Node], d: usize, t: usize, sorted: &[usize], extent: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let (me, up) = (&nodes[d], &nodes[nodes[d].parent]);
    record(&mut out, &[0], me.dir_lba[t], me.dir_len[t], &me.time, 2);
    record(&mut out, &[1], up.dir_lba[t], up.dir_len[t], &up.time, 2);
    for &c in sorted {
        let n = &nodes[c];
        let id = n.id(t);
        let hidden = n.hidden as u8;
        if n.dir {
            record(&mut out, &id, n.dir_lba[t], n.dir_len[t], &n.time, 2 | hidden);
            continue;
        }
        // A file too big for one record gets several; all but the last say "more follows".
        let (mut left, mut lba) = (n.size, n.lba);
        loop {
            let part = left.min(extent);
            left -= part;
            record(&mut out, &id, lba, part as u32, &n.time, hidden | if left > 0 { 0x80 } else { 0 });
            lba = lba.wrapping_add((part / SECTOR as u64) as u32);
            if left == 0 {
                break;
            }
        }
    }
    out.resize(out.len().next_multiple_of(SECTOR), 0);
    out
}

/// A path table for tree `t`: every folder in `order`, with the number of its parent.
fn path_table(nodes: &[Node], t: usize, order: &[usize], number: &HashMap<usize, u16>, big_endian: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for &d in order {
        let id = if d == 0 { vec![0] } else { nodes[d].id(t) };
        let parent = number[&nodes[d].parent];
        out.push(id.len() as u8);
        out.push(0);
        if big_endian {
            out.extend_from_slice(&nodes[d].dir_lba[t].to_be_bytes());
            out.extend_from_slice(&parent.to_be_bytes());
        } else {
            out.extend_from_slice(&nodes[d].dir_lba[t].to_le_bytes());
            out.extend_from_slice(&parent.to_le_bytes());
        }
        out.extend_from_slice(&id);
        if id.len() % 2 == 1 {
            out.push(0);
        }
    }
    out
}

/// Fill a text field of a volume descriptor: ASCII padded with spaces, or (Joliet)
/// big-endian UTF-16 padded with UTF-16 spaces.
fn text(field: &mut [u8], s: &str, joliet: bool) {
    if !joliet {
        field.fill(b' ');
        for (b, c) in field.iter_mut().zip(s.bytes()) {
            *b = c;
        }
        return;
    }
    let mut units = s.encode_utf16().chain(std::iter::repeat(0x20));
    for pair in field.chunks_exact_mut(2) {
        pair.copy_from_slice(&units.next().unwrap_or(0x20).to_be_bytes());
    }
}

struct Volume<'a> {
    label: &'a str,
    sectors: u32,
    table_len: u32,
    table_le: u32,
    table_be: u32,
    root: &'a [u8],
    now: &'a str,
}

fn descriptor(v: &Volume, joliet: bool) -> Vec<u8> {
    let mut d = vec![0u8; SECTOR];
    d[0] = if joliet { 2 } else { 1 };
    d[1..6].copy_from_slice(b"CD001");
    d[6] = 1;
    text(&mut d[8..40], "", joliet);
    if joliet {
        // At most 16 characters, without cutting a character in half.
        let mut label = String::new();
        for c in v.label.chars() {
            if label.encode_utf16().count() + c.len_utf16() > 16 {
                break;
            }
            label.push(c);
        }
        text(&mut d[40..72], &label, true);
        d[88..91].copy_from_slice(b"%/E"); // Joliet, UCS-2 level 3
    } else {
        text(&mut d[40..72], &clean(v.label, 32), false);
    }
    both32(&mut d[80..88], v.sectors);
    both16(&mut d[120..124], 1);
    both16(&mut d[124..128], 1);
    both16(&mut d[128..132], SECTOR as u16);
    both32(&mut d[132..140], v.table_len);
    d[140..144].copy_from_slice(&v.table_le.to_le_bytes());
    d[148..152].copy_from_slice(&v.table_be.to_be_bytes());
    d[156..190].copy_from_slice(&v.root[..34]);
    text(&mut d[190..318], "", joliet);
    text(&mut d[318..446], "", joliet);
    text(&mut d[446..574], "", joliet);
    text(&mut d[574..702], "RUSTIMGMOUNT", joliet);
    text(&mut d[702..738], "", joliet);
    text(&mut d[739..775], "", joliet);
    text(&mut d[776..812], "", joliet);
    if !joliet {
        d[738] = b' ';
        d[775] = b' ';
        d[812] = b' ';
    }
    for (at, set) in [(813, true), (830, true), (847, false), (864, false)] {
        d[at..at + 16].copy_from_slice(if set { v.now.as_bytes() } else { b"0000000000000000" });
    }
    d[881] = 1;
    d
}

/// Write an ISO image of `folder` to `out`. `label` is the disc's name (default: the
/// folder's name).
pub fn create(folder: &Path, out: &Path, label: Option<&str>, overwrite: bool) -> Result<Made> {
    let mut job = Job::new(overwrite, Vec::new());
    let tmp = job.add(out)?;
    let mut notes = Vec::new();
    let mut nodes = walk(folder, &[out.to_owned(), tmp.clone()], &mut notes)?;
    let extent = max_extent();

    // Each tree lists a folder's children in the order of that tree's names, and numbers
    // the folders level by level in the same order.
    let mut sorted: [Vec<Vec<usize>>; 2] = [Vec::new(), Vec::new()];
    let mut order: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
    let mut number: [HashMap<usize, u16>; 2] = [HashMap::new(), HashMap::new()];
    for t in 0..2 {
        sorted[t] = nodes
            .iter()
            .map(|n| {
                let mut c = n.children.clone();
                c.sort_by_cached_key(|&i| nodes[i].id(t));
                c
            })
            .collect();
        let mut queue = VecDeque::from([0usize]);
        while let Some(d) = queue.pop_front() {
            if order[t].len() >= 0xFFFF {
                bail!("the folder holds more than 65,535 folders, the most an ISO can hold");
            }
            number[t].insert(d, order[t].len() as u16 + 1);
            order[t].push(d);
            queue.extend(sorted[t][d].iter().copied().filter(|&c| nodes[c].dir));
        }
    }

    // Lay out the image: descriptors, path tables, folder listings, then the files.
    let mut at: u64 = 19;
    let mut tables = [(0u32, 0u32, 0u32); 2]; // (length, little-endian copy, big-endian copy)
    for t in 0..2 {
        let len = path_table(&nodes, t, &order[t], &number[t], false).len() as u64;
        let sectors = len.div_ceil(SECTOR as u64);
        tables[t] = (len as u32, at as u32, (at + sectors) as u32);
        at += sectors * 2;
    }
    for t in 0..2 {
        for &d in &order[t] {
            let len = listing(&nodes, d, t, &sorted[t][d], extent).len() as u64;
            if at + len / SECTOR as u64 > u32::MAX as u64 {
                bail!("the folder is too big for an ISO image");
            }
            nodes[d].dir_lba[t] = at as u32;
            nodes[d].dir_len[t] = len as u32;
            at += len / SECTOR as u64;
        }
    }
    let (mut files, mut folders) = (0u64, 0u64);
    for n in nodes.iter_mut().skip(1) {
        if n.dir {
            folders += 1;
            continue;
        }
        files += 1;
        if n.size > 0 {
            n.lba = at as u32;
            at += n.size.div_ceil(SECTOR as u64);
        }
    }
    if at > u32::MAX as u64 {
        bail!("the folder is too big for an ISO image (the limit is about 8 TiB)");
    }
    let total = at * SECTOR as u64;
    // Find out now, not most of the way through, that the drive is too full.
    crate::convert::check_free_space(out.parent().unwrap_or(Path::new(".")), total)?;

    let name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "DISC".into());
    let label = label.unwrap_or(&name);
    let (y, m, d, h, mi, s) = civil(unix(SystemTime::now()));
    let now = format!("{:04}{m:02}{d:02}{h:02}{mi:02}{s:02}00", y.clamp(1, 9999));

    let mut w = BufWriter::with_capacity(2 << 20, File::create(&tmp).with_context(|| format!("creating {}", out.display()))?);
    let mut p = Progress::new("Creating ISO", total);
    let mut pos: u64 = 0;
    let put = |w: &mut BufWriter<File>, pos: &mut u64, lba: u32, data: &[u8]| -> Result<()> {
        let start = lba as u64 * SECTOR as u64;
        if start < *pos {
            return Err(anyhow!("internal error: the image layout overlaps at sector {lba}"));
        }
        std::io::copy(&mut std::io::repeat(0).take(start - *pos), w)?;
        w.write_all(data)?;
        *pos = start + data.len() as u64;
        Ok(())
    };
    for t in 0..2 {
        let mut root = Vec::new();
        record(&mut root, &[0], nodes[0].dir_lba[t], nodes[0].dir_len[t], &nodes[0].time, 2);
        let v = Volume { label, sectors: at as u32, table_len: tables[t].0, table_le: tables[t].1, table_be: tables[t].2, root: &root, now: &now };
        put(&mut w, &mut pos, 16 + t as u32, &descriptor(&v, t == 1))?;
    }
    let mut end = vec![0u8; SECTOR];
    end[0] = 255;
    end[1..6].copy_from_slice(b"CD001");
    end[6] = 1;
    put(&mut w, &mut pos, 18, &end)?;
    for t in 0..2 {
        put(&mut w, &mut pos, tables[t].1, &path_table(&nodes, t, &order[t], &number[t], false))?;
        put(&mut w, &mut pos, tables[t].2, &path_table(&nodes, t, &order[t], &number[t], true))?;
    }
    for t in 0..2 {
        for &d in &order[t] {
            put(&mut w, &mut pos, nodes[d].dir_lba[t], &listing(&nodes, d, t, &sorted[t][d], extent))?;
        }
    }
    let mut buf = vec![0u8; 2 << 20];
    for n in nodes.iter().filter(|n| !n.dir && n.size > 0) {
        put(&mut w, &mut pos, n.lba, &[])?;
        let mut f = File::open(&n.path).with_context(|| format!("reading {}", n.path.display()))?;
        let mut left = n.size;
        while left > 0 {
            let want = left.min(buf.len() as u64) as usize;
            let got = f.read(&mut buf[..want]).with_context(|| format!("reading {}", n.path.display()))?;
            if got == 0 {
                bail!("{} changed while it was being read; try again", n.path.display());
            }
            w.write_all(&buf[..got])?;
            left -= got as u64;
            pos += got as u64;
            p.update(pos);
        }
    }
    std::io::copy(&mut std::io::repeat(0).take(total - pos), &mut w)?;
    w.flush()?;
    drop(w);
    p.finish();
    job.commit()?;
    Ok(Made { files, folders, bytes: total, notes })
}
