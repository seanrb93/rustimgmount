use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub fn random_u64() -> u64 {
    let mut h = RandomState::new().build_hasher();
    h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.write_u32(std::process::id());
    h.finish()
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    for c in out.chunks_mut(8) {
        let r = random_u64().to_le_bytes();
        c.copy_from_slice(&r[..c.len()]);
    }
    out
}

/// Stable FNV-1a hash (used for cache file names).
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// True while a progress line is on screen without its final newline.
pub static PROGRESS_LINE_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// End an unfinished progress line, so that what is printed next starts on its own line.
pub fn end_progress_line() {
    if PROGRESS_LINE_OPEN.swap(false, std::sync::atomic::Ordering::Relaxed) {
        eprintln!();
    }
}

pub struct Progress {
    label: String,
    total: u64,
    last_pct: i32,
    start: Instant,
    quiet: bool,
}

impl Progress {
    pub fn new(label: &str, total: u64) -> Self {
        Self { label: label.into(), total, last_pct: -1, start: Instant::now(), quiet: false }
    }
    pub fn quiet(mut self) -> Self {
        self.quiet = true;
        self
    }
    pub fn update(&mut self, done: u64) {
        if self.quiet || self.total == 0 {
            return;
        }
        let pct = (done.saturating_mul(100) / self.total) as i32;
        if pct != self.last_pct {
            self.last_pct = pct;
            let secs = self.start.elapsed().as_secs_f64().max(0.001);
            let rate = done as f64 / secs;
            eprint!("\r{}: {:3}%  ({} / {}, {}/s)   ", self.label, pct, human(done), human(self.total), human(rate as u64));
            let _ = std::io::stderr().flush();
            PROGRESS_LINE_OPEN.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    pub fn finish(&mut self) {
        if !self.quiet && self.last_pct >= 0 {
            self.update(self.total);
            end_progress_line();
        }
    }
}
