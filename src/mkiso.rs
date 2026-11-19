//! Make an ISO image from the files in a folder.
//!
//! The image is ISO 9660 with a Joliet tree, which is what Windows, macOS and Linux all
//! read: Joliet carries the real names (Unicode, up to 103 characters), and the plain
//! ISO 9660 tree beside it carries shortened 8.3 names for anything older. Both trees
//! point at the same file data. Files of 4 GiB and more are recorded in several extents
//! (ISO 9660 "level 3"), which Windows has read since Vista.
//!
//! When a name is too long for Joliet, the image gets a third tree as well: the UDF file
//! system (revision 1.02, as on DVDs), which holds names of up to 254 characters and is
//! what current systems read first. The Joliet tree then carries a shortened name.

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
    /// The file's own name, which is what the UDF tree records.
    real: String,
    /// The name in the Joliet tree: the same, unless that can't hold it.
    name: String,
    /// The name in the plain ISO 9660 tree ("README.TXT;1").
    short: String,
    path: PathBuf,
    dir: bool,
    hidden: bool,
    size: u64,
    time: [u8; 7],
    /// The same time, in seconds since 1970.
    mtime: i64,
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

/// The longest name UDF can record: 254 characters when all of them are Latin-1, else 127.
fn udf_name_fits(name: &str) -> bool {
    if name.chars().all(|c| (c as u32) < 256) {
        name.chars().count() <= 254
    } else {
        name.encode_utf16().count() <= 127
    }
}

/// A Joliet name for a file whose real name is too long for Joliet: the start of the name,
/// "~1" (or the first number that is free in its folder) and the extension.
fn shorten(name: &str, used: &HashSet<String>) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && name[i..].encode_utf16().count() <= 20 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let mut n = 1;
    loop {
        let tail = format!("~{n}{ext}");
        let room = MAX_NAME - tail.encode_utf16().count();
        let mut cand = String::new();
        for c in stem.chars() {
            if cand.encode_utf16().count() + c.len_utf16() > room {
                break;
            }
            cand.push(c);
        }
        cand.push_str(&tail);
        if !used.contains(&cand.to_lowercase()) {
            return cand;
        }
        n += 1;
    }
}

/// What reading the folder found, besides the files.
struct Found {
    nodes: Vec<Node>,
    /// A name is too long for Joliet, so the image needs the UDF file system as well.
    long_names: bool,
}

fn list(what: &str, paths: &[String]) -> String {
    let more = if paths.len() > 5 { format!("\n  ...and {} more", paths.len() - 5) } else { String::new() };
    format!("{} {what}:\n  {}{more}", paths.len(), paths[..paths.len().min(5)].join("\n  "))
}

/// Read the folder into a flat list; entry 0 is the folder itself. Folders are listed
/// level by level, which is the order the path tables need.
fn walk(folder: &Path, skip: &[PathBuf], skip_unreadable: bool, notes: &mut Vec<String>) -> Result<Found> {
    let meta = std::fs::metadata(folder).with_context(|| format!("reading {}", folder.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a folder", folder.display());
    }
    // A whole drive: Windows' own folders on it are not part of what the user keeps there.
    let drive = folder.parent().is_none();
    let node = |real: String, name: String, short: String, path: PathBuf, dir: bool, hidden: bool, size: u64, time: SystemTime, parent: usize| Node {
        real,
        name,
        short,
        path,
        dir,
        hidden,
        size,
        time: record_time(time),
        mtime: unix(time),
        parent,
        children: Vec::new(),
        lba: 0,
        dir_lba: [0; 2],
        dir_len: [0; 2],
    };
    let mut nodes = vec![node(String::new(), String::new(), String::new(), folder.to_owned(), true, false, 0, meta.modified().unwrap_or(UNIX_EPOCH), 0)];
    let mut queue = VecDeque::from([0usize]);
    let (mut too_long, mut unreadable, mut renamed, mut links, mut long_names) = (Vec::new(), Vec::new(), 0u64, 0u64, false);
    while let Some(d) = queue.pop_front() {
        let dir_path = nodes[d].path.clone();
        let mut entries: Vec<_> = std::fs::read_dir(&dir_path)
            .with_context(|| format!("reading {}", dir_path.display()))?
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("reading {}", dir_path.display()))?;
        entries.sort_by_key(|e| e.file_name());
        let (mut shorts, mut next, mut names, mut assigned) = (HashSet::new(), HashMap::new(), HashSet::new(), HashSet::new());
        // Names that fit as they are come first, so a shortened name never takes one of theirs.
        for e in &entries {
            let name = e.file_name().to_string_lossy().replace(';', "_");
            if name.encode_utf16().count() <= MAX_NAME {
                names.insert(name.to_lowercase());
            }
        }
        for e in entries {
            let path = e.path();
            // The image being written, when it is put inside the folder it is made from.
            if skip.iter().any(|s| s.as_os_str().eq_ignore_ascii_case(path.as_os_str())) {
                continue;
            }
            let real = e.file_name().to_string_lossy().into_owned();
            if drive && d == 0 && (real.eq_ignore_ascii_case("System Volume Information") || real.eq_ignore_ascii_case("$RECYCLE.BIN")) {
                continue;
            }
            // What can't be read is found now, before anything is written.
            let read = e.metadata().and_then(|own| {
                let link = own.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
                // A link is followed to the file it points to.
                let meta = if link { std::fs::metadata(&path)? } else { own.clone() };
                if !(link && meta.is_dir()) {
                    if meta.is_dir() {
                        std::fs::read_dir(&path)?;
                    } else {
                        File::open(&path)?;
                    }
                }
                Ok((own, meta, link))
            });
            let (own, meta, link): (std::fs::Metadata, std::fs::Metadata, bool) = match read {
                Ok(r) => r,
                Err(err) => {
                    unreadable.push(format!("{} ({err})", path.display()));
                    continue;
                }
            };
            // Linked folders are left out: they can lead back into the folder and never end.
            if link && meta.is_dir() {
                links += 1;
                continue;
            }
            if !udf_name_fits(&real) {
                too_long.push(path.display().to_string());
                continue;
            }
            // ';' ends a name in the ISO 9660 and Joliet trees (what follows is the version number).
            let mut name = real.replace(';', "_");
            if name.encode_utf16().count() > MAX_NAME {
                // Too long for Joliet: the whole name goes in the UDF tree.
                name = shorten(&name, &names);
                names.insert(name.to_lowercase());
                long_names = true;
            } else if name != real {
                renamed += 1;
            }
            if !assigned.insert(name.to_lowercase()) {
                bail!("{} holds two names that would be the same on the disc: {name}", dir_path.display());
            }
            let dir = meta.is_dir();
            let short = short_name(&name, dir, &mut shorts, &mut next);
            let hidden = own.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0;
            let i = nodes.len();
            nodes.push(node(real, name, short, path, dir, hidden, if dir { 0 } else { meta.len() }, meta.modified().unwrap_or(UNIX_EPOCH), d));
            nodes[d].children.push(i);
            if dir {
                queue.push_back(i);
            }
        }
    }
    if !too_long.is_empty() {
        bail!(
            "{}\nA name on a disc image can be at most 254 characters, or 127 when it has letters outside Western European alphabets. Shorten them and try again.",
            list("name(s) are too long for a disc image", &too_long)
        );
    }
    if !unreadable.is_empty() {
        if !skip_unreadable {
            bail!(
                "{}\nNothing was written. Close the program using them, or leave them out with --skip-unreadable (\"Leave out files that can't be read\" in the dialog).",
                list("file(s) or folder(s) can't be read", &unreadable)
            );
        }
        notes.push(list("file(s) or folder(s) couldn't be read and were left out", &unreadable));
    }
    if renamed > 0 && !long_names {
        notes.push(format!("{renamed} name(s) contain ';', which an ISO can't hold; it was written as '_'."));
    }
    if links > 0 {
        notes.push(format!("{links} linked folder(s) (junctions or symbolic links) were left out."));
    }
    Ok(Found { nodes, long_names })
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

// ---------------------------------------------------------------- UDF

/// Where the UDF partition starts. Sector 256 before it holds the anchor that readers
/// look for; the sectors before that hold the volume descriptors.
const UDF_START: u64 = 257;
/// The most one allocation descriptor of a UDF file can cover, in whole sectors.
const UDF_EXTENT: u64 = 0x3FFF_F800;

fn crc16(data: &[u8]) -> u16 {
    let mut c: u16 = 0;
    for &b in data {
        c ^= (b as u16) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 { (c << 1) ^ 0x1021 } else { c << 1 };
        }
    }
    c
}

/// Fill in the 16-byte tag at the start of a finished UDF descriptor.
fn tag(d: &mut [u8], id: u16, location: u32) {
    d[0..2].copy_from_slice(&id.to_le_bytes());
    d[2..4].copy_from_slice(&2u16.to_le_bytes());
    let crc = crc16(&d[16..]);
    d[8..10].copy_from_slice(&crc.to_le_bytes());
    let len = (d.len() - 16) as u16;
    d[10..12].copy_from_slice(&len.to_le_bytes());
    d[12..16].copy_from_slice(&location.to_le_bytes());
    d[4] = 0;
    d[4] = d[..16].iter().fold(0u8, |a, &b| a.wrapping_add(b));
}

/// A name in UDF's "compressed Unicode": a byte saying 8 or 16 bits per character, then
/// the characters. At most `max` bytes, without cutting a character in half.
fn d_chars(s: &str, max: usize) -> Vec<u8> {
    let narrow = s.chars().all(|c| (c as u32) < 256);
    let mut out = vec![if narrow { 8 } else { 16 }];
    for c in s.chars() {
        let mut units = [0u16; 2];
        let bytes: Vec<u8> = if narrow { vec![c as u8] } else { c.encode_utf16(&mut units).iter().flat_map(|u| u.to_be_bytes()).collect() };
        if out.len() + bytes.len() > max {
            break;
        }
        out.extend(bytes);
    }
    out
}

/// A fixed-size text field: the name, zero padding, and its length in the last byte.
fn dstring(field: &mut [u8], s: &str) {
    if s.is_empty() {
        return;
    }
    let chars = d_chars(s, field.len() - 1);
    field[..chars.len()].copy_from_slice(&chars);
    field[field.len() - 1] = chars.len() as u8;
}

fn charspec(field: &mut [u8]) {
    field[1..24].copy_from_slice(b"OSTA Compressed Unicode");
}

fn regid(field: &mut [u8], id: &[u8], suffix: &[u8]) {
    field[1..1 + id.len()].copy_from_slice(id);
    field[24..24 + suffix.len()].copy_from_slice(suffix);
}

/// "Written by rustimgmount".
fn implementation(field: &mut [u8]) {
    regid(field, b"*rustimgmount", &[]);
}

/// The domain every UDF structure belongs to; the suffix says UDF revision 1.02.
fn domain(field: &mut [u8]) {
    regid(field, b"*OSTA UDF Compliant", &[0x02, 0x01]);
}

fn timestamp(secs: i64) -> [u8; 12] {
    let (y, m, d, h, mi, s) = civil(secs);
    let mut t = [0u8; 12];
    t[0..2].copy_from_slice(&0x1000u16.to_le_bytes()); // local time type, offset 0: UTC
    t[2..4].copy_from_slice(&(y.clamp(1, 9999) as i16).to_le_bytes());
    t[4..9].copy_from_slice(&[m as u8, d as u8, h as u8, mi as u8, s as u8]);
    t
}

/// A reference to `len` bytes at block `lbn` of the partition.
fn long_ad(field: &mut [u8], len: u32, lbn: u32) {
    field[0..4].copy_from_slice(&len.to_le_bytes());
    field[4..8].copy_from_slice(&lbn.to_le_bytes());
}

struct Udf<'a> {
    label: &'a str,
    now: i64,
    /// Sectors in the partition, which runs from UDF_START to just before the last sector.
    blocks: u32,
    files: u32,
    folders: u32,
    next_id: u64,
}

/// The descriptors of the volume descriptor sequence, for the copy starting at `sector`.
fn udf_volume(u: &Udf, sector: u32) -> Vec<Vec<u8>> {
    let mut primary = vec![0u8; 512];
    dstring(&mut primary[24..56], u.label);
    primary[56..58].copy_from_slice(&1u16.to_le_bytes());
    primary[58..60].copy_from_slice(&1u16.to_le_bytes());
    primary[60..62].copy_from_slice(&2u16.to_le_bytes());
    primary[62..64].copy_from_slice(&2u16.to_le_bytes());
    primary[64..68].copy_from_slice(&1u32.to_le_bytes());
    primary[68..72].copy_from_slice(&1u32.to_le_bytes());
    dstring(&mut primary[72..200], &format!("{:016X}{}", u.now, u.label));
    charspec(&mut primary[200..264]);
    charspec(&mut primary[264..328]);
    implementation(&mut primary[344..376]);
    primary[376..388].copy_from_slice(&timestamp(u.now));
    implementation(&mut primary[388..420]);

    let mut info = vec![0u8; 512];
    info[16..20].copy_from_slice(&1u32.to_le_bytes());
    regid(&mut info[20..52], b"*UDF LV Info", &[0x02, 0x01]);
    charspec(&mut info[52..116]);
    dstring(&mut info[116..244], u.label);
    implementation(&mut info[352..384]);

    let mut partition = vec![0u8; 512];
    partition[16..20].copy_from_slice(&2u32.to_le_bytes());
    partition[20..22].copy_from_slice(&1u16.to_le_bytes()); // space is allocated
    partition[24] = 2;
    partition[25..31].copy_from_slice(b"+NSR02");
    partition[184..188].copy_from_slice(&1u32.to_le_bytes()); // read-only
    partition[188..192].copy_from_slice(&(UDF_START as u32).to_le_bytes());
    partition[192..196].copy_from_slice(&u.blocks.to_le_bytes());
    implementation(&mut partition[196..228]);

    let mut logical = vec![0u8; 446];
    logical[16..20].copy_from_slice(&3u32.to_le_bytes());
    charspec(&mut logical[20..84]);
    dstring(&mut logical[84..212], u.label);
    logical[212..216].copy_from_slice(&(SECTOR as u32).to_le_bytes());
    domain(&mut logical[216..248]);
    long_ad(&mut logical[248..264], SECTOR as u32, 0); // the file set descriptor, block 0
    logical[264..268].copy_from_slice(&6u32.to_le_bytes());
    logical[268..272].copy_from_slice(&1u32.to_le_bytes());
    implementation(&mut logical[272..304]);
    logical[432..436].copy_from_slice(&(2 * SECTOR as u32).to_le_bytes()); // integrity descriptor
    logical[436..440].copy_from_slice(&64u32.to_le_bytes());
    logical[440..446].copy_from_slice(&[1, 6, 1, 0, 0, 0]); // one map: volume 1, partition 0

    let mut unallocated = vec![0u8; 24];
    unallocated[16..20].copy_from_slice(&4u32.to_le_bytes());

    let mut all = vec![primary, info, partition, logical, unallocated, vec![0u8; 512]];
    for (i, (d, id)) in all.iter_mut().zip([1u16, 4, 5, 6, 7, 8]).enumerate() {
        tag(d, id, sector + i as u32);
    }
    all
}

/// The integrity descriptor: says the volume was closed properly, and how much it holds.
fn udf_integrity(u: &Udf) -> Vec<u8> {
    let mut d = vec![0u8; 134];
    d[16..28].copy_from_slice(&timestamp(u.now));
    d[28..32].copy_from_slice(&1u32.to_le_bytes()); // closed
    d[40..48].copy_from_slice(&u.next_id.to_le_bytes());
    d[72..76].copy_from_slice(&1u32.to_le_bytes());
    d[76..80].copy_from_slice(&46u32.to_le_bytes());
    d[84..88].copy_from_slice(&u.blocks.to_le_bytes());
    implementation(&mut d[88..120]);
    d[120..124].copy_from_slice(&u.files.to_le_bytes());
    d[124..128].copy_from_slice(&u.folders.to_le_bytes());
    for at in [128, 130, 132] {
        d[at..at + 2].copy_from_slice(&0x0102u16.to_le_bytes());
    }
    tag(&mut d, 9, 64);
    d
}

fn udf_anchor(sector: u32) -> Vec<u8> {
    let mut d = vec![0u8; 512];
    for (at, start) in [(16, 32u32), (24, 48)] {
        d[at..at + 4].copy_from_slice(&(16 * SECTOR as u32).to_le_bytes());
        d[at + 4..at + 8].copy_from_slice(&start.to_le_bytes());
    }
    tag(&mut d, 2, sector);
    d
}

fn udf_file_set(u: &Udf) -> Vec<u8> {
    let mut d = vec![0u8; 512];
    d[16..28].copy_from_slice(&timestamp(u.now));
    d[28..30].copy_from_slice(&3u16.to_le_bytes());
    d[30..32].copy_from_slice(&3u16.to_le_bytes());
    d[32..36].copy_from_slice(&1u32.to_le_bytes());
    d[36..40].copy_from_slice(&1u32.to_le_bytes());
    charspec(&mut d[48..112]);
    dstring(&mut d[112..240], u.label);
    charspec(&mut d[240..304]);
    dstring(&mut d[304..336], u.label);
    long_ad(&mut d[400..416], SECTOR as u32, 1); // the root folder's entry, block 1
    domain(&mut d[416..448]);
    tag(&mut d, 256, 0);
    d
}

/// A file entry: what kind of thing it is, its size and dates, and where its data is.
/// `data` is (first block, length in bytes); a big file is described in several pieces.
fn udf_entry(lbn: u32, dir: bool, links: u16, mtime: i64, unique: u64, data: (u32, u64)) -> Result<Vec<u8>> {
    let (mut block, mut left) = data;
    let mut pieces = Vec::new();
    let extent = max_extent().min(UDF_EXTENT); // (smaller only under the tests' hook)
    while left > 0 {
        let part = left.min(extent);
        pieces.push((part as u32, block));
        block = block.wrapping_add((part / SECTOR as u64) as u32);
        left -= part;
    }
    if 176 + pieces.len() * 8 > SECTOR {
        bail!("a file is too big for a disc image (the limit is about 230 GiB)");
    }
    let mut d = vec![0u8; 176 + pieces.len() * 8];
    d[20..22].copy_from_slice(&4u16.to_le_bytes());
    d[24..26].copy_from_slice(&1u16.to_le_bytes());
    d[27] = if dir { 4 } else { 5 };
    d[36..44].fill(0xFF); // no owner or group
    d[44..48].copy_from_slice(&0x14A5u32.to_le_bytes()); // everyone may read and open
    d[48..50].copy_from_slice(&links.to_le_bytes());
    d[56..64].copy_from_slice(&data.1.to_le_bytes());
    d[64..72].copy_from_slice(&data.1.div_ceil(SECTOR as u64).to_le_bytes());
    for at in [72, 84, 96] {
        d[at..at + 12].copy_from_slice(&timestamp(mtime));
    }
    d[108..112].copy_from_slice(&1u32.to_le_bytes());
    implementation(&mut d[128..160]);
    d[160..168].copy_from_slice(&unique.to_le_bytes());
    d[172..176].copy_from_slice(&((pieces.len() * 8) as u32).to_le_bytes());
    for (i, (len, at)) in pieces.iter().enumerate() {
        d[176 + i * 8..180 + i * 8].copy_from_slice(&len.to_le_bytes());
        d[180 + i * 8..184 + i * 8].copy_from_slice(&at.to_le_bytes());
    }
    tag(&mut d, 261, lbn);
    Ok(d)
}

/// Block of node `i`'s file entry: they follow the file set descriptor, in node order.
fn udf_entry_block(i: usize) -> u32 {
    1 + i as u32
}

fn udf_unique(i: usize) -> u64 {
    if i == 0 {
        0
    } else {
        16 + i as u64
    }
}

/// The contents of folder `d`: one record for its parent, then one per child. `at` is
/// the block the listing starts at (records carry the block they begin in).
fn udf_listing(nodes: &[Node], d: usize, at: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut add = |name: &str, flags: u8, target: usize| {
        let id = if name.is_empty() { Vec::new() } else { d_chars(name, 255) };
        let start = out.len();
        out.resize(start + ((38 + id.len() + 3) & !3), 0);
        let r = &mut out[start..];
        r[16..18].copy_from_slice(&1u16.to_le_bytes());
        r[18] = flags;
        r[19] = id.len() as u8;
        long_ad(&mut r[20..36], SECTOR as u32, udf_entry_block(target));
        r[32..36].copy_from_slice(&(udf_unique(target) as u32).to_le_bytes());
        r[38..38 + id.len()].copy_from_slice(&id);
        tag(r, 257, at + (start / SECTOR) as u32);
    };
    add("", 0x0A, nodes[d].parent);
    for &c in &nodes[d].children {
        let n = &nodes[c];
        add(&n.real, n.hidden as u8 | if n.dir { 2 } else { 0 }, c);
    }
    out
}

// ---------------------------------------------------------------- writing the image

/// Choices for `write`.
#[derive(Default)]
pub struct Options<'a> {
    /// The disc's name (default: the folder's name).
    pub label: Option<&'a str>,
    /// Leave out files that can't be read, where otherwise nothing is written.
    pub skip_unreadable: bool,
    /// Write the UDF file system even when no name needs it.
    pub udf: bool,
}

/// Write an ISO image of `folder` to the file `out` (which is created or replaced).
/// `skip` are files to leave out of the image: the image itself, when it is being
/// written inside the folder.
pub fn write(folder: &Path, out: &Path, skip: &[PathBuf], opts: &Options) -> Result<Made> {
    let mut notes = Vec::new();
    let Found { mut nodes, long_names } = walk(folder, skip, opts.skip_unreadable, &mut notes)?;
    let udf = opts.udf || long_names;
    if long_names {
        notes.push("Some names are longer than 103 characters, so the image also has the UDF file system, which holds them whole. Windows, macOS and Linux read that; a program that only reads the older ISO 9660 names sees them shortened.".into());
    }
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

    // Lay out the image: descriptors, (UDF's own structures,) path tables, folder
    // listings, then the files.
    let mut at: u64 = if udf { UDF_START + 1 + nodes.len() as u64 } else { 19 };
    // UDF: where each folder's listing is, as (sector, length in bytes).
    let mut udf_dirs: HashMap<usize, (u64, u64)> = HashMap::new();
    if udf {
        for d in (0..nodes.len()).filter(|&d| nodes[d].dir) {
            let len = udf_listing(&nodes, d, 0).len() as u64;
            udf_dirs.insert(d, (at, len));
            at += len.div_ceil(SECTOR as u64);
        }
    }
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
    // UDF readers look for a second anchor in the last sector.
    let last = at;
    if udf {
        at += 1;
    }
    if at > u32::MAX as u64 {
        bail!("the folder is too big for an ISO image (the limit is about 8 TiB)");
    }
    let total = at * SECTOR as u64;
    // Find out now, not most of the way through, that the drive is too full.
    crate::convert::check_free_space(out.parent().unwrap_or(Path::new(".")), total)?;

    let name = match folder.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => crate::vdisk::volume_label(folder).filter(|l| !l.is_empty()).unwrap_or_else(|| "DISC".into()),
    };
    let label = opts.label.unwrap_or(&name);
    let now_secs = unix(SystemTime::now());
    let (y, m, d, h, mi, s) = civil(now_secs);
    let now = format!("{:04}{m:02}{d:02}{h:02}{mi:02}{s:02}00", y.clamp(1, 9999));

    let mut w = BufWriter::with_capacity(2 << 20, File::create(out).with_context(|| format!("creating {}", out.display()))?);
    let mut p = Progress::new("Creating ISO", total);
    let mut pos: u64 = 0;
    let put = |w: &mut BufWriter<File>, pos: &mut u64, lba: u64, data: &[u8]| -> Result<()> {
        let start = lba * SECTOR as u64;
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
        put(&mut w, &mut pos, 16 + t as u64, &descriptor(&v, t == 1))?;
    }
    let mut end = vec![0u8; SECTOR];
    end[0] = 255;
    end[1..6].copy_from_slice(b"CD001");
    end[6] = 1;
    put(&mut w, &mut pos, 18, &end)?;
    if udf {
        // "A UDF volume follows", for readers that go through the ISO 9660 descriptors.
        for (i, id) in [b"BEA01", b"NSR02", b"TEA01"].iter().enumerate() {
            let mut d = vec![0u8; SECTOR];
            d[1..6].copy_from_slice(*id);
            d[6] = 1;
            put(&mut w, &mut pos, 19 + i as u64, &d)?;
        }
        let u = Udf { label, now: now_secs, blocks: (last - UDF_START) as u32, files: files as u32, folders: folders as u32 + 1, next_id: udf_unique(nodes.len()) };
        for copy in [32u32, 48] {
            for (i, d) in udf_volume(&u, copy).iter().enumerate() {
                put(&mut w, &mut pos, copy as u64 + i as u64, d)?;
            }
        }
        put(&mut w, &mut pos, 64, &udf_integrity(&u))?;
        let mut stop = vec![0u8; 512];
        tag(&mut stop, 8, 65);
        put(&mut w, &mut pos, 65, &stop)?;
        put(&mut w, &mut pos, 256, &udf_anchor(256))?;
        put(&mut w, &mut pos, UDF_START, &udf_file_set(&u))?;
        let block = |sector: u64| (sector - UDF_START) as u32;
        for (i, n) in nodes.iter().enumerate() {
            let entry = if n.dir {
                let (sector, len) = udf_dirs[&i];
                let subfolders = n.children.iter().filter(|&&c| nodes[c].dir).count();
                udf_entry(udf_entry_block(i), true, 1 + subfolders.min(0xFFFE) as u16, n.mtime, udf_unique(i), (block(sector), len))?
            } else {
                let first = if n.size > 0 { block(n.lba as u64) } else { 0 };
                udf_entry(udf_entry_block(i), false, 1, n.mtime, udf_unique(i), (first, n.size))?
            };
            put(&mut w, &mut pos, UDF_START + udf_entry_block(i) as u64, &entry)?;
        }
        for d in (0..nodes.len()).filter(|&d| nodes[d].dir) {
            let sector = udf_dirs[&d].0;
            put(&mut w, &mut pos, sector, &udf_listing(&nodes, d, block(sector)))?;
        }
    }
    for t in 0..2 {
        put(&mut w, &mut pos, tables[t].1 as u64, &path_table(&nodes, t, &order[t], &number[t], false))?;
        put(&mut w, &mut pos, tables[t].2 as u64, &path_table(&nodes, t, &order[t], &number[t], true))?;
    }
    for t in 0..2 {
        for &d in &order[t] {
            put(&mut w, &mut pos, nodes[d].dir_lba[t] as u64, &listing(&nodes, d, t, &sorted[t][d], extent))?;
        }
    }
    let mut buf = vec![0u8; 2 << 20];
    for n in nodes.iter().filter(|n| !n.dir && n.size > 0) {
        put(&mut w, &mut pos, n.lba as u64, &[])?;
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
    if udf {
        put(&mut w, &mut pos, last, &udf_anchor(last as u32))?;
    }
    std::io::copy(&mut std::io::repeat(0).take(total - pos), &mut w)?;
    w.flush()?;
    drop(w);
    p.finish();
    Ok(Made { files, folders, bytes: total, notes })
}
