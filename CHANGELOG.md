# Changelog

What changed in each release of rustimgmount. The newest is first. Each section here
becomes that version's release notes on GitHub.

## 0.1.7

- **"Create ISO image" behaves like converting.** Several folders are done one after another in one
  progress window, and a notification says when a long one, or a batch, has finished.
- Making an ISO checks first that the drive has room for it.
- Every build now installs and uninstalls itself with each combination of the installer's options, and
  checks what that leaves behind.

## 0.1.6

- **Make an ISO from a folder.** Right-click a folder and choose "Create ISO image", use "Create ISO
  from folder..." in the tray menu, or run `rustimgmount create <folder>`. The image keeps long and
  non-English file names and holds files of any size, and it opens on Windows, macOS and Linux.
- **Checksums.** Each release now has a `sha256.txt` file listing the SHA-256 checksum of every
  download, so you can check that yours arrived intact.
- Release notes no longer end with an automatic list of commits on some releases and not on others.

## 0.1.5

- **Only formats that fit are offered.** The right-click menu now looks inside the image, as the Convert
  dialog already did. A `.img` that is a disc is offered disc formats, and one that is a disk gets disk
  formats. A hard-disk CHD is offered disk formats, where before it was wrongly offered disc formats.
- CloneCD (CCD/IMG/SUB) is only offered for images with raw sectors, which that format needs.
- A playlist or multi-disc file is judged by its first disc.
- "Convert to CHD" no longer appears on a file that is already a CHD. Recompressing is still in the
  Convert dialog.

## 0.1.4

- **One CHD option.** CHD comes in a CD kind and a DVD kind. rustimgmount now picks for you: the DVD
  kind for PSP discs, images with a DVD file system and anything too big for a CD, and the CD kind for
  the rest. `--to chd-dvd` still forces the DVD kind.
- **Per-format icons are optional.** A new tick box in the installer, and `rustimgmount setup
  --plain-icons`, give every image type the one rustimgmount icon.
- **The Convert dialog remembers** the format, folder and options you used last time. It does not
  remember "move the original to the Recycle Bin".
- The README has a new opening for people who just want to install and use it.

## 0.1.3

- **Installer option for file associations.** "Mount disk images when I double-click them" can be
  unticked to leave your file types alone. The last page can open Windows' Default apps settings.
- File types that another program also opens are left as they were until you choose rustimgmount for
  them. Before, they showed an internal name in Explorer's Type column.
- The right-click menu offers **Unmount** for images that Windows itself mounted.
- The file icons are now in one program only, which makes the download smaller.
- More tests: damaged VHD, VHDX and CloneCD files are checked for crashes and hangs.

## 0.1.2

- **Each image format has its own icon and description in Explorer:** a disc for optical and emulator
  formats, a hard drive for virtual-machine disks, a plain file for raw and forensic images, and a floppy
  disk for floppy and retro-computer formats, with the format's name on each.
- **Easier to uninstall.** There is an "Uninstall rustimgmount..." entry in the tray menu, and copies
  installed with `rustimgmount setup` now appear in Settings > Apps too.

## 0.1.1

- The installer can start the tray app each time you sign in to Windows.

## 0.1.0

The first release.

- **Mount** about 45 kinds of disk image as drives: everyday images (ISO, VHD, IMG), emulator formats
  (CHD, CUE/BIN, GDI, CSO, CCD, NRG, MDS/MDF...), virtual-machine disks (VMDK, VDI, QCOW2) and images
  inside archives. Discs with CD audio also get a drive with the music as WAV files.
- **Convert** between disc formats (ISO, CSO, ZSO, CHD, BIN/CUE, CCD, GDI) and between disk formats
  (VHD, VHDX, IMG, VMDK, VDI, QCOW2), one image or many at once, with an optional check against the
  original.
- **Explorer integration:** double-click to mount, a right-click menu, a Convert dialog and a tray app.
- An installer that needs no administrator rights.
