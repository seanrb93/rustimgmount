//! Runs the format test suites in tests/fixtures as part of `cargo test`.
//!
//! The suites generate an image in every supported format, convert each one with the
//! rustimgmount binary under test, and check the result by reading its file system.
//! They need Python 3 (standard library only). Without Python, the test is skipped with a
//! message. Set RUSTIMGMOUNT_SKIP_FIXTURES=1 to skip them deliberately.

use std::path::{Path, PathBuf};
use std::process::Command;

fn python() -> Option<Vec<String>> {
    for cand in [vec!["python"], vec!["py", "-3"], vec!["python3"]] {
        let ok = Command::new(cand[0]).args(&cand[1..]).arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
        if ok {
            return Some(cand.iter().map(|s| s.to_string()).collect());
        }
    }
    None
}

/// Build the fixture generators (examples/mkchd.rs, examples/mkfixtures.rs) in their own
/// target directory, so this doesn't wait on the build lock of the `cargo test` running us.
fn fixture_tools(root: &Path, tmp: &Path) -> PathBuf {
    let target = tmp.join("fixture-tools");
    let status = Command::new(env!("CARGO"))
        .current_dir(root)
        .args(["build", "--release", "--example", "mkchd", "--example", "mkfixtures", "--target-dir"])
        .arg(&target)
        .status()
        .expect("running cargo to build the fixture generators");
    assert!(status.success(), "building the fixture generators failed");
    target.join("release").join("examples")
}

fn run(py: &[String], script: &Path, args: &[&Path]) {
    let out = Command::new(&py[0]).args(&py[1..]).arg("-I").arg(script).args(args).output().expect("running python");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("{stdout}");
    let failures: Vec<&str> = stdout.lines().filter(|l| l.starts_with("FAIL")).collect();
    assert!(
        out.status.success() && failures.is_empty(),
        "{} failed:\n{}\n{stderr}",
        script.display(),
        failures.join("\n")
    );
}

#[test]
fn fixture_suites() {
    if std::env::var_os("RUSTIMGMOUNT_SKIP_FIXTURES").is_some() {
        eprintln!("skipped: RUSTIMGMOUNT_SKIP_FIXTURES is set");
        return;
    }
    let Some(py) = python() else {
        eprintln!("skipped: Python 3 was not found (the fixture suites need it)");
        return;
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let exe = Path::new(env!("CARGO_BIN_EXE_rustimgmount"));
    let fixtures = root.join("tests").join("fixtures");
    let tools = fixture_tools(root, tmp);
    let data = tmp.join("fixture-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();

    // Basic formats: ISO, BIN/CUE, NRG, MDF, CCD, raw disks, VMDK, VDI, QCOW2.
    let basic = data.join("basic");
    run(&py, &fixtures.join("gen_basic.py"), &[&basic]);
    run(&py, &fixtures.join("verify_basic.py"), &[exe, &basic]);
    // Emulation formats: CHD, GDI, CDI, CSO/ZSO/DAX, ECM, gz/xz/zip.
    run(&py, &fixtures.join("emu.py"), &[exe, &tools.join("mkchd.exe"), &data.join("emu"), &basic.join("disk.img")]);
    // 7z, bz2, split images, E01, PBP, M3U, ISZ, PC-98, Atari ST, MDX, large disks (VHDX).
    run(&py, &fixtures.join("more.py"), &[exe, &tools.join("mkfixtures.exe"), &data.join("more")]);
    // `convert --to`: round trips between formats, using the images made above.
    run(&py, &fixtures.join("convert.py"), &[exe, &data, &data.join("convert")]);
    // `create`: an ISO image made from a folder, read back file by file.
    run(&py, &fixtures.join("mkiso.py"), &[exe, &data.join("mkiso")]);
}
