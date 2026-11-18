//! Test-fixture generator: builds CHD files with the `chd` crate's writer.
//!
//!   mkchd cd  <out.chd> <codec> TYPE:file [TYPE:file ...]   (CD-ROM, CHT2 metadata)
//!   mkchd gd  <out.chd> <codec> TYPE:file [TYPE:file ...]   (GD-ROM, CHGD metadata)
//!   mkchd hd  <out.chd> <disk.img>
//!   mkchd dvd <out.chd> <image.iso>
//!
//! codec: default | zl | lz | fl | zs | none. TYPE: MODE1 (2048-byte sectors in file),
//! MODE1_RAW / MODE2_RAW / AUDIO (2352-byte sectors).

use chd::header::CodecType;
use chd::metadata::KnownMetadata;
use chd::write::{ChdWriteOptions, ChdWriter};
use std::ffi::CString;
use std::fs::File;
use std::io::Write;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (mode, out) = (a[1].as_str(), &a[2]);
    match mode {
        "hd" => {
            let data = std::fs::read(&a[3]).unwrap();
            let mut w = ChdWriter::new(File::create(out).unwrap(), ChdWriteOptions::hard_disk()).unwrap();
            let sectors = data.len() / 512;
            let meta = CString::new(format!("CYLS:{},HEADS:1,SECS:1,BPS:512", sectors)).unwrap();
            w.add_metadata(KnownMetadata::HardDisk, meta.as_c_str(), true).unwrap();
            w.write_all(&data).unwrap();
            w.finish().unwrap();
        }
        "dvd" => {
            let data = std::fs::read(&a[3]).unwrap();
            let mut w = ChdWriter::new(File::create(out).unwrap(), ChdWriteOptions::dvd()).unwrap();
            w.add_metadata(u32::from_be_bytes(*b"DVD "), &b"\0"[..], true).unwrap();
            w.write_all(&data).unwrap();
            w.finish().unwrap();
        }
        // mkchd cdchild <out.chd> <parent.chd> MODE1_RAW:file  -> a delta CHD on top of a parent
        "cdchild" => {
            let mut parent = chd::Chd::open(std::io::BufReader::new(File::open(&a[3]).unwrap()), None).unwrap();
            let (ty, path) = a[4].split_once(':').unwrap();
            let data = std::fs::read(path).unwrap();
            let frames = data.len() / 2352;
            let mut all = Vec::new();
            for f in 0..frames {
                let mut fr = [0u8; 2448];
                fr[..2352].copy_from_slice(&data[f * 2352..(f + 1) * 2352]);
                all.extend_from_slice(&fr);
            }
            for _ in 0..(4 - frames % 4) % 4 {
                all.extend_from_slice(&[0u8; 2448]);
            }
            let opts = ChdWriteOptions::cd().compression(&[CodecType::ZLibCdV5]).logical_bytes(all.len() as u64);
            let mut w = ChdWriter::with_parent(File::create(out).unwrap(), opts, &mut parent).unwrap();
            let m = CString::new(format!("TRACK:1 TYPE:{ty} SUBTYPE:NONE FRAMES:{frames} PREGAP:0 PGTYPE:MODE1 PGSUB:RW POSTGAP:0")).unwrap();
            w.add_metadata(KnownMetadata::CdRomTrack2, m.as_c_str(), true).unwrap();
            w.write_all(&all).unwrap();
            w.finish().unwrap();
        }
        "cd" | "gd" => {
            let opts = match a[3].as_str() {
                "zl" => ChdWriteOptions::cd().compression(&[CodecType::ZLibCdV5]),
                "lz" => ChdWriteOptions::cd().compression(&[CodecType::LzmaCdV5]),
                "fl" => ChdWriteOptions::cd().compression(&[CodecType::FlacCdV5]),
                "zs" => ChdWriteOptions::cd().compression(&[CodecType::ZstdCdV5]),
                "none" => ChdWriteOptions::cd().compression(&[] as &[CodecType; 0]),
                _ => ChdWriteOptions::cd(),
            };
            let tag = if mode == "gd" { KnownMetadata::GdRomTrack } else { KnownMetadata::CdRomTrack2 };
            let mut frames_all: Vec<u8> = Vec::new();
            let mut metas = Vec::new();
            for (i, spec) in a[4..].iter().enumerate() {
                let (ty, path) = spec.split_once(':').unwrap();
                let data = std::fs::read(path).unwrap();
                let ss = if ty == "MODE1" { 2048 } else { 2352 };
                let frames = data.len() / ss;
                for f in 0..frames {
                    let mut fr = [0u8; 2448];
                    fr[..ss].copy_from_slice(&data[f * ss..(f + 1) * ss]);
                    if ty == "AUDIO" {
                        // Like chdman: CD audio is stored big-endian in CHDs.
                        for pair in fr[..ss].chunks_exact_mut(2) {
                            pair.swap(0, 1);
                        }
                    }
                    frames_all.extend_from_slice(&fr);
                }
                for _ in 0..(4 - frames % 4) % 4 {
                    frames_all.extend_from_slice(&[0u8; 2448]);
                }
                let m = format!(
                    "TRACK:{} TYPE:{} SUBTYPE:NONE FRAMES:{} PREGAP:0 PGTYPE:MODE1 PGSUB:RW POSTGAP:0",
                    i + 1,
                    ty,
                    frames
                );
                metas.push(CString::new(m).unwrap());
            }
            let mut w = ChdWriter::new(File::create(out).unwrap(), opts.logical_bytes(frames_all.len() as u64)).unwrap();
            for m in &metas {
                w.add_metadata(tag, m.as_c_str(), true).unwrap();
            }
            w.write_all(&frames_all).unwrap();
            w.finish().unwrap();
        }
        _ => panic!("unknown mode"),
    }
}
