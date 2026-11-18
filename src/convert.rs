//! Writers that turn a `Source` into something Windows can attach natively:
//! a plain .iso for optical media, or a dynamic (sparse) .vhd for disks.

use crate::source::Source;
use crate::util::{random_bytes, Progress};
use anyhow::{bail, Result};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::os::windows::fs::FileExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const CHUNK: usize = 2 << 20;

pub fn write_iso(src: &mut dyn Source, out: &Path, quiet: bool) -> Result<()> {
    let total = src.len();
    let mut w = BufWriter::with_capacity(CHUNK, File::create(out)?);
    let mut buf = vec![0u8; CHUNK];
    let mut p = Progress::new("Converting to ISO", total);
    if quiet {
        p = p.quiet();
    }
    let mut pos = 0;
    while pos < total {
        let n = ((total - pos) as usize).min(CHUNK);
        src.read_at(pos, &mut buf[..n])?;
        w.write_all(&buf[..n])?;
        pos += n as u64;
        p.update(pos);
    }
    w.flush()?;
    p.finish();
    Ok(())
}

// ---------------------------------------------------------------- VHD

const VHD_BLOCK: u64 = 2 << 20;
/// Largest size the VHD format supports (2040 GiB).
pub const VHD_MAX: u64 = 2040 << 30;

fn checksum(b: &[u8]) -> u32 {
    !b.iter().fold(0u32, |a, &x| a.wrapping_add(x as u32))
}

/// CHS geometry per the VHD specification, appendix "CHS calculation".
fn geometry(size: u64) -> (u16, u8, u8) {
    let mut total = size / 512;
    if total > 65535 * 16 * 255 {
        total = 65535 * 16 * 255;
    }
    let (spt, heads, cth);
    if total >= 65535 * 16 * 63 {
        spt = 255;
        heads = 16;
        cth = total / spt;
    } else {
        let mut s = 17;
        let mut c = total / s;
        let mut h = (c + 1023) / 1024;
        if h < 4 {
            h = 4;
        }
        if c >= h * 1024 || h > 16 {
            s = 31;
            h = 16;
            c = total / s;
        }
        if c >= h * 1024 {
            s = 63;
            h = 16;
            c = total / s;
        }
        spt = s;
        heads = h;
        cth = c;
    }
    ((cth / heads) as u16, heads as u8, spt as u8)
}

pub(crate) fn vhd_footer(size: u64, dynamic: bool) -> [u8; 512] {
    let mut f = [0u8; 512];
    f[0..8].copy_from_slice(b"conectix");
    f[8..12].copy_from_slice(&2u32.to_be_bytes()); // features: reserved bit
    f[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    let data_off: u64 = if dynamic { 512 } else { u64::MAX };
    f[16..24].copy_from_slice(&data_off.to_be_bytes());
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0).saturating_sub(946_684_800) as u32;
    f[24..28].copy_from_slice(&ts.to_be_bytes());
    f[28..32].copy_from_slice(b"imgm");
    f[32..36].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    f[36..40].copy_from_slice(b"Wi2k");
    f[40..48].copy_from_slice(&size.to_be_bytes());
    f[48..56].copy_from_slice(&size.to_be_bytes());
    let (c, h, s) = geometry(size);
    f[56..58].copy_from_slice(&c.to_be_bytes());
    f[58] = h;
    f[59] = s;
    f[60..64].copy_from_slice(&(if dynamic { 3u32 } else { 2u32 }).to_be_bytes());
    f[68..84].copy_from_slice(&random_bytes::<16>());
    let cs = checksum(&f);
    f[64..68].copy_from_slice(&cs.to_be_bytes());
    f
}

/// Write a dynamic VHD. All-zero 2 MiB blocks are left unallocated, so sparse
/// virtual machine disks stay small.
pub fn write_vhd(src: &mut dyn Source, out: &Path, quiet: bool) -> Result<()> {
    let size = src.len().div_ceil(512) * 512;
    if size > VHD_MAX {
        bail!("disk is {} bytes; VHD supports at most 2040 GiB", size);
    }
    let blocks = size.div_ceil(VHD_BLOCK);
    let footer = vhd_footer(size, true);

    // Dynamic disk header.
    let bat_off: u64 = 512 + 1024;
    let mut dh = [0u8; 1024];
    dh[0..8].copy_from_slice(b"cxsparse");
    dh[8..16].copy_from_slice(&u64::MAX.to_be_bytes());
    dh[16..24].copy_from_slice(&bat_off.to_be_bytes());
    dh[24..28].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    dh[28..32].copy_from_slice(&(blocks as u32).to_be_bytes());
    dh[32..36].copy_from_slice(&(VHD_BLOCK as u32).to_be_bytes());
    let cs = checksum(&dh);
    dh[36..40].copy_from_slice(&cs.to_be_bytes());

    let bat_bytes = (blocks * 4).div_ceil(512) * 512;
    let mut bat = vec![0xFFu8; bat_bytes as usize];

    let mut file = File::create(out)?;
    file.write_all(&footer)?;
    file.write_all(&dh)?;
    file.write_all(&bat)?; // placeholder
    let mut next = bat_off + bat_bytes; // sector-aligned

    let mut w = BufWriter::with_capacity(4 << 20, file);
    w.seek(SeekFrom::Start(next))?;
    let bitmap = [0xFFu8; 512];
    let mut buf = vec![0u8; VHD_BLOCK as usize];
    let mut p = Progress::new("Converting to VHD", size);
    if quiet {
        p = p.quiet();
    }
    for b in 0..blocks {
        let pos = b * VHD_BLOCK;
        if src.is_zero(pos, VHD_BLOCK.min(size - pos))? {
            p.update((pos + VHD_BLOCK).min(size));
            continue; // known-empty space in a sparse source: don't even read it
        }
        src.read_at(pos, &mut buf)?;
        if pos + VHD_BLOCK > size {
            buf[(size - pos) as usize..].fill(0);
        }
        if buf.iter().any(|&x| x != 0) {
            let sector = (next / 512) as u32;
            bat[b as usize * 4..b as usize * 4 + 4].copy_from_slice(&sector.to_be_bytes());
            w.write_all(&bitmap)?;
            w.write_all(&buf)?;
            next += 512 + VHD_BLOCK;
        }
        p.update((pos + VHD_BLOCK).min(size));
    }
    w.write_all(&footer)?;
    let file = w.into_inner().map_err(|e| e.into_error())?;
    file.seek_write(&bat, bat_off)?;
    file.sync_all()?;
    p.finish();
    Ok(())
}

// ---------------------------------------------------------------- VHDX (MS-VHDX v1.0)

/// CRC-32C (Castagnoli), as used by every VHDX checksum.
fn crc32c(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    !data.iter().fold(!0u32, |c, &b| t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8))
}

/// A GUID in its on-disk (mixed-endian) byte order.
pub(crate) fn guid(s: &str) -> [u8; 16] {
    let h: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let byte = |i: usize| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap();
    let mut g = [0u8; 16];
    for (dst, src) in [(0, 3), (1, 2), (2, 1), (3, 0), (4, 5), (5, 4), (6, 7), (7, 6)] {
        g[dst] = byte(src);
    }
    for i in 8..16 {
        g[i] = byte(i);
    }
    g
}

const VHDX_BLOCK: u64 = 2 << 20;
const MB: u64 = 1 << 20;
/// Largest virtual size the VHDX format supports (64 TiB).
pub const VHDX_MAX: u64 = 64 << 40;

/// Write a dynamic VHDX. Used for disks too large for VHD.
pub fn write_vhdx(src: &mut dyn Source, out: &Path, quiet: bool) -> Result<()> {
    let size = src.len().div_ceil(512) * 512;
    if size > VHDX_MAX {
        bail!("disk is {} bytes; VHDX supports at most 64 TiB", size);
    }
    let blocks = size.div_ceil(VHDX_BLOCK);
    // One sector-bitmap entry follows every `chunk` payload entries (2^23 sectors).
    let chunk = (1u64 << 23) * 512 / VHDX_BLOCK;
    let bat_entries = blocks + (blocks.saturating_sub(1)) / chunk;
    let bat_off = 3 * MB;
    let bat_len = (bat_entries * 8).div_ceil(MB).max(1) * MB;
    let mut bat = vec![0u8; bat_len as usize];

    let mut file = File::create(out)?;
    // File type identifier (64 KiB): signature and creator.
    let mut ident = vec![0u8; 64 << 10];
    ident[..8].copy_from_slice(b"vhdxfile");
    for (i, c) in "rustimgmount".encode_utf16().enumerate() {
        ident[8 + i * 2..10 + i * 2].copy_from_slice(&c.to_le_bytes());
    }
    file.write_all(&ident)?;

    // Two headers (64 and 128 KiB); the one with the higher sequence number is current.
    let (file_write, data_write) = (random_bytes::<16>(), random_bytes::<16>());
    for seq in 0..2u64 {
        let mut h = vec![0u8; 4096];
        h[0..4].copy_from_slice(b"head");
        h[8..16].copy_from_slice(&seq.to_le_bytes());
        h[16..32].copy_from_slice(&file_write);
        h[32..48].copy_from_slice(&data_write);
        // LogGuid (48..64) stays zero: the log is empty.
        h[66..68].copy_from_slice(&1u16.to_le_bytes()); // version
        h[68..72].copy_from_slice(&(MB as u32).to_le_bytes()); // log length
        h[72..80].copy_from_slice(&MB.to_le_bytes()); // log offset
        let crc = crc32c(&h);
        h[4..8].copy_from_slice(&crc.to_le_bytes());
        let mut slot = vec![0u8; 64 << 10];
        slot[..4096].copy_from_slice(&h);
        file.write_all(&slot)?;
    }

    // Two identical region tables (192 and 256 KiB): where the BAT and metadata live.
    let mut rt = vec![0u8; 64 << 10];
    rt[0..4].copy_from_slice(b"regi");
    rt[8..12].copy_from_slice(&2u32.to_le_bytes());
    let regions = [(guid("2DC27766-F623-4200-9D64-115E9BFD4A08"), bat_off, bat_len), (guid("8B7CA206-4790-4B9A-B8FE-575F050F886E"), 2 * MB, MB)];
    for (i, (g, off, len)) in regions.iter().enumerate() {
        let e = &mut rt[16 + i * 32..48 + i * 32];
        e[0..16].copy_from_slice(g);
        e[16..24].copy_from_slice(&off.to_le_bytes());
        e[24..28].copy_from_slice(&(*len as u32).to_le_bytes());
        e[28..32].copy_from_slice(&1u32.to_le_bytes()); // required
    }
    let crc = crc32c(&rt);
    rt[4..8].copy_from_slice(&crc.to_le_bytes());
    file.write_all(&rt)?;
    file.write_all(&rt)?;
    file.write_all(&vec![0u8; (MB - (320 << 10)) as usize])?; // up to the log
    file.write_all(&vec![0u8; MB as usize])?; // empty log (1 MiB)

    // Metadata region (2 MiB): table, then the items from offset 64 KiB.
    let mut md = vec![0u8; MB as usize];
    md[0..8].copy_from_slice(b"metadata");
    let mut params = Vec::new();
    params.extend_from_slice(&(VHDX_BLOCK as u32).to_le_bytes());
    params.extend_from_slice(&0u32.to_le_bytes());
    let items: [(&str, Vec<u8>, u32); 5] = [
        ("CAA16737-FA36-4D43-B3B6-33F0AA44E76B", params, 4),                         // file parameters
        ("2FA54224-CD1B-4876-B211-5DBED83BF4B8", size.to_le_bytes().to_vec(), 6),   // virtual disk size
        ("BECA12AB-B2E6-4523-93EF-C309E000C746", random_bytes::<16>().to_vec(), 6), // virtual disk id
        ("8141BF1D-A96F-4709-BA47-F233A8FAAB5F", 512u32.to_le_bytes().to_vec(), 6), // logical sector size
        ("CDA348C7-445D-4471-9CC9-E9885251C556", 512u32.to_le_bytes().to_vec(), 6), // physical sector size
    ];
    md[10..12].copy_from_slice(&(items.len() as u16).to_le_bytes());
    let mut item_off = 64u32 << 10;
    for (i, (id, data, flags)) in items.iter().enumerate() {
        let e = &mut md[32 + i * 32..64 + i * 32];
        e[0..16].copy_from_slice(&guid(id));
        e[16..20].copy_from_slice(&item_off.to_le_bytes());
        e[20..24].copy_from_slice(&(data.len() as u32).to_le_bytes());
        e[24..28].copy_from_slice(&flags.to_le_bytes()); // bit1 virtual disk, bit2 required
        md[item_off as usize..item_off as usize + data.len()].copy_from_slice(data);
        item_off += 64; // keep items apart and aligned
    }
    file.write_all(&md)?;
    file.write_all(&bat)?; // placeholder BAT at 3 MiB

    let mut next = bat_off + bat_len; // 1 MiB aligned; blocks are 2 MiB so stay aligned
    let mut w = BufWriter::with_capacity(4 << 20, file);
    let mut buf = vec![0u8; VHDX_BLOCK as usize];
    let mut p = Progress::new("Converting to VHDX", size);
    if quiet {
        p = p.quiet();
    }
    for b in 0..blocks {
        let pos = b * VHDX_BLOCK;
        let n = VHDX_BLOCK.min(size - pos);
        p.update(pos + n);
        if src.is_zero(pos, n)? {
            continue;
        }
        src.read_at(pos, &mut buf)?;
        buf[n as usize..].fill(0);
        if buf.iter().all(|&x| x == 0) {
            continue;
        }
        let idx = (b + b / chunk) as usize;
        let entry = 6u64 | ((next / MB) << 20); // PAYLOAD_BLOCK_FULLY_PRESENT
        bat[idx * 8..idx * 8 + 8].copy_from_slice(&entry.to_le_bytes());
        w.write_all(&buf)?;
        next += VHDX_BLOCK;
    }
    let file = w.into_inner().map_err(|e| e.into_error())?;
    file.seek_write(&bat, bat_off)?;
    file.sync_all()?;
    p.finish();
    Ok(())
}

// ---------------------------------------------------------------- choosing and safety

/// VHD up to its 2040 GiB limit (the best-tested path), VHDX above that.
pub fn disk_format(size: u64) -> crate::formats::NativeKind {
    if size.div_ceil(512) * 512 > VHD_MAX || std::env::var_os("RUSTIMGMOUNT_FORCE_VHDX").is_some() {
        crate::formats::NativeKind::Vhdx
    } else {
        crate::formats::NativeKind::Vhd
    }
}

fn free_space_check(out: &Path, need: u64, exact: bool) -> Result<()> {
    let dir = out.parent().unwrap_or(Path::new("."));
    let Some(free) = crate::vdisk::free_space(dir) else { return Ok(()) };
    const MARGIN: u64 = 64 << 20;
    if need + MARGIN <= free {
        return Ok(());
    }
    let hint = "set RUSTIMGMOUNT_CACHE to a folder on a drive with more room";
    if exact {
        bail!(
            "not enough free space on {}: this needs {}, only {} free ({hint})",
            dir.display(),
            crate::util::human(need),
            crate::util::human(free)
        );
    }
    eprintln!(
        "warning: the converted disk could need up to {} (its full size) and {} is free on {}; \
         empty space is skipped, so it may still fit ({hint})",
        crate::util::human(need),
        crate::util::human(free),
        dir.display()
    );
    Ok(())
}

/// Refuse if `need` bytes won't fit in `dir` (which may not exist yet).
pub fn check_free_space(dir: &Path, need: u64) -> Result<()> {
    let existing = dir.ancestors().find(|a| a.exists()).unwrap_or(dir);
    free_space_check(&existing.join("x"), need, true)
}

/// Did this error come from the destination drive filling up?
pub fn disk_full(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<std::io::Error>().and_then(|e| e.raw_os_error()).is_some_and(|c| c == 112 || c == 39))
}

/// Turn I/O failures into a clear message and remove the half-written file.
fn finish(r: Result<()>, out: &Path) -> Result<()> {
    if let Err(e) = r {
        let _ = std::fs::remove_file(out);
        if disk_full(&e) {
            bail!("ran out of disk space while writing {} (the partial file was deleted)", out.display());
        }
        return Err(e);
    }
    Ok(())
}

/// Convert a disc to an ISO, checking for space first.
pub fn to_iso(src: &mut dyn Source, out: &Path) -> Result<()> {
    free_space_check(out, src.len(), true)?;
    finish(write_iso(src, out, false), out)
}

/// Convert a disk to `out` (.vhd or .vhdx, per `disk_format`), checking for space first.
pub fn to_disk(src: &mut dyn Source, out: &Path) -> Result<()> {
    free_space_check(out, src.len(), false)?;
    let r = match crate::formats::NativeKind::from_ext(out) {
        Some(crate::formats::NativeKind::Vhdx) => write_vhdx(src, out, false),
        _ => write_vhd(src, out, false),
    };
    finish(r, out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc32c_known_value() {
        // RFC 3720 test vector: 32 bytes of zeros.
        assert_eq!(super::crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(super::crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn guid_byte_order() {
        let g = super::guid("2DC27766-F623-4200-9D64-115E9BFD4A08");
        assert_eq!(g, [0x66, 0x77, 0xC2, 0x2D, 0x23, 0xF6, 0x00, 0x42, 0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A, 0x08]);
    }
}

#[cfg(test)]
mod vhd_tests {
    use super::*;

    #[test]
    fn geometry_matches_spec_examples() {
        // 127 MiB disk from Hyper-V: known geometry 1024/8/17? Just check bounds/consistency.
        let (c, h, s) = geometry(3 << 20);
        assert!(c as u64 * h as u64 * s as u64 * 512 <= 3 << 20);
        let (c, h, s) = geometry(VHD_MAX);
        assert_eq!((c, h, s), (65535, 16, 255));
    }

    #[test]
    fn footer_checksum_valid() {
        let f = vhd_footer(10 << 20, true);
        let mut z = f;
        z[64..68].fill(0);
        assert_eq!(checksum(&z), u32::from_be_bytes(f[64..68].try_into().unwrap()));
    }
}
