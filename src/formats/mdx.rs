//! Alcohol 120% / DAEMON Tools "Media Descriptor" version 2: single-file .mdx images and
//! v2 .mds descriptors. The descriptor is zlib-compressed and AES-256 encrypted with a
//! TrueCrypt-style key scheme; track data may be stored in deflate-compressed groups.
//!
//! Layout and constants follow libmirage's `image-mdx` plugin (CDEmu project), which in
//! turn credits Marisa-Chan's "mdsx" reverse-engineering. Images whose track data is
//! encrypted (a user password, or password-less "TAGES" encryption) are not supported.

use super::cd::{shared, Disc, TrackCand};
use crate::source::{le16, le32, le64, open_ro, pread, read_vec, Source};
use aes::cipher::{generic_array::GenericArray, BlockDecrypt, KeyInit};
use aes::Aes256;
use anyhow::{anyhow, bail, Context, Result};
use flate2::read::{DeflateDecoder, ZlibDecoder};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

const MAGIC_TRUE: u32 = 0x5452_5545;

pub fn is_v2(head: &[u8]) -> bool {
    head.len() >= 17 && head.starts_with(b"MEDIA DESCRIPTOR") && head[16] == 2
}

/// CRC used by CD EDC: reflected polynomial 0xD8018001, initial value 0, no final XOR.
fn edc_crc(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xD801_8001 } else { crc >> 1 };
        }
    }
    crc
}

/// The password Alcohol derives from the salt when the user set none.
fn salt_password(salt: &[u8; 64]) -> [u8; 64] {
    let mut modifier = edc_crc(salt) ^ 0x5673_72FF;
    let mut out = [0u8; 64];
    for i in 0..16 {
        let mut w = u32::from_le_bytes(salt[i * 4..i * 4 + 4].try_into().unwrap());
        modifier = modifier.wrapping_mul(0x35E8_5A6D).wrapping_add(0x1548_DCE9);
        w ^= modifier ^ 0xEC56_4717;
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    for b in out.iter_mut() {
        if *b == 0 {
            *b = 0x5F;
        }
    }
    out
}

/// AES-256 in Alcohol's CBC variant: each ciphertext block is first "de-whitened" by
/// XORing both 64-bit halves with the IV's second half, then ECB-decrypted and XORed with
/// the chaining value, which starts as the IV and then is the previous de-whitened block.
fn decrypt_cbc(key: &[u8], iv: &[u8], data: &mut [u8]) {
    let c = Aes256::new(GenericArray::from_slice(key));
    let iv1 = u64::from_le_bytes(iv[8..16].try_into().unwrap());
    let mut chain = [0u8; 16];
    chain.copy_from_slice(&iv[..16]);
    for blk in data.chunks_exact_mut(16) {
        for h in 0..2 {
            let v = u64::from_le_bytes(blk[h * 8..h * 8 + 8].try_into().unwrap()) ^ iv1;
            blk[h * 8..h * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        let saved: [u8; 16] = blk.try_into().unwrap();
        let mut b = GenericArray::clone_from_slice(blk);
        c.decrypt_block(&mut b);
        for i in 0..16 {
            blk[i] = b[i] ^ chain[i];
        }
        chain = saved;
    }
}

struct EncHeader {
    key_data: Vec<u8>,
    compressed: usize,
    decompressed: usize,
}

/// Decrypt and validate a 512-byte encryption header with the salt-derived password.
fn open_header(raw: &[u8]) -> Result<EncHeader> {
    if raw.len() < 512 {
        bail!("truncated MDX encryption header");
    }
    let salt: [u8; 64] = raw[..64].try_into().unwrap();
    let pw = salt_password(&salt);
    let mut dk = [0u8; 152];
    pbkdf2::pbkdf2_hmac::<ripemd::Ripemd160>(&pw, &salt, 2000, &mut dk);
    let mut h = raw[64..512].to_vec();
    decrypt_cbc(&dk[32..64], &dk[0..16], &mut h);
    let checksum = le32(&h, 0);
    let magic = le32(&h, 4);
    let key_size = le16(&h, 10);
    let key_data = h[16..16 + 256].to_vec();
    if magic != MAGIC_TRUE || key_size != 0x100 || crc32fast::hash(&key_data) != checksum {
        bail!("cannot decrypt the MDX descriptor (unsupported variant or damaged file)");
    }
    Ok(EncHeader { key_data, compressed: le32(&h, 272) as usize, decompressed: le32(&h, 276) as usize })
}

/// One track's stored sectors, decompressing groups as needed.
struct GroupedTrack {
    file: File,
    ss: u64,
    group: u64,
    sectors: u64,
    /// (file offset, table entry) per group.
    groups: Vec<(u64, u16)>,
    cache: Option<(usize, Vec<u8>)>,
}

impl GroupedTrack {
    fn load(&mut self, g: usize) -> io::Result<()> {
        if self.cache.as_ref().map(|c| c.0) == Some(g) {
            return Ok(());
        }
        let n_sect = self.group.min(self.sectors - g as u64 * self.group);
        let size = (n_sect * self.ss) as usize;
        let mut out = vec![0u8; size];
        let (off, e) = self.groups[g];
        if e == 0 {
            pread(&self.file, off, &mut out)?;
        } else if e & 0x8000 != 0 {
            out.fill(e as u8);
        } else {
            let comp = read_vec(&self.file, off, e as usize)?;
            let mut r = DeflateDecoder::new(&comp[..]);
            let mut n = 0;
            while n < size {
                match r.read(&mut out[n..])? {
                    0 => break,
                    k => n += k,
                }
            }
        }
        self.cache = Some((g, out));
        Ok(())
    }
}

impl Source for GroupedTrack {
    fn len(&self) -> u64 {
        self.sectors * self.ss
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let gbytes = self.group * self.ss;
        let mut done = 0;
        while done < buf.len() {
            let off = offset + done as u64;
            if off >= self.len() {
                buf[done..].fill(0);
                break;
            }
            let g = (off / gbytes) as usize;
            let within = (off % gbytes) as usize;
            self.load(g)?;
            let c = &self.cache.as_ref().unwrap().1;
            let n = (c.len() - within).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&c[within..within + n]);
            done += n;
        }
        Ok(())
    }
}

/// Open an MDX file, or a v2 MDS descriptor (with its data file next to it).
pub fn open(path: &Path) -> Result<Disc> {
    let f = open_ro(path)?;
    let flen = f.metadata()?.len();
    let fh = read_vec(&f, 0, 64)?;
    if !is_v2(&fh) {
        bail!("not a Media Descriptor v2 file");
    }
    let enc_off = le32(&fh, 0x2C);
    let single = enc_off == 0xFFFF_FFFF;
    let (desc_off, enc_hdr_off) = if single {
        let footer = le64(&fh, 0x30);
        let footer_len = le64(&fh, 0x38);
        if footer_len < 64 || footer + footer_len > flen + 512 {
            bail!("corrupt MDX footer pointer");
        }
        (footer, footer + footer_len - 64)
    } else {
        (0x30, enc_off as u64)
    };
    let hdr = open_header(&read_vec(&f, enc_hdr_off, 512)?)?;

    // Descriptor: AES in 512-byte units (chaining restarts per unit), then zlib.
    let mut enc = read_vec(&f, desc_off, hdr.compressed.div_ceil(16) * 16)?;
    let (key, iv) = (&hdr.key_data[32..64], &hdr.key_data[0..16]);
    for chunk in enc.chunks_mut(512) {
        let n = chunk.len() / 16 * 16;
        decrypt_cbc(key, iv, &mut chunk[..n]);
    }
    if hdr.decompressed > 64 << 20 || hdr.compressed > 64 << 20 {
        bail!("corrupt MDX descriptor size");
    }
    let mut body = Vec::with_capacity(hdr.decompressed);
    ZlibDecoder::new(&enc[..hdr.compressed.min(enc.len())])
        .take(hdr.decompressed as u64)
        .read_to_end(&mut body)
        .context("decompressing the MDX descriptor")?;
    // Offsets inside the descriptor count from the start of the file header.
    let mut d = fh[..18].to_vec();
    d.extend_from_slice(&body);
    let at = |o: usize, n: usize| -> Result<&[u8]> { d.get(o..o + n).ok_or_else(|| anyhow!("truncated MDX descriptor")) };

    if le32(at(0x58, 4)?, 0) != 0 {
        bail!("this image's track data is encrypted (password-protected or TAGES-protected MDX), which is not supported");
    }
    let sessions = le16(at(0x14, 2)?, 0) as usize;
    let sess_off = le32(at(0x50, 4)?, 0) as usize;

    let data_file: PathBuf = if single {
        path.to_owned()
    } else {
        ["mdf", "MDF"].iter().map(|e| path.with_extension(e)).find(|p| p.exists()).ok_or_else(|| anyhow!("data file (.mdf) not found next to {}", path.display()))?
    };

    let mut cands = Vec::new();
    for s in 0..sessions {
        let sb = at(sess_off + s * 32, 32)?;
        let nblocks = sb[0x0A] as usize;
        let toff = le32(sb, 0x14) as usize;
        for b in 0..nblocks {
            let t = at(toff + b * 80, 80)?.to_vec();
            let point = t[4];
            if point == 0 || point > 99 {
                continue;
            }
            let kind = t[0] & 7;
            if kind == 1 {
                continue; // audio
            }
            let ss = le16(&t, 0x10) as u64;
            if ss == 0 {
                continue;
            }
            let start_lba = le32(&t, 0x24) as i32 as i64;
            let start_off = le64(&t, 0x28);
            let footers = le32(&t, 0x30) as usize;
            let foot_off = le32(&t, 0x34) as usize;
            let mut length = le64(&t, 0x40);
            // User data position: sync (12) + header (4) + subheader (8) when present.
            let hint = (if t[0] & 0x80 != 0 { 12 } else { 0 }) + (if t[0] & 0x20 != 0 { 4 } else { 0 }) + (if t[0] & 0x40 != 0 { 8 } else { 0 });
            let (mut compressed, mut group, mut table_off) = (false, 0u64, 0u64);
            if footers > 0 {
                let ft = at(foot_off, 32)?;
                compressed = ft[4] & 1 != 0;
                group = le32(ft, 12) as u64;
                if length == 0 {
                    length = le64(ft, 16);
                }
                table_off = le64(ft, 24);
            }
            let file = open_ro(&data_file)?;
            let src: Box<dyn Source> = if compressed {
                if group == 0 || group > 4096 || ss > 4096 {
                    bail!("corrupt MDX compression parameters");
                }
                let n = length.div_ceil(group) as usize;
                let comp = read_vec(&file, start_off + table_off, (n + 0x800) * 2)?;
                let mut tab = Vec::with_capacity(n * 2);
                ZlibDecoder::new(&comp[..]).take(n as u64 * 2).read_to_end(&mut tab).context("reading MDX compression table")?;
                if tab.len() < n * 2 {
                    bail!("truncated MDX compression table");
                }
                let mut groups = Vec::with_capacity(n);
                let mut pos = start_off;
                for g in 0..n {
                    let e = le16(&tab, g * 2);
                    groups.push((pos, e));
                    pos += match e {
                        0 => group.min(length - g as u64 * group) * ss,
                        e if e & 0x8000 != 0 => 0,
                        e => e as u64,
                    };
                }
                Box::new(GroupedTrack { file, ss, group, sectors: length, groups, cache: None })
            } else {
                Box::new(crate::source::FileSlice::new(file, start_off, length * ss))
            };
            cands.push(
                TrackCand::new(shared(src), 0, ss as u32, length)
                    .hint((hint + 2048 <= ss as u32).then_some(hint))
                    .lba(Some(start_lba))
                    .label(format!("track {point}")),
            );
        }
    }
    Disc::assemble(cands, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edc_crc_matches_known_value() {
        // CRC of an all-zero buffer is zero; of a single 0x01 byte equals the polynomial step.
        assert_eq!(edc_crc(&[0u8; 16]), 0);
        let mut crc = 1u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xD801_8001 } else { crc >> 1 };
        }
        assert_eq!(edc_crc(&[1]), crc);
    }
}
