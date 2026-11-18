//! Test-fixture generator for formats that need a real third-party writer:
//!
//!   mkfixtures e01 <input.raw> <out.E01> [max_segment_bytes]   (EnCase EWF1, via ewf-image)
//!   mkfixtures 7z  <input_dir> <out.7z>                        (via sevenz-rust2)

use std::fs::File;
use std::io;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "e01" => {
            let mut opts = ewf_image::WriteOptions {
                format: ewf_image::WriteFormat::Ewf1Physical,
                compression: ewf_image::WriteCompression::Zlib,
                ..ewf_image::WriteOptions::default()
            };
            if let Some(max) = a.get(4) {
                opts.maximum_segment_size = Some(max.parse().unwrap());
            }
            let mut input = File::open(&a[2]).unwrap();
            let mut w = ewf_image::EwfWriter::create(&a[3], opts).unwrap();
            io::copy(&mut input, &mut w).unwrap();
            let r = w.finish().unwrap();
            for p in r.segment_paths {
                println!("{}", p.display());
            }
        }
        "7z" => sevenz_rust2::compress_to_path(&a[2], &a[3]).unwrap(),
        _ => panic!("unknown mode"),
    }
}
