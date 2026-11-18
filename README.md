# rustimgmount

A Windows disk image mounter, written in Rust. It handles everyday images (ISO, VHD, IMG) as well as
virtual-machine disks and the formats emulators use (CHD, GDI, CSO, ECM, ...). Use it from the
command line, by double-clicking images, or from its notification-area (tray) icon.

**Quick start:** build it (see [Building](#building)), then run `rustimgmount setup` once. That installs it
for your user account and sets up double-click mounting and the Start Menu entry for the tray app.

```
rustimgmount mount "Sonic Adventure (USA).chd"
Format: CHD (CD/DVD)
Converting to ISO: 100%  (1.1 GiB / 1.1 GiB, 1.6 GiB/s)
Mounted C:\Games\Sonic Adventure (USA).chd (read-only)
  -> E:\
```

## Supported formats

### Optical discs (CD / DVD / GD-ROM)

| Format | Extensions | Notes |
|---|---|---|
| ISO 9660 / UDF | `.iso` (also hybrid `.img`) | mounted natively, no conversion |
| Raw CD images | `.bin`, `.img`, `.raw`, `.b5t`, ... | 2048/2336/2352/2448-byte sectors, mode 1 and mode 2 |
| CUE sheets | `.cue` | multi-track and multi-file; audio tracks can be saved as WAV (see [CD audio](#cd-audio)) |
| MAME CHD | `.chd` | v1â€“v5; zlib, LZMA, Huffman, FLAC, zstd and the CD codecs; CD, GD-ROM and DVD; delta CHDs whose parent `.chd` is in the same folder |
| Dreamcast GDI | `.gdi` | high-density area placed at LBA 45000, data split across tracks handled |
| DiscJuggler | `.cdi` | including Dreamcast multi-session images |
| Alcohol 120% | `.mds` + `.mdf` | track table parsed; split images (`.mdf` + `.md0`/`.md1`/...) joined |
| Alcohol / DAEMON Tools v2 | `.mdx`, v2 `.mds` | encrypted descriptor and compressed track data; password-protected images not supported |
| UltraISO | `.isz` | zlib/bzip2 chunks; encrypted and multi-part (`.i01`) ISZ not supported |
| PSP EBOOT | `.pbp` | PlayStation discs converted by popstation / PSX2PSP / psxpackager; multi-disc EBOOTs mount every disc. Official (encrypted) PSN EBOOTs are not supported |
| Playlists | `.m3u` | mounts every disc listed (multi-disc PS1/Saturn/Sega CD sets) |
| Nero | `.nrg` | v1 and v2, DAO and TAO |
| CloneCD | `.ccd` + `.img` | |
| CSO / ZSO / DAX | `.cso`, `.zso`, `.dax` | PSP/PS2 compressed ISOs (CSO v1/v2) |
| ECM | `.ecm` (e.g. `game.bin.ecm`) | also used automatically when a `.cue` points at `game.bin` and only `game.bin.ecm` exists |

### Hard disks and floppies

| Format | Extensions |
|---|---|
| Virtual Hard Disk | `.vhd`, `.vhdx`, `.avhd`, `.avhdx` (mounted natively, read-write by default) |
| Raw disk images | `.img`, `.raw`, `.dd`, `.bin`, `.dsk`, `.001` |
| Floppy / single-partition images | `.img`, `.ima`, `.flp`, `.vfd` (an MBR is added around them) |
| VMware | `.vmdk` (monolithic sparse, streamOptimized, flat, split) |
| VirtualBox | `.vdi` |
| QEMU | `.qcow2` (v2/v3, zlib- or zstd-compressed clusters, backing files) |
| Parallels | `.hdd` |
| MAME CHD | `.chd` hard disk images |
| PC-98 | `.hdi`, `.fdi` (Anex86), `.nhd` (T98-Next): the PC-98 partition table is converted to MBR |
| Atari ST | `.st`, `.msa` (Magic Shadow Archiver) |
| D88 floppies (PC-98, PC-88, FM-7, X1) | `.d88`, `.d77`, `.d98`, `.88d`, `.98d`, `.d68`, `.1dd`: DOS (FAT) disks; game boot disks without a file system have nothing Windows can show |
| Forensic | `.E01` / `.Ex01` (EnCase, multi-segment), split raw `.001` + `.002` + ... |

**Note on PC-98 and Atari ST:** these disks use FAT, but Windows' FAT driver rejects their boot sectors as
written:
- **Atari ST** boot sectors start with a 68000 branch instead of an x86 jump.
- **PC-98** volumes often use 1024- or 2048-byte logical sectors.

rustimgmount fixes the boot sector in the temporary copy only, and your original image is never changed:
- It adds a standard jump instruction, and replaces an invalid media byte if there is one.
- It restates the volume's counts in 512-byte sectors, which leaves every file system structure at the same byte offset.

### Compressed wrappers

Any of the above inside **`.gz`**, **`.xz`**, **`.bz2`**, **`.zip`**, **`.7z`** or **`.rar`** (for example
`game.iso.gz`, or an archive holding a `.cue` and its `.bin` files) is decompressed into the cache first,
then mounted. The decompressed copy is deleted when you unmount.
- **Multi-part RARs:** these work too (`game.part1.rar`, `.r00`...).
- **Password-protected archives:** not supported.
- **Free space:** an archive that unpacks to more than the free space is refused before anything is written.
- **RAR licensing:** RAR support uses RARLAB's official UnRAR code, under the license in `LICENSE-UNRAR.txt`.
  It allows extracting RAR archives in any software, but not creating them.

Formats are recognised by their contents, not just the extension.

### Not supported

These give a clear error message instead:
- **Formats Windows can't read:** Apple `.dmg` (HFS+/APFS), GameCube/Wii/Switch images (`.rvz`, `.wbfs`, `.gcm`, `.nsp`, ...) and Xbox XISO all use file systems Windows has no driver for.
- **Not supported:** encrypted qcow2, and VMDK snapshot (delta) disks (mount the base disk instead).
- **`.wim`:** these are file archives, not disks; use `dism /Mount-Image` instead.

## How it works

Windows can only attach ISO, VHD and VHDX files itself (through the VirtDisk API, the
same one Explorer's "Mount" uses). rustimgmount attaches those directly. For everything else it reads the
image and writes a plain `.iso`, or a sparse dynamic `.vhd`, into `%LOCALAPPDATA%\rustimgmount\cache`. It
then attaches that file. Disks larger than VHD's 2040 GiB limit are written as `.vhdx` instead, up to 64 TiB.

- **Empty space is skipped.** Space a VMDK, VDI or QCOW2 never used isn't even read, so a 2.5 TiB disk that's
  mostly empty converts in well under a second.
- **Free space is checked before converting.**
  - Disc images need exactly their size, so a conversion that can't fit doesn't start.
  - Disks only need their used space, so you get a warning when the worst case wouldn't fit.
  - If the drive does fill up, the half-written file is deleted.
  - To keep conversions on another drive, set the `RUSTIMGMOUNT_CACHE` environment variable to a folder.
- **Disk IDs are fixed when needed.** Windows keeps a second disk offline when its MBR signature or GPT GUID
  matches a disk already in the PC (for example, an image of your own system drive), and it can't fix that on
  a read-only disk. When a converted disk has no signature, or its IDs clash, imgmount gives the copy fresh
  IDs. Only the copy changes, never your original image.
- A plain ISO that only has the wrong extension is hard-linked rather than copied.
- **Multi-session discs and GD-ROMs:** their file system refers to absolute sector numbers. For example,
  a Dreamcast game's data starts at sector 45000, and a CD-Extra's or CDI's data sits in the second
  session. rustimgmount finds where the root directory really is, places each data track at the sector
  number its file system expects, and copies the volume descriptors to sector 16, where Windows
  looks for them.

Converted images are mounted **read-only** by default, because any changes would only reach the
temporary copy. Use `--writable` if you want a scratch disk anyway.

**Ejecting:** the cached copy is removed when you unmount, whether with `rustimgmount unmount` or with
Explorer's right-click **Eject**:
- While a converted image is mounted, a small helper without a window (`rustimgmount watch`) checks every few
  seconds whether it is still attached. It deletes the copy within a few seconds of an eject, then exits.
- If the helper isn't running (for example after a reboot), the next rustimgmount command does the cleanup.
- Copies left by mounts that never completed (for example a declined admin prompt) are deleted by any
  rustimgmount command once they are an hour old. `rustimgmount clean` deletes them straight away.

## Usage

For multi-disc images, `game.pbp#2` (or any `file#N`) addresses one disc; `mount` / `unmount` on the
`.m3u` or `.pbp` itself handles every disc.

```
rustimgmount mount <image> [--read-only] [--writable] [--no-drive-letter] [--refresh] [--open] [--no-audio]
rustimgmount unmount <image | E: | all> [--everything]   # "all" = images rustimgmount mounted;
                                                         # --everything also unmounts other programs' images
rustimgmount list
rustimgmount info <image>               # show the format, tracks and plan, without mounting
rustimgmount convert <image> --to FORMAT [output] [--verify] [--delete-original] ...   # see "Converting" below
rustimgmount clean                      # delete cached conversions that are not mounted
rustimgmount install                    # add "Mount", "Unmount" and "Convert to" to Explorer's right-click menu
rustimgmount uninstall
rustimgmount associate                  # double-clicking a supported image mounts it and opens the drive
rustimgmount unassociate                # undo, restoring your previous defaults
rustimgmount audio <image> [folder]     # save a disc's CD audio tracks as WAV files
rustimgmount setup [--no-menu]          # install for your user account (see below)
rustimgmount setup --remove [--purge]   # undo setup (--purge also deletes records and cache)
```

You can also drag an image onto `rustimgmount.exe`.

**Setup.** `rustimgmount setup` installs the program for your user account, with no admin rights needed:
- It copies both programs to `%LOCALAPPDATA%\Programs\rustimgmount` and adds that folder to your PATH.
- It sets up double-click mounting and the right-click menu (skip the menu with `--no-menu`).
- It adds a Start Menu entry, **rustimgmount**, that starts the tray app.

Run it again after rebuilding to update the installed copy. `setup --remove` undoes everything.

**Tray app.** The Start Menu entry (or `rustimgmountw --tray`) puts a disc icon in the notification area.
Click it for a menu of what's mounted (open or unmount each image), **Mount image...**, **Mount recent**
(the last ten images you mounted), **Convert image...** (opens the Convert dialog), **Unmount all** and
**Start with Windows**.

**Progress.** Mounting from Explorer, the tray or the right-click menu shows no window when it's quick. If
it takes more than about 1.5 seconds (converting a big image), a small progress dialog appears with a
**Cancel** button. Cancelling stops the work and deletes the half-written copy.

### Converting between formats

Right-click an image, open **rustimgmount** and pick **Convert to ...** with the format. The new file
appears next to the original, with the same name. **More formats and options...** opens the Convert
dialog; files whose extension is used for both discs and disks (`.img`, `.raw`, `.001`, `.e01`) only get
that entry, because the dialog looks inside the file first. (With the classic menu, the same choices are
under **Convert to**.)

**The Convert dialog** looks inside the image first and lists only the formats that fit it. There you can also:
- choose another folder for the result,
- have the new image checked against the original when it's done,
- have the original moved to the Recycle Bin once that check has passed,
- allow existing files to be replaced.

**Several images at once.** Select any number of images and convert them in one go. They share one dialog
and one queue: one conversion runs at a time (each already uses every CPU core), with a single progress
window that says which image it's on. **Cancel** stops the current one, deletes its half-written file and
drops the rest. Failures are listed together at the end, and a notification tells you when a long or
multi-image job has finished. Images added while the queue is running join it. Conversions that were still waiting when the PC was shut down or
the queue was killed are dropped, not picked up again later.

From a terminal:

```
rustimgmount convert <image> --to <format> [output] [--out-dir FOLDER] [--verify] [--delete-original] [--overwrite]
```

| Format | `--to` | What it holds |
|---|---|---|
| ISO | `iso` | The disc's data track, as Windows mounts it. Audio tracks are not included |
| CSO | `cso` | The same, compressed (PSP and PS2 emulators and loaders) |
| ZSO | `zso` | The same, compressed with LZ4: bigger than CSO, quicker to read |
| CHD | `chd` | The whole disc: every track, audio included, compressed. Also DVDs and hard disks |
| CHD, Zstandard | `chd-zstd` | The same with Zstandard instead of LZMA: a little bigger, much quicker for emulators to read. Older emulators can't open it |
| BIN/CUE | `cue` | The whole disc: every track, uncompressed, with a cue sheet |
| CCD/IMG/SUB | `ccd` | The whole disc in CloneCD's format, including subchannel data |
| GDI | `gdi` | A Dreamcast GD-ROM as track files with a `.gdi` (from a GD-ROM CHD) |
| VHD, VHDX | `vhd`, `vhdx` | Hyper-V and Windows disks, growing as needed |
| VHD, fixed | `vhd-fixed` | A VHD with all its space allocated (some tools and Azure need this) |
| IMG | `img` | A raw disk, byte for byte |
| VMDK | `vmdk` | VMware (one growable file) |
| VDI | `vdi` | VirtualBox (dynamically allocated) |
| QCOW2 | `qcow2`, `qcow2-compressed` | QEMU, plain or with compressed clusters |

Any supported disc can become any disc format, and any supported disk any disk format, so `chd` to
`cue`, `cue` to `chd`, `iso` to `cso`, `vmdk` to `vdi`, `vhdx` to `qcow2` and so on all work. Images inside
archives are converted straight from the archive. For a playlist or multi-disc EBOOT, every disc is
converted.

- **Recompressing.** An image can be converted to another flavour of its own format: a CHD to
  `chd-zstd`, a VHD to `vhd-fixed`, a QCOW2 to `qcow2-compressed`. The result is named after the flavour,
  for example `game (zstd).chd`.
- **A bare `.bin` of raw sectors** converted to BIN/CUE just gets its missing `.cue` written next to it.
- **CHD output matches chdman.** For CD and GD-ROM images, the result has the same SHA-1 as the file
  `chdman createcd` makes from the same source, and `chdman verify` accepts it. Compression runs on all
  cores.
- **Whole-disc formats keep what's stored.** Track modes, raw sectors, audio and pregaps carry over
  between CUE, CHD, CCD, GDI, MDS/MDF and NRG. Images without a track list (a plain `.iso`, `.cso`,
  `.cdi`...) become a one-track disc.
- **Subchannel data is kept where the format can hold it.** If the source has it (CCD with a `.sub`,
  MDS/MDF or NRG with 2448-byte sectors, a CHD made with subchannels), CHD and CCD output keep it
  unchanged. BIN/CUE and GDI can't hold it. When a CCD is made from an image without subchannel data, the
  `.sub` is filled with the standard position information.
- **LibCrypt (copy-protected PlayStation games).** A `.sbi` or `.lsd` patch file next to the image is
  copied to follow the converted image, which is how emulators find it.
- **Disks are copied exactly**, except to VHD/VHDX. Those are prepared for Windows the same way as when
  mounting (a floppy gets a partition table, a missing disk ID is filled in).
- **Empty space isn't stored** in VHD, VHDX, VMDK, VDI and QCOW2 output.
- **`--verify`** reads the finished image back and compares every track or sector with the original. If
  anything differs, the result is deleted and the conversion fails. It uses rustimgmount's own readers, so
  it catches bad writes and bugs in the writer, not a mistake both sides share.
- **`--delete-original`** implies `--verify`, and only then moves the original to the Recycle Bin: the
  image and the files it's made of (a cue sheet's `.bin` files, a `.gdi`'s tracks, `.mds` with `.mdf`,
  `.ccd` with `.img` and `.sub`). Originals whose parts can't all be listed (split images, multi-disc
  files, VMDK descriptors with extents) are kept, and it says so.
- **Nothing half-written is left behind.** Output is written as `<name>.partial` and renamed when it's
  complete. Cancelling, a failure or a full drive deletes it. An existing file is only replaced with
  `--overwrite`, and a conversion never writes over the image it's reading.

### CD audio

Windows' ISO mount only shows a disc's data. Many PlayStation, Saturn and Sega CD games keep their music in
CD-audio tracks, which a mounted data drive can't show. rustimgmount handles this in two ways.

**Audio drive on mount.** When you mount a disc that has audio tracks, a second read-only drive appears next
to the data drive, labelled "*NAME*_AUDIO". It holds `TRACK02.WAV`, `TRACK03.WAV`, ... (numbered as on the
disc) and `ALBUM.CUE`.
- **Playing it:** open `ALBUM.CUE` in an audio player such as foobar2000, or drag the drive in, and the disc
  plays as one album with its tracks in order. Any player can also open the WAV files directly.
- **Lifetime:** the audio drive belongs to its disc. Unmounting or ejecting the data drive removes both, and
  ejecting only the audio drive leaves the data drive alone.
- **Cost:** it takes cache space while mounted, about 10 MB per minute of music. `mount --no-audio` skips it.
- **Not a real audio CD:** Windows' mounting can't present CD-audio tracks, so a player's "Open Audio CD"
  command and CD rippers won't see it. That would need a virtual CD-drive driver.

**Saving the tracks.** `rustimgmount audio <image>` saves the tracks as standard WAV files (44.1 kHz, 16-bit
stereo), by default into a folder named "*image* (audio)" next to the image. Give an output name ending in
`.iso` to get the audio disc image (WAV files + `ALBUM.CUE`) instead.

Both work for CUE/BIN, GDI, CHD, MDS/MDF and NRG, also inside archives. `info` shows how many audio tracks
a disc has.

**Double-click to mount.** `rustimgmount associate` registers a "Disk image" file type for the current user,
with the program's icon, and points every supported extension at it. Double-clicking an image then mounts
it and opens the new drive in Explorer. No console window appears: Explorer runs `rustimgmountw.exe`, a
windowless companion that must sit next to `rustimgmount.exe`. It runs the mount hidden, and shows an
error dialog only if something goes wrong. A UAC prompt still appears when admin rights are needed, and
the elevated part runs hidden too. The right-click menu uses the same companion.

Some extensions won't switch automatically:
- **Your own earlier choices win.** Where you previously chose an app ("Always use this app"), Windows protects
  that choice from programs. `associate` lists those extensions and opens Settings â†’ Default apps on
  rustimgmount's page, where you can switch them. Windows also asks "How do you want to open this?" the next
  time you double-click one of them, and you can pick rustimgmount there.
- **Not associated on purpose:** `.m3u` (music players use it for playlists) and archives (`.zip`, `.7z`,
  `.gz`...). Mount those with `rustimgmount mount` or the right-click menu.

`unassociate` puts back any per-user default it replaced. If you move the exe, the associations follow it,
the same way the right-click menu does.

**Administrator rights.**
- **Without admin:** you can mount and unmount anything that ends up as an ISO, which covers every CD/DVD/GD-ROM format above.
- **With admin:** Windows requires it to attach VHD files, which covers every converted hard disk image. rustimgmount shows a UAC prompt when it needs one.

**The right-click menu.** `rustimgmount setup` installs one of two menus, never both:

- **Windows 11, with Developer Mode on** (Settings > System > For developers): a single **rustimgmount**
  entry in the menu you get straight away (it also shows under "Show more options"). Its flyout holds
  **Mount** or **Unmount** (whichever applies to the image), **Convert to ...** for the formats that suit
  the file type, and **More formats and options...** for the Convert dialog. Windows only lists commands
  from packaged apps in that menu, so setup registers a small package that points at
  `rustimgmount_shell.dll` in the install folder. The package is unsigned, which is why Developer Mode is
  needed.
- **Otherwise** (Windows 10, Developer Mode off, or the DLL not built): the classic entries under "Show
  more options": **Mount with rustimgmount**, **Unmount (rustimgmount)** and **Convert to** with its list
  of formats. `rustimgmount install` adds these on their own.

Double-click associations add a bold **Mount** at the top of the menu as well; that is Windows showing
the file type's default action. `setup --remove` takes all of it away.

If you move `rustimgmount.exe` after running `install`, the next time you run it the classic menu is updated
to the new location.

## Building

```
cargo build --release --bins
cargo build --release --manifest-path shellext/Cargo.toml --target-dir target/shellext   # Windows 11 menu DLL
copy target\shellext
elease
ustimgmount_shell.dll target
elease\                      # so `setup` finds it
```

This builds two programs in `target\release`, which belong together:
- `rustimgmount.exe` is the command-line tool.
- `rustimgmountw.exe` is the windowless companion for Explorer and the tray.

Neither has runtime dependencies. Building RAR support compiles RARLAB's UnRAR C++ code, which needs the
Visual Studio C++ tools (the same ones Rust on Windows already uses).

**Speed.** CHD and CSO/ZSO/DAX images are decompressed on all CPU cores while converting. On a 24-core PC a
300 MB LZMA-compressed CHD converts at about 310 MB/s, up from 46 MB/s single-threaded.

**Robustness.** Image headers are untrusted input:
- Every size read from a header is checked against hard limits and against the file's real size.
- Anything malformed produces an error message, never a crash.
- `cargo test` includes a fuzz test that throws about 1,300 corrupted images at the program.

## Tests

`cargo test` runs everything: unit tests plus all format suites, in about a minute. The suites need
Python 3 (standard library only). Without Python they're skipped with a message; set
`RUSTIMGMOUNT_SKIP_FIXTURES=1` to skip them on purpose. Mounting disk images for real needs admin
rights, so that check is separate: run `tests/fixtures/admin_mount_test.ps1` as administrator (add
`-Only <name>` to run a subset).

The scripts in `tests/fixtures` generate an image in every supported format from a small ISO and
small FAT disks. Each one is converted with rustimgmount. Disk results are compared byte for byte. CD
results are checked by reading their ISO 9660 file system the way an OS does.
Fixtures that need a real third-party writer are made by Rust examples:
- `examples/mkchd.rs` makes CHDs with the `chd` crate's writer.
- `examples/mkfixtures.rs` makes E01 files with `ewf-image` and 7z files with `sevenz-rust2`.

`convert.py` takes the images the other suites made, converts them between formats and back, and compares
the result with the original byte for byte.

The MDX fixture's encryption is produced by an independent pure-Python AES, checked against the FIPS-197 test vector.

```
cargo build --release --example mkchd --example mkfixtures
python tests/fixtures/more.py target/release/rustimgmount.exe target/release/examples/mkfixtures.exe testdata/more
python tests/fixtures/gen_basic.py testdata
python tests/fixtures/verify_basic.py target/release/rustimgmount.exe testdata
python tests/fixtures/emu.py target/release/rustimgmount.exe target/release/examples/mkchd.exe testdata/emu testdata/disk.img
```

## Project layout

- `src/main.rs`: CLI, the mount and unmount flow, the cache
- `src/vdisk.rs`: VirtDisk API wrapper, drive-letter discovery, device-path dismount
- `src/convert.rs`: ISO, dynamic VHD and VHDX writers, free-space checks
- `src/export.rs`: `convert --to`: CSO/ZSO, CHD, BIN/CUE, CCD, GDI, raw, VMDK, VDI and QCOW2 writers, verification
- `src/formats/`: format detection and readers
  - `cd.rs`: disc assembly, plus ISO/BIN/CUE/GDI/NRG/MDS/CCD/CDI
  - `chd.rs`, `packed.rs` (CSO/ZSO/DAX), `isz.rs`, `pbp.rs`, `mdx.rs`
  - `expand.rs`: gz, xz, bz2, zip, 7z, RAR, ECM
  - `audio.rs`: CD audio track export (WAV)
  - `layout.rs`: full track lists (data and audio) for whole-disc conversion
  - `vhd.rs`: VHD and VHDX readers (used when converting them)
  - `pieces.rs`: split raw, split MDF, E01
  - `disk.rs`: MBR wrapping, FAT boot-sector fixes, PC-98, Atari ST/MSA
  - `vmdk.rs`, `vdi.rs`, `qcow2.rs`
- `shellext/`: the Windows 11 menu entry (a COM DLL implementing `IExplorerCommand`), built separately
- `src/winutil.rs`: UAC elevation, Explorer registration, file associations, setup (PATH, shortcut, autostart)
- `src/bin/rustimgmountw/`: the windowless companion (`main.rs`: hidden runner, progress and error
  dialogs; `convertui.rs`: the Convert dialog and the conversion queue; `tray.rs`: the tray app)

## License

MIT; see [LICENSE](LICENSE). RAR support is built from RARLAB's UnRAR source, which has its own terms: see [LICENSE-UNRAR.txt](LICENSE-UNRAR.txt).
