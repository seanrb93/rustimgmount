//! Fuzz-style robustness test: feed rustimgmount hundreds of malformed images (real
//! format signatures followed by random header fields, truncated files, garbage text
//! sheets) and check that every run ends with a normal exit code: success or a reported
//! error, never a crash, an abort (e.g. from an absurd allocation) or a hang in `info`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[(self.next() % v.len() as u64) as usize]
    }
}

/// Random data with `magic` written at `at`.
fn with_magic(rng: &mut Rng, len: usize, at: usize, magic: &[u8]) -> Vec<u8> {
    let mut v = rng.bytes(len.max(at + magic.len()));
    v[at..at + magic.len()].copy_from_slice(magic);
    v
}

fn cases(rng: &mut Rng) -> Vec<(String, Vec<u8>)> {
    let sizes = [0usize, 5, 64, 300, 513, 2048, 4096, 40_000, 200_000];
    let binary: &[(&str, usize, &[u8])] = &[
        ("vmdk", 0, b"KDMV"),
        ("qcow2", 0, b"QFI\xfb\x00\x00\x00\x03"),
        ("vdi", 0x40, &[0x7F, 0x10, 0xDA, 0xBE]),
        ("chd", 0, b"MComprHD"),
        ("cso", 0, b"CISO"),
        ("zso", 0, b"ZISO"),
        ("dax", 0, b"DAX\0"),
        ("isz", 0, b"IsZ!"),
        ("pbp", 0, b"\0PBP"),
        ("mdx", 0, b"MEDIA DESCRIPTOR\x02\x01"),
        ("mds", 0, b"MEDIA DESCRIPTOR\x01\x05"),
        ("hdd", 0, b"WithoutFreeSpace"),
        ("e01", 0, b"EVF\x09\x0D\x0A\xFF\x00"),
        ("vhdx", 0, b"vhdxfile"),
        ("nhd", 0, b"T98HDDIMAGE.R0\0"),
        ("msa", 0, &[0x0E, 0x0F]),
        ("ecm", 0, b"ECM\0"),
        ("gz", 0, &[0x1F, 0x8B, 8, 0]),
        ("7z", 0, &[b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C]),
        ("zip", 0, b"PK\x03\x04"),
        ("bz2", 0, b"BZh91AY&SY"),
        ("xz", 0, &[0xFD, b'7', b'z', b'X', b'Z', 0]),
        ("iso", 0x8001, b"CD001"),
        ("img", 510, &[0x55, 0xAA]),
        ("st", 0, &[0x60, 0x38]),
        ("hdi", 8, &[0, 0x10, 0, 0]),
        ("fdi", 8, &[0, 0x10, 0, 0]),
        ("d88", 0, b"\0"),
    ];
    let mut out = Vec::new();
    for (ext, at, magic) in binary {
        for (i, &len) in sizes.iter().enumerate() {
            out.push((format!("b{i}.{ext}"), with_magic(rng, len, *at, magic)));
        }
    }
    // NRG footer pointing somewhere random.
    for i in 0..4 {
        let mut v = rng.bytes(5000);
        let n = v.len();
        v[n - 12..n - 8].copy_from_slice(b"NER5");
        out.push((format!("n{i}.nrg"), v));
    }
    // Dynamic VHDs: a valid footer and "cxsparse" header, everything else random (table
    // offset, entry count, block size, disk size).
    for i in 0..10 {
        let mut v = rng.bytes(8192);
        v[0..8].copy_from_slice(b"conectix");
        v[16..24].copy_from_slice(&512u64.to_be_bytes());
        v[60..64].copy_from_slice(&rng.pick(&[2u32, 3, 3, 3, 4, 9]).to_be_bytes());
        v[512..520].copy_from_slice(b"cxsparse");
        if i % 2 == 0 {
            // Plausible numbers, so the reader gets as far as the block table.
            v[48..56].copy_from_slice(&(rng.next() % (64 << 20)).to_be_bytes());
            v[528..536].copy_from_slice(&1536u64.to_be_bytes());
            v[540..544].copy_from_slice(&((rng.next() % 64) as u32).to_be_bytes());
            v[544..548].copy_from_slice(&(2u32 << 20).to_be_bytes());
        }
        out.push((format!("d{i}.vhd"), v));
    }
    // VHDX: the signatures and region GUIDs in place, the tables they lead to random.
    let guid = |b: [u8; 16]| b;
    let bat = guid([0x66, 0x77, 0xC2, 0x2D, 0x23, 0xF6, 0x00, 0x42, 0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A, 0x08]);
    let meta = guid([0x06, 0xA2, 0x7C, 0x8B, 0x90, 0x47, 0x9A, 0x4B, 0xB8, 0xFE, 0x57, 0x5F, 0x05, 0x0F, 0x88, 0x6E]);
    for i in 0..10 {
        let mut v = rng.bytes(400_000);
        v[0..8].copy_from_slice(b"vhdxfile");
        for at in [64usize << 10, 128 << 10] {
            v[at..at + 4].copy_from_slice(b"head");
            if i % 3 != 0 {
                v[at + 48..at + 64].fill(0); // no pending log
            }
        }
        let rt = 192usize << 10;
        v[rt..rt + 4].copy_from_slice(b"regi");
        v[rt + 8..rt + 12].copy_from_slice(&2u32.to_le_bytes());
        for (k, (g, off)) in [(bat, 300_000u64), (meta, 262_144u64)].iter().enumerate() {
            let e = rt + 16 + k * 32;
            v[e..e + 16].copy_from_slice(g);
            v[e + 16..e + 24].copy_from_slice(&(if i % 4 == 3 { rng.next() } else { *off }).to_le_bytes());
            v[e + 24..e + 28].copy_from_slice(&(if i % 5 == 4 { rng.next() as u32 } else { 65_536 }).to_le_bytes());
        }
        v[262_144..262_152].copy_from_slice(b"metadata");
        out.push((format!("x{i}.vhdx"), v));
    }
    // CloneCD: a sheet with random track numbers, modes and positions, next to its .img
    // (and sometimes a .sub of the wrong size).
    for i in 0..10 {
        let mut s = String::from("[CloneCD]\nVersion=3\n");
        for t in 0..(rng.next() % 6) {
            s.push_str(&format!("[TRACK {}]\n", if rng.next() % 5 == 0 { rng.next() % 5_000_000_000 } else { t + 1 }));
            for key in ["MODE", "INDEX 0", "INDEX 1", "INDEX 1"] {
                if rng.next() % 3 != 0 {
                    s.push_str(&format!("{key}={}\n", rng.pick(&[0u64, 1, 2, 150, 400, 4_294_967_295, 18_446_744_073_709_551_615])));
                }
            }
        }
        out.push((format!("c{i}.ccd"), s.into_bytes()));
        let n = rng.pick(&[0usize, 100, 2352 * 3, 2352 * 40 + 7]);
        out.push((format!("c{i}.img"), rng.bytes(n)));
        if i % 2 == 0 {
            let n = rng.pick(&[0usize, 96, 96 * 40]);
            out.push((format!("c{i}.sub"), rng.bytes(n)));
        }
    }
    // Text sheets with random numbers and names.
    let words = ["FILE", "TRACK", "INDEX", "MODE1/2352", "AUDIO", "BINARY", "\"x.bin\"", "99:99:99", "-5", "4294967295", "RW", "FLAT", "SPARSE", "\"a.vmdk\"", "0", "45000", "2352"];
    for (ext, header) in [("cue", ""), ("gdi", "3\n"), ("m3u", ""), ("vmdk", "# Disk DescriptorFile\nparentCID=ffffffff\n")] {
        for i in 0..6 {
            let mut s = String::from(header);
            for _ in 0..(rng.next() % 40) {
                s.push_str(rng.pick(&words));
                s.push(if rng.next() % 4 == 0 { '\n' } else { ' ' });
            }
            out.push((format!("t{i}.{ext}"), s.into_bytes()));
        }
    }
    out
}

/// Run with a time limit. Returns the exit code, or None if it had to be killed.
fn run(exe: &Path, args: &[&Path], data: &Path, limit: Duration) -> Option<i32> {
    let mut child = Command::new(exe)
        .args(args)
        .env("RUSTIMGMOUNT_DATA", data)
        .env("RUSTIMGMOUNT_CACHE", data.join("cache"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn rustimgmount");
    let start = Instant::now();
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return Some(s.code().unwrap_or(-1));
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn malformed_images_never_crash() {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_rustimgmount"));
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("robustness");
    let _ = std::fs::remove_dir_all(&root);
    let mut jobs = Vec::new();
    for seed in 1..=5u64 {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed);
        let dir = root.join(format!("seed{seed}"));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, data) in cases(&mut rng) {
            let p = dir.join(&name);
            std::fs::write(&p, data).unwrap();
            jobs.push(p);
        }
    }
    let data = root.join("appdata");
    let failures = std::sync::Mutex::new(Vec::new());
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(p) = jobs.get(i) else { break };
                // `info` must finish promptly with 0 or 1.
                match run(&exe, &[Path::new("info"), p], &data, Duration::from_secs(30)) {
                    Some(0 | 1) => {}
                    other => failures.lock().unwrap().push(format!("info {}: {:?}", p.display(), other)),
                }
                // `convert` may legitimately run long on a huge (but valid-looking) disk;
                // only a crash counts as a failure here.
                // (An unknown output extension lets rustimgmount pick ISO or VHD itself.)
                let out = p.with_extension("out.tmp");
                if let Some(code) = run(&exe, &[Path::new("convert"), p, &out], &data, Duration::from_secs(20)) {
                    if code != 0 && code != 1 {
                        failures.lock().unwrap().push(format!("convert {}: exit code {code}", p.display()));
                    }
                }
                let _ = std::fs::remove_file(&out);
                // VHD and VHDX are only read when converted to something else.
                if p.extension().is_some_and(|e| e == "vhd" || e == "vhdx") {
                    let img = p.with_extension("conv.img");
                    if let Some(code) = run(&exe, &[Path::new("convert"), p, Path::new("--to"), Path::new("img"), &img], &data, Duration::from_secs(20)) {
                        if code != 0 && code != 1 {
                            failures.lock().unwrap().push(format!("convert to img {}: exit code {code}", p.display()));
                        }
                    }
                    let _ = std::fs::remove_file(&img);
                }
                // The whole-disc path (track lists, raw sectors) used by `convert --to cue/chd`.
                let cue = p.with_extension("conv.cue");
                if let Some(code) = run(&exe, &[Path::new("convert"), p, &cue], &data, Duration::from_secs(20)) {
                    if code != 0 && code != 1 {
                        failures.lock().unwrap().push(format!("convert to cue {}: exit code {code}", p.display()));
                    }
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    assert!(failures.is_empty(), "{} of {} malformed images caused crashes or hangs:\n{}", failures.len(), jobs.len(), failures.join("\n"));
    let _ = std::fs::remove_dir_all(&root);
}
