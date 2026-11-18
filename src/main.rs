//! rustimgmount — mount common disk images on Windows.
//!
//! ISO, VHD and VHDX are attached directly through the Windows VirtDisk API.
//! Every other format is translated once into a cached .iso (optical media) or a
//! sparse .vhd (hard disks / floppies), which is then attached the same way.

mod convert;
mod export;
mod formats;
mod source;
mod state;
mod util;
mod vdisk;
mod winutil;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use formats::{NativeKind, Plan};
use state::{same_path, Record, State};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "rustimgmount", version, about = "Mount disk images on Windows (ISO, IMG, BIN/CUE, NRG, MDF/MDS, CCD, CDI, VHD, VHDX, VMDK, VDI, QCOW2, HDD, ...)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Wait for Enter before exiting (used by the Explorer context menu).
    #[arg(long, global = true, hide = true)]
    pause: bool,
    /// Running hidden under rustimgmountw.exe, which shows any error in a dialog.
    #[arg(long, global = true, hide = true)]
    quiet_ui: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mount an image and print the drive letter(s) it received.
    Mount {
        image: PathBuf,
        /// Mount read-only (VHD/VHDX are writable by default; everything else is read-only).
        #[arg(long, conflicts_with = "writable")]
        read_only: bool,
        /// Allow writes to a converted image. Changes go to the cached copy and are discarded on unmount.
        #[arg(long)]
        writable: bool,
        /// Attach without assigning drive letters.
        #[arg(long)]
        no_drive_letter: bool,
        /// Re-convert even if a cached conversion exists.
        #[arg(long)]
        refresh: bool,
        /// Open the mounted drive in Explorer afterwards (what double-clicking an image does).
        #[arg(long)]
        open: bool,
        /// Don't mount the extra drive with a disc's CD audio tracks (WAV files + ALBUM.CUE).
        #[arg(long)]
        no_audio: bool,
    },
    /// Unmount an image. TARGET is the image path, a drive letter (E:), or "all".
    #[command(alias = "umount", alias = "eject")]
    Unmount {
        target: String,
        /// With "all": also unmount images that rustimgmount did not mount
        /// (ISOs opened from Explorer, VHDs attached by other programs).
        #[arg(long)]
        everything: bool,
    },
    /// List mounted virtual disks.
    #[command(alias = "ls")]
    List {
        /// Machine-readable output (used by the tray app).
        #[arg(long)]
        json: bool,
    },
    /// Install to %LOCALAPPDATA%\Programs\rustimgmount: PATH, double-click associations,
    /// right-click menu, Start Menu shortcut for the tray app.
    Setup {
        /// Skip the Explorer right-click menu.
        #[arg(long)]
        no_menu: bool,
        /// Undo setup: remove associations, menu, PATH entry, shortcut and installed files.
        #[arg(long)]
        remove: bool,
        /// With --remove: also delete the records and cache in %LOCALAPPDATA%\rustimgmount.
        #[arg(long)]
        purge: bool,
        /// With --remove: leave the program files in place (the installer's uninstaller deletes them).
        #[arg(long, hide = true)]
        keep_files: bool,
    },
    /// (internal) Turn starting the tray app at sign-in on or off.
    #[command(hide = true)]
    Autostart { state: String },
    /// Show what format an image is and how it would be mounted.
    Info { image: PathBuf },
    /// Convert an image to another format (default: .iso for discs, .vhd for disks).
    Convert {
        image: PathBuf,
        /// File to write (default: next to the image, with the new format's extension).
        output: Option<PathBuf>,
        /// Format to convert to. Discs: iso, cso, zso (data only), chd, chd-zstd, cue (BIN/CUE),
        /// ccd (CloneCD), gdi (Dreamcast). Disks: vhd, vhd-fixed, vhdx, img (raw), vmdk, vdi,
        /// qcow2, qcow2-compressed, chd.
        #[arg(long, value_name = "FORMAT")]
        to: Option<String>,
        /// Folder for the result (default: the image's own folder).
        #[arg(long, value_name = "FOLDER", conflicts_with = "output")]
        out_dir: Option<PathBuf>,
        /// Replace the output file if it exists.
        #[arg(long)]
        overwrite: bool,
        /// Read the result back and compare it with the original; delete it if it differs.
        #[arg(long)]
        verify: bool,
        /// After a successful --verify, move the original to the Recycle Bin.
        #[arg(long)]
        delete_original: bool,
    },
    /// (internal) The formats these images can be converted to, as JSON.
    #[command(hide = true)]
    Formats { images: Vec<PathBuf> },
    /// Save a disc's CD audio tracks as WAV files (Windows can only mount the data part).
    Audio {
        image: PathBuf,
        /// Folder for the WAV files (default: "<image name> (audio)" next to the image).
        output: Option<PathBuf>,
    },
    /// Delete cached conversions that are no longer mounted.
    Clean,
    /// Add "Mount with rustimgmount" / "Unmount" to the Explorer right-click menu (current user).
    Install,
    /// Remove the Explorer right-click menu entries.
    Uninstall,
    /// Make double-clicking a supported image mount it (current user).
    Associate {
        /// Don't open Settings > Default apps for extensions Windows won't let us change.
        #[arg(long, hide = true)]
        no_settings: bool,
    },
    /// Undo `associate`, restoring the previous per-user defaults.
    Unassociate,
    /// (internal) Wait in the background until FILE is detached, then delete its copy.
    #[command(hide = true)]
    Watch { file: PathBuf },
}

fn main() {
    // Dropping a file onto the exe (or "Open with") passes just the path: treat as mount.
    let mut args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && Path::new(&args[1]).is_file() {
        args = vec![args[0].clone(), "mount".into(), args[1].clone(), "--pause".into(), "--open".into()];
    }
    let cli = Cli::parse_from(&args);
    winutil::QUIET_UI.store(cli.quiet_ui, std::sync::atomic::Ordering::Relaxed);
    let pause = cli.pause && !cli.quiet_ui;
    // A malformed image must produce an error message, never a crash: turn any panic in
    // a format reader into an ordinary error.
    std::panic::set_hook(Box::new(|_| {}));
    let code = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(cli))) {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            util::end_progress_line(); // a failure half-way leaves "...: 45%" on the line
            eprintln!("error: {e:#}");
            1
        }
        Err(p) => {
            let what = p
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| p.downcast_ref::<&str>().copied())
                .unwrap_or("unknown failure");
            util::end_progress_line();
            eprintln!("error: the image appears to be corrupt or uses an unsupported variant ({what})");
            1
        }
    };
    if pause {
        // Windows opened a console just for us (double-click, right-click menu). On
        // success it closes by itself; on failure it waits so the message can be read.
        if code == 0 {
            eprintln!("\nDone. This window closes in 3 seconds.");
            std::thread::sleep(Duration::from_secs(3));
        } else {
            eprintln!("\nPress Enter to close...");
            let _ = std::io::stdin().read_line(&mut String::new());
        }
    }
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32> {
    state::migrate_legacy();
    if !matches!(cli.cmd, Cmd::Watch { .. } | Cmd::Install | Cmd::Uninstall | Cmd::Setup { .. } | Cmd::Autostart { .. }) && winutil::repair_shell_if_moved() {
        eprintln!("note: rustimgmount was moved; the Explorer right-click menu now points at its new location");
    }
    // Housekeeping on every command: drop records of ejected mounts (deleting their
    // copies) and delete unused cache files older than an hour. `clean` does the same
    // without the grace period. The background watcher only needs its own check.
    if !matches!(
        cli.cmd,
        Cmd::Install | Cmd::Uninstall | Cmd::Associate { .. } | Cmd::Unassociate | Cmd::Watch { .. } | Cmd::Clean | Cmd::Setup { .. } | Cmd::Autostart { .. }
    ) {
        let _ = clean_cache(false);
        export::sweep(); // half-written files of a cancelled `convert`
    }
    match cli.cmd {
        Cmd::Mount { image, read_only, writable, no_drive_letter, refresh, open, no_audio } => {
            let code = mount(&image, MountOpts { read_only, writable, no_drive_letter, refresh, open, no_audio })?;
            if code == 0 {
                if let Ok(p) = absolute(&image) {
                    state::add_recent(&p); // for the tray app's "Recent" list
                }
            }
            Ok(code)
        }
        Cmd::Associate { no_settings } => {
            let r = winutil::associate()?;
            println!("Double-clicking these now mounts them: {}", r.ours.join(", "));
            if !r.others.is_empty() {
                println!("\nWindows keeps your earlier choice of app for these, and doesn't let programs change that:");
                for (ext, cmd) in &r.others {
                    println!("  .{ext:<6} opens with {cmd}");
                }
                if no_settings {
                    println!("\nSwitch them in Settings > Apps > Default apps > rustimgmount.");
                } else {
                    println!("\nOpening Settings > Default apps for rustimgmount, where you can switch them.");
                    winutil::open_default_apps_settings();
                }
            }
            println!("(.m3u is left alone because music players use it; mount playlists with `rustimgmount mount`.)");
            Ok(0)
        }
        Cmd::Unassociate => {
            winutil::unassociate()?;
            println!("File associations removed; previous defaults restored.");
            Ok(0)
        }
        Cmd::Unmount { target, everything } => unmount(&target, everything),
        Cmd::List { json } => list(json),
        Cmd::Setup { no_menu, remove, purge, keep_files } => {
            if remove {
                setup_remove(purge, keep_files)
            } else {
                setup(no_menu)
            }
        }
        Cmd::Autostart { state } => {
            match state.as_str() {
                "on" => winutil::set_autostart(true)?,
                "off" => winutil::set_autostart(false)?,
                _ => println!("{}", if winutil::autostart_enabled() { "on" } else { "off" }),
            }
            Ok(0)
        }
        Cmd::Info { image } => info(&image),
        Cmd::Convert { image, output, to, out_dir, overwrite, verify, delete_original } => {
            convert_cmd(&image, output, to, ConvertOpts { out_dir, overwrite, verify: verify || delete_original, delete_original })
        }
        Cmd::Formats { images } => formats_cmd(&images),
        Cmd::Audio { image, output } => audio_cmd(&image, output),
        Cmd::Clean => {
            let n = clean_cache(true)?;
            println!("Removed {n} cached file(s).");
            Ok(0)
        }
        Cmd::Install => {
            winutil::install_shell()?;
            println!("Explorer context menu installed for: {}", winutil::SHELL_EXTS.join(", "));
            println!("(On Windows 11 it is under \"Show more options\" / Shift+right-click.)");
            Ok(0)
        }
        Cmd::Watch { file } => watch(&file),
        Cmd::Uninstall => {
            winutil::uninstall_shell()?;
            println!("Explorer context menu removed.");
            Ok(0)
        }
    }
}

/// Absolute path without the `\\?\` prefix that `canonicalize` adds.
fn absolute(p: &Path) -> Result<PathBuf> {
    // Keep a `#N` disc selector (game.pbp#2) on the end of the resolved path.
    let (base, disc) = formats::split_disc(p);
    if let Some(d) = disc {
        let b = absolute(&base)?;
        return Ok(PathBuf::from(format!("{}#{}", b.display(), d + 1)));
    }
    let c = std::fs::canonicalize(p).with_context(|| format!("cannot open {}", p.display()))?;
    let s = c.to_string_lossy();
    Ok(if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        c
    })
}

#[derive(Clone)]
struct MountOpts {
    read_only: bool,
    writable: bool,
    no_drive_letter: bool,
    refresh: bool,
    open: bool,
    no_audio: bool,
}

/// Show a mounted drive in Explorer (like double-clicking an ISO does).
fn announce_elevation(what: &str) {
    println!("Administrator rights are needed to {what} this image; asking for permission...");
    if winutil::QUIET_UI.load(std::sync::atomic::Ordering::Relaxed) {
        // Tell rustimgmountw.exe to hide its progress dialog: the elevated copy shows its own.
        eprintln!("@@rustimgmount:elevating");
    }
}

fn open_in_explorer(mount_point: &str) {
    let _ = std::process::Command::new("explorer.exe").arg(mount_point).spawn();
}

/// Cache file name that changes whenever the source file changes.
pub(crate) fn cache_path(src: &Path, ext: &str) -> Result<PathBuf> {
    let (file, disc) = formats::split_disc(src);
    let md = std::fs::metadata(&file)?;
    let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
    let key = format!("{}|{}|{}", src.to_string_lossy().to_lowercase(), md.len(), mtime);
    let mut stem: String = file
        .file_stem()
        .map(|s| s.to_string_lossy().chars().filter(|c| c.is_alphanumeric() || "-_. ".contains(*c)).take(60).collect())
        .unwrap_or_default();
    if let Some(d) = disc {
        stem.push_str(&format!("-disc{}", d + 1));
    }
    Ok(state::cache_dir()?.join(format!("{stem}-{:016x}.{ext}", util::fnv1a(key.as_bytes()))))
}

/// Give Windows a file with the extension it expects, without copying if possible.
fn alias(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        return Ok(());
    }
    if std::fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    if std::os::windows::fs::symlink_file(src, dst).is_ok() {
        return Ok(());
    }
    eprintln!("Copying image to the cache (it is on another drive)...");
    let tmp = dst.with_extension("partial");
    std::fs::copy(src, &tmp)?;
    std::fs::rename(&tmp, dst)?;
    Ok(())
}

fn mount(image: &Path, opts: MountOpts) -> Result<i32> {
    let image = absolute(image)?;
    // Playlists and multi-disc EBOOTs: mount every disc.
    if let Some(discs) = formats::multi(&image)? {
        println!("{} holds {} discs.", image.display(), discs.len());
        let mut code = 0;
        for (i, d) in discs.iter().enumerate() {
            println!("\n[disc {}] {}", i + 1, d.display());
            match mount(d, opts.clone()) {
                Ok(c) => code = code.max(c),
                Err(e) => {
                    eprintln!("error: {e:#}");
                    code = 1;
                }
            }
        }
        return Ok(code);
    }
    let st = State::load();

    // Already mounted (by us, or directly by Explorer)?
    // Candidates: the recorded attach file, the image itself, and every cache file name
    // it could have been converted/aliased to (the names are deterministic).
    let attached_now = vdisk::attached_images().unwrap_or_default();
    let mut candidates: Vec<PathBuf> = st.by_source(&image).map(|r| r.attached.clone()).into_iter().collect();
    candidates.push(image.clone());
    for ext in ["iso", "vhd", "vhdx"] {
        if let Ok(p) = cache_path(&image, ext) {
            candidates.push(p);
        }
    }
    // A natively mountable image inside a decompressed archive lives in its folder.
    if let Ok(dir) = cache_path(&image, "d") {
        candidates.extend(attached_now.iter().filter(|a| state::is_inside(a, &dir)).cloned());
    }
    if let Some(target) = candidates.into_iter().find(|c| attached_now.iter().any(|a| state::same_file(a, c))) {
        println!("{} is already mounted.", image.display());
        let mps = mounts_of(&target, &vdisk::image_volumes());
        if mps.is_empty() {
            println!("  (no drive letter)");
        }
        if opts.open {
            if let Some(m) = mps.first() {
                open_in_explorer(m);
            }
        }
        for m in mps {
            println!("  -> {m}");
        }
        return Ok(0);
    }
    let det = formats::detect(&image);
    // Folders holding decompressed data (gz/xz/zip/ecm); removed on unmount.
    let extra = formats::expand::take_created();
    let det = det?;
    println!("Format: {}", det.format);

    let (attach_path, kind, temp) = match det.plan {
        Plan::Native(kind) => {
            if NativeKind::from_ext(&det.data_path) == Some(kind) {
                (det.data_path.clone(), kind, false)
            } else {
                let dst = cache_path(&det.data_path, kind.ext())?;
                alias(&det.data_path, &dst)?;
                (dst, kind, true)
            }
        }
        Plan::Cd(mut disc) => {
            let dst = cache_path(&image, "iso")?;
            if opts.refresh {
                let _ = std::fs::remove_file(&dst);
            }
            if let Some(plain) = disc.plain_file.clone() {
                alias(&plain, &dst)?;
            } else if !dst.exists() {
                let tmp = dst.with_extension("partial");
                convert::to_iso(&mut disc, &tmp)?;
                std::fs::rename(&tmp, &dst)?;
            }
            (dst, NativeKind::Iso, true)
        }
        Plan::Disk(mut src) => {
            // VHD for disks up to 2040 GiB, VHDX beyond that.
            let kind = convert::disk_format(src.len());
            let dst = cache_path(&image, kind.ext())?;
            if opts.refresh {
                let _ = std::fs::remove_file(&dst);
            }
            if !dst.exists() {
                // Keep the real extension in the temporary name: the writer picks the
                // format from it.
                let tmp = dst.with_extension(format!("partial.{}", kind.ext()));
                convert::to_disk(src.as_mut(), &tmp)?;
                std::fs::rename(&tmp, &dst)?;
            }
            (dst, kind, true)
        }
    };

    // Native VHD/VHDX default to writable (like Explorer); conversions default to read-only.
    let read_only = match kind {
        NativeKind::Iso => true,
        _ if temp && det.format != "VHD" && det.format != "VHDX" => !opts.writable,
        _ => opts.read_only,
    };

    let attach = || -> Result<String> {
        let disk = vdisk::VirtualDisk::open(&attach_path, kind, read_only)
            .with_context(|| format!("opening {}", attach_path.display()))?;
        disk.attach(read_only, opts.no_drive_letter).context("attaching virtual disk")?;
        Ok(disk.physical_path()?)
    };

    let phys = match attach() {
        Ok(p) => p,
        Err(e) if vdisk::is_access_error(&e) && !winutil::is_elevated() => {
            announce_elevation("mount");
            let mut args = vec!["mount".to_string(), image.to_string_lossy().into_owned(), "--pause".into()];
            if opts.read_only {
                args.push("--read-only".into());
            }
            if opts.writable {
                args.push("--writable".into());
            }
            if opts.no_drive_letter {
                args.push("--no-drive-letter".into());
            }
            if opts.open {
                args.push("--open".into());
            }
            if opts.no_audio {
                args.push("--no-audio".into());
            }
            let code = winutil::run_elevated(&args)?;
            return Ok(code as i32);
        }
        Err(e) => {
            if temp {
                // Leave the cached conversion for a retry; `rustimgmount clean` removes it.
            }
            return Err(e);
        }
    };

    // Record the mount against the *current* record file (other rustimgmount processes may
    // have changed it during a long conversion).
    let has_extra = !extra.is_empty();
    State::modify(|st| {
        st.upsert(Record {
            source: image.clone(),
            attached: attach_path.clone(),
            kind,
            temp,
            format: det.format.clone(),
            read_only,
            extra,
            companions: Vec::new(),
        })
    })?;
    // Converted copies are deleted as soon as the drive is ejected (e.g. from Explorer).
    if temp || has_extra {
        spawn_watcher(&attach_path);
    }

    println!("Mounted {}{}", image.display(), if read_only { " (read-only)" } else { "" });
    if opts.no_drive_letter {
        println!("  device: {phys}");
    } else {
        let mps = report(&phys, Duration::from_secs(10));
        if opts.open {
            if let Some(m) = mps.first() {
                open_in_explorer(m);
            }
        }
    }
    // A disc's CD audio can't appear on the data drive: give it a drive of its own.
    if kind == NativeKind::Iso && !opts.no_audio {
        if let Err(e) = mount_audio_drive(&image, &det.data_path, &attach_path, opts.no_drive_letter) {
            eprintln!("note: could not mount the audio-tracks drive: {e:#}");
        }
    }
    Ok(0)
}

/// If the disc has CD audio tracks, build a small ISO with TRACKnn.WAV files and
/// ALBUM.CUE, attach it next to the data drive and record it as that mount's companion,
/// so it is detached and deleted together with it.
fn mount_audio_drive(image: &Path, data_path: &Path, primary: &Path, no_drive_letter: bool) -> Result<()> {
    let tracks = formats::audio::tracks(data_path).unwrap_or_default();
    if tracks.is_empty() {
        return Ok(());
    }
    let dst = cache_path(image, "audio.iso")?;
    if !still_attached(&dst) {
        if !dst.exists() {
            let tmp = dst.with_extension("partial");
            convert::check_free_space(&tmp, formats::audio::iso_size(&tracks))?;
            let title = formats::split_disc(image).0.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Disc".into());
            if let Err(e) = formats::audio::write_iso(&tracks, &title, &tmp, false) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
            std::fs::rename(&tmp, &dst)?;
        }
        // Record it before attaching: an attached audio drive that no mount claims is
        // treated as a leftover and detached by the next command's cleanup.
        State::modify(|st| {
            if let Some(r) = st.mounts.iter_mut().find(|r| state::same_file(&r.attached, primary)) {
                r.companions.retain(|c| !state::same_file(c, &dst));
                r.companions.push(dst.clone());
            }
        })?;
        let disk = vdisk::VirtualDisk::open(&dst, NativeKind::Iso, true)?;
        disk.attach(true, no_drive_letter)?;
        let phys = disk.physical_path()?;
        let secs: f64 = tracks.iter().map(|t| t.seconds()).sum();
        println!("Audio tracks: {} ({}:{:02}), as WAV files on their own drive", tracks.len(), secs as u64 / 60, secs as u64 % 60);
        if no_drive_letter {
            println!("  device: {phys}");
        } else if let Some(m) = report(&phys, Duration::from_secs(10)).first() {
            println!("  Open {m}ALBUM.CUE in an audio player (e.g. foobar2000) to play the disc.");
        }
    }
    Ok(())
}

/// Print the mount points of a freshly attached disk, waiting for volumes to arrive.
fn report(phys: &str, wait: Duration) -> Vec<String> {
    let start = Instant::now();
    loop {
        let mps = vdisk::mount_points_for(phys);
        if !mps.is_empty() {
            for m in &mps {
                println!("  -> {m}");
            }
            return mps;
        }
        if start.elapsed() > wait {
            println!("  device: {phys}");
            println!("  (no drive letter: the image may have no partitions or a file system Windows cannot read)");
            return mps;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

struct Attached {
    file: PathBuf,
    phys: Option<String>,
    mounts: Vec<String>,
}

/// Mount points of an attached image, found via the volumes' storage dependencies
/// (works without admin rights).
fn mounts_of(file: &Path, volumes: &[(PathBuf, Vec<String>)]) -> Vec<String> {
    volumes.iter().filter(|(f, _)| state::same_file(f, file)).flat_map(|(_, m)| m.clone()).collect()
}

fn attached_disks() -> Result<Vec<Attached>> {
    let volumes = vdisk::image_volumes();
    Ok(vdisk::attached_images()?
        .into_iter()
        .map(|file| {
            // Re-opening the image only yields its device path when elevated; it's informational.
            let phys = vdisk::VirtualDisk::open_query(&file).and_then(|d| d.physical_path()).ok();
            let mounts = mounts_of(&file, &volumes);
            Attached { file, phys, mounts }
        })
        .collect())
}

/// Detach every attachment of `path` (a file can end up attached more than once).
fn detach_file(path: &Path) -> Result<()> {
    let elevated = winutil::is_elevated();
    let mut failures = 0;
    for _ in 0..12 {
        let disk = vdisk::VirtualDisk::open_query(path).with_context(|| format!("opening {}", path.display()))?;
        if let Err(e) = disk.detach() {
            if !still_attached(path) {
                return Ok(());
            }
            failures += 1;
            // Elevated, "device not ready" right after a previous detach is transient
            // (Windows is still tearing the last attachment down): wait and retry.
            if !elevated {
                // The storage service can dismount ISOs for standard users; only ask for
                // admin rights (via the returned error) if that does not work either.
                if dismount_by_device(path).is_ok() {
                    return Ok(());
                }
                return Err(anyhow::Error::new(e).context("detaching virtual disk"));
            }
            if failures > 4 {
                return dismount_by_device(path).map_err(|fe| {
                    anyhow::Error::new(e).context(format!("detaching virtual disk (device fallback also failed: {fe:#})"))
                });
            }
            std::thread::sleep(Duration::from_millis(750));
            continue;
        }
        if !still_attached(path) {
            return Ok(());
        }
    }
    bail!("{} is still attached after repeated detach attempts", path.display())
}

/// Last resort: dismount each device the file backs, one by one.
fn dismount_by_device(path: &Path) -> Result<()> {
    let devices = vdisk::devices_of(path);
    if devices.is_empty() {
        bail!("no device with a volume found for {}", path.display());
    }
    let mut last_err = None;
    for dev in &devices {
        if let Err(e) = vdisk::dismount_device(dev) {
            last_err = Some(anyhow!("{dev}: {e}"));
        }
    }
    if !still_attached(path) {
        return Ok(());
    }
    Err(last_err.unwrap_or_else(|| anyhow!("{} is still attached", path.display())))
}

fn still_attached(path: &Path) -> bool {
    vdisk::attached_images().map(|v| v.iter().any(|a| state::same_file(a, path))).unwrap_or(false)
}

fn detach_record(rec: &Record) -> Result<()> {
    detach_file(&rec.attached)?;
    forget_record(rec)
}

/// Remove a mount's record (under the lock) and delete its cached files.
fn forget_record(rec: &Record) -> Result<()> {
    State::modify(|st| st.remove_attached(&rec.attached))?;
    cleanup_record_files(rec);
    Ok(())
}

fn cleanup_record_files(rec: &Record) {
    if rec.temp {
        remove_with_retry(&rec.attached);
    }
    for d in &rec.extra {
        remove_with_retry(d);
    }
    // The audio-tracks drive goes with its disc.
    for c in &rec.companions {
        if still_attached(c) {
            let _ = detach_file(c);
        }
        remove_with_retry(c);
    }
}

/// Delete a file or folder, retrying while Windows releases the image.
fn remove_with_retry(p: &Path) {
    for _ in 0..20 {
        let r = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
        if r.is_ok() || !p.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn unmount(target: &str, everything: bool) -> Result<i32> {
    // Playlists and multi-disc EBOOTs: unmount every disc.
    if Path::new(target).is_file() {
        let image = absolute(Path::new(target))?;
        if let Some(discs) = formats::multi(&image)? {
            let mut code = 0;
            for d in discs {
                match unmount(&d.to_string_lossy(), everything) {
                    Ok(c) => code = code.max(c),
                    Err(e) => {
                        eprintln!("{}: {e:#}", d.display());
                        code = code.max(1);
                    }
                }
            }
            return Ok(code);
        }
    }
    let r = unmount_inner(target, everything);
    match r {
        Err(e) if vdisk::is_access_error(&e) && !winutil::is_elevated() => {
            announce_elevation("unmount");
            let t = if Path::new(target).exists() { absolute(Path::new(target))?.to_string_lossy().into_owned() } else { target.to_string() };
            let mut args = vec!["unmount".to_string(), t, "--pause".into()];
            if everything {
                args.push("--everything".into());
            }
            Ok(winutil::run_elevated(&args)? as i32)
        }
        other => other,
    }
}

fn unmount_inner(target: &str, everything: bool) -> Result<i32> {
    let st = State::load();

    if target.eq_ignore_ascii_case("all") {
        let cache = state::cache_dir()?;
        let mut failed = 0;
        let mut skipped = Vec::new();
        for d in attached_disks()? {
            let file = d.file;
            if !still_attached(&file) {
                continue; // already detached together with its disc (an audio drive)
            }
            let rec = st.by_attached(&file).cloned();
            // Only images rustimgmount mounted (recorded, or living in its cache), unless
            // asked otherwise: an ISO opened from Explorer or a VHD attached by another
            // program (Hyper-V, backup software, ...) is not ours to eject.
            let ours = rec.is_some() || state::is_inside(&file, &cache);
            if !ours && !everything {
                skipped.push(file);
                continue;
            }
            let res = match &rec {
                Some(r) => detach_record(r),
                None => detach_file(&file).map(|_| {
                    if state::is_inside(&file, &cache) {
                        remove_with_retry(&file);
                    }
                }),
            };
            match res {
                Ok(()) => println!("Unmounted {}", rec.map(|r| r.source).unwrap_or(file).display()),
                Err(e) => {
                    if vdisk::is_access_error(&e) {
                        return Err(e);
                    }
                    eprintln!("failed to unmount {}: {e:#}", file.display());
                    failed += 1;
                }
            }
        }
        if !skipped.is_empty() {
            println!("Left {} image(s) mounted that were not mounted by rustimgmount:", skipped.len());
            for f in &skipped {
                println!("  {}", f.display());
            }
            println!("(use `rustimgmount unmount all --everything` to unmount those too)");
        }
        return Ok(if failed > 0 { 1 } else { 0 });
    }

    // Drive letter or mount folder?
    let t = target.trim_end_matches('\\');
    let is_drive = t.len() <= 2 && t.chars().next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false) && (t.len() == 1 || t.ends_with(':'));
    if is_drive {
        let letter = format!("{}:\\", t.chars().next().unwrap().to_ascii_uppercase());
        let disk = attached_disks()?
            .into_iter()
            .find(|d| d.mounts.iter().any(|m| m.eq_ignore_ascii_case(&letter)))
            .ok_or_else(|| anyhow!("{letter} is not a mounted disk image"))?;
        return unmount_file(&st, &disk.file);
    }

    let p = absolute(Path::new(target))?;
    unmount_file(&st, &p)
}

fn unmount_file(st: &State, p: &Path) -> Result<i32> {
    let attached = vdisk::attached_images()?;
    let is_attached = |f: &Path| attached.iter().any(|a| state::same_file(a, f));
    if let Some(rec) = st.by_source(p).cloned().or_else(|| st.by_attached(p).cloned()) {
        if !is_attached(&rec.attached) {
            // Detached elsewhere (e.g. Explorer's "Eject"): just tidy up.
            forget_record(&rec)?;
            println!("{} was not mounted (cleaned up).", rec.source.display());
            return Ok(0);
        }
        detach_record(&rec)?;
        println!("Unmounted {}", rec.source.display());
        return Ok(0);
    }
    // No record (e.g. lost state): the image itself, or one of its predictable cache files.
    let mut target = None;
    if is_attached(p) {
        target = Some(p.to_owned());
    } else {
        for ext in ["iso", "vhd", "vhdx"] {
            if let Ok(c) = cache_path(p, ext) {
                if let Some(a) = attached.iter().find(|a| state::same_file(a, &c)) {
                    target = Some(a.clone());
                }
            }
        }
        if let Ok(dir) = cache_path(p, "d") {
            target = target.or_else(|| attached.iter().find(|a| state::is_inside(a, &dir)).cloned());
        }
    }
    let Some(target) = target else { bail!("{} is not mounted", p.display()) };
    detach_file(&target)?;
    // If it was one of our cache files, remove it (and its decompression folder) too.
    let cache = state::cache_dir()?;
    if state::is_inside(&target, &cache) {
        remove_with_retry(&target);
        if let Ok(dir) = cache_path(p, "d") {
            remove_with_retry(&dir);
        }
    }
    println!("Unmounted {}", p.display());
    Ok(0)
}

fn list(json: bool) -> Result<i32> {
    let st = State::load();
    let disks = attached_disks()?;
    if json {
        let items: Vec<serde_json::Value> = disks
            .iter()
            .map(|d| {
                let rec = st.by_attached(&d.file);
                let audio_of = st.by_companion(&d.file);
                serde_json::json!({
                    "name": audio_of.map(|r| format!("{} (audio tracks)", r.source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())),
                    "source": rec.map(|r| r.source.display().to_string()).unwrap_or_else(|| d.file.display().to_string()),
                    "format": rec.map(|r| r.format.clone()),
                    "ours": rec.is_some() || state::cache_dir().map(|c| state::is_inside(&d.file, &c)).unwrap_or(false),
                    "mounts": d.mounts,
                    "file": d.file.display().to_string(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&items)?);
        return Ok(0);
    }
    if disks.is_empty() {
        println!("No disk images are mounted.");
        return Ok(0);
    }
    for d in disks {
        let name = match (st.by_attached(&d.file), st.by_companion(&d.file)) {
            (Some(r), _) => format!("{}  [{}]", r.source.display(), r.format),
            (None, Some(r)) => format!("{}  [audio tracks: WAV files + ALBUM.CUE]", r.source.display()),
            (None, None) => d.file.display().to_string(),
        };
        let mounts = if d.mounts.is_empty() { "(no drive letter)".to_string() } else { d.mounts.join(", ") };
        println!("{mounts:<18} {name}");
        if let Some(p) = d.phys {
            println!("{:<18} device: {p}", "");
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------- CD audio

fn audio_cmd(image: &Path, output: Option<PathBuf>) -> Result<i32> {
    let image = absolute(image)?;
    let r = (|| -> Result<()> {
        let target = if formats::expand::kind(&image)?.is_some() { formats::expand::expand(&image)? } else { image.clone() };
        let tracks = formats::audio::tracks(&target)?;
        if tracks.is_empty() {
            bail!("no audio tracks found in {} (audio export works for CUE/BIN, GDI, CHD, MDS/MDF and NRG)", image.display());
        }
        let stem = image.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "disc".into());
        // An output ending in .iso gets the audio disc image (WAV files + ALBUM.CUE) instead of a folder.
        if let Some(iso) = output.as_ref().filter(|o| formats::ext(o) == "iso") {
            if iso.exists() {
                bail!("{} already exists", iso.display());
            }
            convert::check_free_space(iso, formats::audio::iso_size(&tracks))?;
            formats::audio::write_iso(&tracks, &stem, iso, false)?;
            println!("Wrote {} ({} audio tracks)", iso.display(), tracks.len());
            return Ok(());
        }
        let dir = output.unwrap_or_else(|| image.with_file_name(format!("{stem} (audio)")));
        let total: u64 = tracks.iter().map(|t| t.frames * 2352).sum();
        convert::check_free_space(&dir, total)?;
        for (t, f) in tracks.iter().zip(formats::audio::export(&tracks, &dir)?) {
            let secs = t.seconds() as u64;
            println!("  {}  ({}:{:02})", f.display(), secs / 60, secs % 60);
        }
        println!("Saved {} audio track(s) to {}", tracks.len(), dir.display());
        Ok(())
    })();
    for d in formats::expand::take_created() {
        remove_with_retry(&d);
    }
    r.map(|_| 0)
}

// ---------------------------------------------------------------- setup

/// Copy `src` over `dst`. A running copy can't be overwritten but can be renamed, so it is
/// moved aside (and deleted by a later setup once it has exited).
fn install_file(src: &Path, dst: &Path) -> Result<()> {
    if std::fs::copy(src, dst).is_ok() {
        return Ok(());
    }
    if dst.exists() {
        let aside = dst.with_extension(format!("{}.old", std::process::id()));
        std::fs::rename(dst, &aside).with_context(|| format!("replacing {}", dst.display()))?;
    }
    std::fs::copy(src, dst).with_context(|| format!("copying to {}", dst.display()))?;
    Ok(())
}

/// Run the installed rustimgmount.exe with `args`, showing its output.
fn run_installed(exe: &Path, args: &[&str]) -> Result<()> {
    let st = std::process::Command::new(exe).args(args).status()?;
    if !st.success() {
        bail!("`rustimgmount {}` failed", args.join(" "));
    }
    Ok(())
}

fn setup(no_menu: bool) -> Result<i32> {
    let here = std::env::current_exe()?;
    let src_dir = here.parent().ok_or_else(|| anyhow!("cannot locate the program folder"))?.to_owned();
    let dest = winutil::install_dir();
    std::fs::create_dir_all(&dest)?;
    let installing_from_elsewhere = !state::same_file(&src_dir, &dest);
    if installing_from_elsewhere {
        install_file(&here, &dest.join("rustimgmount.exe"))?;
        let w = src_dir.join("rustimgmountw.exe");
        if w.exists() {
            install_file(&w, &dest.join("rustimgmountw.exe"))?;
        } else {
            eprintln!("warning: rustimgmountw.exe is missing next to rustimgmount.exe; double-click will show a console window");
        }
        let dll = src_dir.join(winutil::SHELL_DLL);
        if dll.exists() {
            install_file(&dll, &dest.join(winutil::SHELL_DLL))?;
        }
    }
    // Remove copies set aside by earlier setups once they are no longer running.
    for e in std::fs::read_dir(&dest)?.flatten() {
        if e.path().extension().is_some_and(|x| x == "old") {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let exe = dest.join("rustimgmount.exe");
    println!("Installed to {}", dest.display());
    winutil::register_uninstaller(&dest)?;
    if winutil::add_to_path(&dest)? {
        println!("Added to your PATH (open a new terminal to use `rustimgmount` anywhere).");
    }
    // Register from the installed copy so Explorer points there, not at this one.
    run_installed(&exe, &["associate", "--no-settings"])?;
    if !no_menu {
        // Windows 11: one "rustimgmount" entry in the main right-click menu. It needs the
        // menu DLL and Developer Mode; where it can't be had, the classic entries under
        // "Show more options" are installed instead. Never both: they would repeat each other.
        let mut modern = false;
        if dest.join(winutil::SHELL_DLL).exists() && dest.join("rustimgmountw.exe").exists() {
            // Registering the package makes Windows close the programs in it, the tray app included.
            let tray = winutil::tray_running();
            let result = winutil::register_modern_menu(&dest);
            if tray && !winutil::tray_running() {
                winutil::start_tray(&dest);
            }
            match result {
                Ok(()) => {
                    modern = true;
                    winutil::uninstall_shell()?;
                    println!("Right-click menu: \"rustimgmount\" (Mount, Unmount, Convert) added to the Windows 11 menu.");
                }
                Err(e) => println!(
                    "Windows 11 menu: not added ({e}).
  It needs Developer Mode: Settings > System > For developers. Turn it on and run setup again."
                ),
            }
        }
        if !modern {
            run_installed(&exe, &["install"])?;
        }
    } else {
        winutil::unregister_modern_menu();
    }
    let w = dest.join("rustimgmountw.exe");
    if w.exists() {
        winutil::create_shortcut(&winutil::start_menu_shortcut(), &w, "--tray", "Shows mounted disk images in the notification area")?;
        println!("Start Menu: \"rustimgmount\" opens the tray app.");
    }
    println!("\nSetup complete. Undo it any time with `rustimgmount setup --remove`.");
    Ok(0)
}

fn setup_remove(purge: bool, keep_files: bool) -> Result<i32> {
    let dest = winutil::install_dir();
    let installed = dest.join("rustimgmount.exe");
    let exe = if installed.exists() { installed } else { std::env::current_exe()? };
    let _ = run_installed(&exe, &["unassociate"]);
    let _ = run_installed(&exe, &["uninstall"]);
    winutil::unregister_modern_menu();
    winutil::unregister_uninstaller();
    winutil::set_autostart(false)?;
    let _ = std::fs::remove_file(winutil::start_menu_shortcut());
    winutil::remove_from_path(&dest)?;
    // Stop the tray app if it runs from the install folder.
    let _ = std::process::Command::new("taskkill").args(["/F", "/IM", "rustimgmountw.exe"]).output();
    if purge {
        if vdisk::attached_images().map(|v| v.iter().any(|a| state::is_inside(a, &state::app_dir()))).unwrap_or(true) {
            eprintln!("note: images are still mounted from the cache; keeping {}", state::app_dir().display());
        } else {
            let _ = std::fs::remove_dir_all(state::app_dir());
        }
    }
    // A running program can't delete its own folder: let a short-lived helper do it.
    let here = std::env::current_exe()?;
    if keep_files {
        // The installer put the files there and removes them itself.
    } else if here.starts_with(&dest) || state::is_inside(&here, &dest) {
        let cmd = format!("ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dest.display());
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd.exe").args(["/c", &cmd]).creation_flags(0x0800_0000).spawn();
    } else if dest.exists() {
        std::fs::remove_dir_all(&dest).with_context(|| format!("deleting {}", dest.display()))?;
    }
    println!("rustimgmount setup removed{}.", if purge { " (records and cache deleted)" } else { "" });
    Ok(0)
}

fn info(image: &Path) -> Result<i32> {
    let r = info_inner(image);
    // Archives (zip/7z/gz/...) are extracted to look inside them; nothing is mounted,
    // so the extracted copy goes straight away.
    for d in formats::expand::take_created() {
        remove_with_retry(&d);
    }
    r
}

fn info_inner(image: &Path) -> Result<i32> {
    let image = absolute(image)?;
    if let Some(discs) = formats::multi(&image)? {
        println!("File:    {}", image.display());
        println!("Discs:   {} (all are mounted together; `info <file>#N` shows one)", discs.len());
        for (i, d) in discs.iter().enumerate() {
            println!("  {}: {}", i + 1, d.display());
        }
        return Ok(0);
    }
    let det = formats::detect(&image)?;
    println!("File:    {}", image.display());
    println!("Format:  {}", det.format);
    if !same_path(&det.data_path, &image) {
        println!("Data:    {}", det.data_path.display());
    }
    match det.plan {
        Plan::Native(k) => println!("Mount:   natively as {}", k.ext().to_uppercase()),
        Plan::Cd(disc) => {
            for line in disc.describe() {
                println!("Track:   {line}");
            }
            println!("Size:    {}", util::human(source::Source::len(&disc)));
            println!("Mount:   {}", if disc.plain_file.is_some() { "as ISO (no conversion needed)" } else { "converted to ISO" });
            if let Ok(a) = formats::audio::tracks(&det.data_path) {
                if !a.is_empty() {
                    let secs: f64 = a.iter().map(|t| t.seconds()).sum();
                    println!("Audio:   {} track(s), {}:{:02} (save as WAV with `rustimgmount audio`)", a.len(), secs as u64 / 60, secs as u64 % 60);
                }
            }
        }
        Plan::Disk(s) => {
            println!("Size:    {} (virtual)", util::human(s.len()));
            println!("Mount:   converted to a sparse VHD");
        }
    }
    Ok(0)
}

struct ConvertOpts {
    out_dir: Option<PathBuf>,
    overwrite: bool,
    verify: bool,
    delete_original: bool,
}

fn convert_cmd(image: &Path, output: Option<PathBuf>, to: Option<String>, opts: ConvertOpts) -> Result<i32> {
    use export::Format;
    let image = absolute(image)?;
    let format = match &to {
        Some(t) => Some(Format::parse(t).ok_or_else(|| anyhow!("unknown format '{t}'; choose one of: {}", Format::ids()))?),
        None => output.as_deref().and_then(|o| Format::parse(&formats::ext(o))),
    };
    if let Some(discs) = formats::multi(&image)? {
        if output.is_some() {
            bail!("{} holds {} discs, which are each converted to a file named after them; leave out the output file", image.display(), discs.len());
        }
        for d in &discs {
            convert_one(&absolute(d)?, None, format, &opts, false)?;
        }
        if opts.delete_original {
            println!("The original was kept: {} holds several discs.", image.display());
        }
        return Ok(0);
    }
    convert_one(&image, output, format, &opts, opts.delete_original)?;
    Ok(0)
}

fn convert_one(image: &Path, output: Option<PathBuf>, format: Option<&'static export::Format>, opts: &ConvertOpts, delete: bool) -> Result<()> {
    use export::Target;
    // VHD/VHDX output is for Windows to mount, so disks get the same preparation as when
    // mounting. Every other format gets the disk's contents exactly as they are.
    let exact = format.is_some_and(|f| !matches!(f.target, Target::Vhd | Target::Vhdx));
    formats::disk::EXACT.store(exact, std::sync::atomic::Ordering::Relaxed);
    let det = formats::detect(image);
    // Decompressed copies are only needed while converting.
    let created = formats::expand::take_created();
    let result = convert_detected(image, det, output, format, opts);
    for d in created {
        remove_with_retry(&d);
    }
    let written = result?;
    if delete {
        match export::source_files(image) {
            // (The output can never be one of these: the converter refuses to write over its source.)
            Some(files) if !files.iter().any(|f| written.iter().any(|w| state::same_file(f, w))) => {
                winutil::recycle(&files)?;
                for f in &files {
                    println!("Moved to the Recycle Bin: {}", f.display());
                }
            }
            _ => println!("The original was kept: it is made of several files, and rustimgmount only removes originals it can list completely."),
        }
    }
    Ok(())
}

fn convert_detected(
    image: &Path,
    det: Result<formats::Detected>,
    output: Option<PathBuf>,
    format: Option<&'static export::Format>,
    opts: &ConvertOpts,
) -> Result<Vec<PathBuf>> {
    use export::{Format, Target};
    let (base, disc) = formats::split_disc(image);
    let format = match (format, &det) {
        (Some(f), _) => f,
        (None, Err(_)) => Format::of(Target::Iso), // reports the detection error
        (None, Ok(d)) => match &d.plan {
            Plan::Native(k) if same_path(&d.data_path, image) => {
                bail!("{} is already a {} image that Windows can mount directly; pick another format with --to", image.display(), k.ext().to_uppercase())
            }
            Plan::Native(k) => Format::parse(k.ext()).unwrap_or(Format::of(Target::Iso)),
            Plan::Cd(_) => Format::of(Target::Iso),
            Plan::Disk(s) => Format::parse(convert::disk_format(s.len()).ext()).unwrap_or(Format::of(Target::Vhd)),
        },
    };
    let target = format.target;
    // Same format: only a different flavour of it (CHD recompressed with Zstandard, a VHD
    // made fixed-size, ...) is a real conversion. It gets a name of its own.
    let same_format = formats::ext(&base) == target.ext();
    let flavour = match format.variant {
        // The standard flavour of a format that has others: converting back to it.
        export::Variant::Plain => match target {
            Target::Chd => Some("lzma"),
            Target::Vhd => Some("dynamic"),
            Target::Qcow2 => Some("uncompressed"),
            _ => None,
        },
        export::Variant::Zstd => Some("zstd"),
        export::Variant::Fixed => Some("fixed"),
        export::Variant::Compressed => Some("compressed"),
    };
    if same_format && flavour.is_none() {
        bail!("{} is already in {} format", image.display(), target.ext().to_uppercase());
    }
    let out = match output {
        Some(o) => o,
        None => {
            let mut stem = base.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            // game.iso.gz -> game, not game.iso
            if matches!(formats::ext(&base).as_str(), "gz" | "xz" | "bz2" | "ecm") {
                if let Some((s, e)) = stem.rsplit_once('.') {
                    if e.len() <= 5 && !s.is_empty() {
                        stem = s.to_string();
                    }
                }
            }
            if let Some(d) = disc {
                stem.push_str(&format!(" (Disc {})", d + 1));
            }
            if let (true, Some(f)) = (same_format, flavour) {
                stem.push_str(&format!(" ({f})"));
            }
            let name = format!("{stem}.{}", target.ext());
            match &opts.out_dir {
                Some(dir) => {
                    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
                    dir.join(name)
                }
                None => base.with_file_name(name),
            }
        }
    };
    if target == Target::Vhd {
        if let Ok(formats::Detected { plan: Plan::Disk(s), .. }) = &det {
            if s.len() > convert::VHD_MAX {
                bail!("this disk is larger than VHD's 2040 GiB limit; convert it to vhdx instead");
            }
        }
    }
    let eopts = export::Opts { overwrite: opts.overwrite, verify: opts.verify };
    let written = export::run(image, det, format, &out, eopts).map_err(|e| {
        if convert::disk_full(&e) {
            anyhow!("ran out of disk space while writing {} (the partial file was deleted)", out.display())
        } else {
            e
        }
    })?;
    for f in written.iter().rev() {
        println!("Wrote {}", f.display());
    }
    if opts.verify {
        println!("Verified: it reads back identical to the original.");
    }
    Ok(written)
}

/// What the Convert dialog offers for a selection of images: the formats every one of
/// them can be converted to.
fn formats_cmd(images: &[PathBuf]) -> Result<i32> {
    let mut common: Option<Vec<&'static export::Format>> = None;
    let mut errors = Vec::new();
    for image in images {
        let kind = absolute(image).and_then(|p| export::kind_of(&p).map(|k| (p, k)));
        for d in formats::expand::take_created() {
            remove_with_retry(&d);
        }
        match kind {
            Ok((p, kind)) => {
                let mine = export::choices(kind, &formats::ext(&formats::split_disc(&p).0));
                common = Some(match common {
                    None => mine,
                    Some(c) => c.into_iter().filter(|f| mine.iter().any(|m| m.id == f.id)).collect(),
                });
            }
            Err(e) => errors.push(serde_json::json!({ "image": image.display().to_string(), "error": format!("{e:#}") })),
        }
    }
    let formats: Vec<_> = common.unwrap_or_default().iter().map(|f| serde_json::json!({ "id": f.id, "label": f.label })).collect();
    println!("{}", serde_json::json!({ "formats": formats, "errors": errors }));
    Ok(0)
}

/// Start a windowless background `rustimgmount watch` for a mounted converted image.
fn spawn_watcher(attached: &Path) {
    // Best effort: without the helper, cleanup still happens on the next rustimgmount command.
    let _ = winutil::spawn_detached(&["watch".to_string(), attached.to_string_lossy().into_owned()]);
}

/// Background helper: poll until `file` is no longer attached, then clean up and exit.
/// Exits quietly if the mount is removed through rustimgmount itself.
fn watch(file: &Path) -> Result<i32> {
    loop {
        std::thread::sleep(Duration::from_secs(2));
        if State::load().by_attached(file).is_none() {
            return Ok(0); // unmounted (and cleaned up) by rustimgmount
        }
        if let Ok(attached) = vdisk::attached_images() {
            if !attached.iter().any(|a| state::same_file(a, file)) {
                purge_ejected();
                return Ok(0);
            }
        }
    }
}

/// Images detached outside rustimgmount (Explorer's Eject, Dismount-DiskImage, ...): drop
/// their records and delete their converted copies right away. Only recorded mounts are
/// touched; unrecorded cache files (e.g. a conversion waiting for a UAC retry) keep the
/// one-hour grace period in `clean_cache`.
fn purge_ejected() {
    // Look at what is attached *inside* the lock: a mount that finishes concurrently
    // records itself under the same lock, after attaching, so it is never seen as ejected.
    let gone = State::modify(|st| {
        let Ok(attached) = vdisk::attached_images() else { return Vec::new() };
        let (gone, kept): (Vec<Record>, Vec<Record>) =
            st.mounts.drain(..).partition(|r| !attached.iter().any(|a| state::same_file(a, &r.attached)));
        st.mounts = kept;
        gone
    });
    for rec in gone.iter().flatten() {
        cleanup_record_files(rec);
    }
}

/// Remove cache files (and stale records) whose disks are no longer attached.
/// When `all` is false, recent files are kept so a retry (e.g. after a UAC prompt)
/// can reuse a conversion.
fn clean_cache(all: bool) -> Result<usize> {
    // Records of mounts that are gone are dropped (and their files deleted) first.
    purge_ejected();
    // If we cannot enumerate attached disks, do nothing rather than guess.
    let mut attached = vdisk::attached_images()?;
    // An audio-tracks drive whose disc is gone (e.g. an unmount was interrupted half-way):
    // detach it. The file itself is then removed by the sweep below.
    let cache = state::cache_dir()?;
    let st = State::load();
    let orphans: Vec<PathBuf> = attached
        .iter()
        .filter(|a| state::is_inside(a, &cache) && a.to_string_lossy().to_lowercase().ends_with(".audio.iso") && st.by_companion(a).is_none())
        .cloned()
        .collect();
    for o in orphans {
        if detach_file(&o).is_ok() {
            remove_with_retry(&o);
            attached.retain(|a| a != &o);
        }
    }
    let is_attached = |p: &Path| attached.iter().any(|a| state::same_file(a, p));
    let verbose = all;
    let mut n = 0;
    for e in std::fs::read_dir(state::cache_dir()?)?.flatten() {
        let p = e.path();
        // Folders (decompressed archives) are in use if an attached image lives inside.
        if is_attached(&p) || attached.iter().any(|a| state::is_inside(a, &p)) {
            continue;
        }
        let recent = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|age| age < Duration::from_secs(3600))
            .unwrap_or(false);
        // Half-written conversions (*.partial*) are locked while being written, so one we
        // can delete belongs to a cancelled or crashed run: remove it whatever its age.
        let partial = p.file_name().is_some_and(|n| n.to_string_lossy().contains(".partial"));
        if !all && recent && !partial {
            continue;
        }
        let removed = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
        if removed.is_ok() {
            n += 1;
            if verbose {
                println!("removed {}", p.display());
            }
        }
    }
    Ok(n)
}
