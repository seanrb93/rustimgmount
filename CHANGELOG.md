# Changelog

What changed in each release of rustimgmount. The newest is first. Each section here
becomes that version's release notes on GitHub.

## 0.1.13

- **Fixed: an error box when shutting down or signing out.** While Windows was shutting down, the
  tray app could still start its regular check of what's mounted, and Windows showed
  "rustimgmount.exe - Application Error: The application was unable to start correctly
  (0xc0000142)". The tray app now stops checking as soon as Windows announces a shutdown or
  sign-out, and closes when it goes ahead.

## 0.1.12

- **Less like malware to antivirus programs.** Microsoft Defender flagged a copy of rustimgmount as
  `Trojan:Win32/Bearfoos.A!ml`, a guess by its machine-learning model, not a known threat. Several
  things the program did are also typical of malware, and are now done differently:
  - Uninstalling no longer starts a hidden command window that waits and then deletes the folder.
    Files in use are moved into the temporary folder instead, and the folder is removed directly.
  - Uninstalling asks the tray app to close instead of force-killing it.
  - The Start Menu shortcut is made with Windows' own shortcut object, not a PowerShell script.
  - Checking for updates uses Windows' own web client instead of starting `curl.exe` hidden.
  - Unmounting no longer asks PowerShell to bypass its script policy, which it didn't need.

## 0.1.11

> **⚠ Antivirus warning:** Microsoft Defender may flag this version as `Trojan:Win32/Bearfoos.A!ml`.
> This is a false positive. The `!ml` means the alert comes from Defender's machine-learning check,
> not from a known threat. Version 0.1.11 doesn't do anything harmful, but a few things it did when
> uninstalling and checking for updates look like what malware does. Version 0.1.12 does those
> things differently. Please install 0.1.12 or later instead. You can check that any download is the
> official one with its `-sha256.txt` file.

- **Fixed: "Create ISO image" was missing when right-clicking a folder** on PCs with the Windows 11
  menu installed (Developer Mode on). Windows takes no notice of folder entries from rustimgmount's
  menu package, so folders now have the same ordinary entries that drives already had: **Create ISO
  image** and **Create disc image: more options...**. On Windows 11 they are under "Show more options".
  Run the installer, or `rustimgmount setup`, again to get them.

## 0.1.10

- **Check a new image against its folder.** "Check the image against the folder when it's done" in the
  Create dialog, or `rustimgmount create <folder> --verify`, reads the finished image back and compares
  every file with the original. An image that differs is deleted.
- **Check for updates when the tray app starts.** A new tick box in the tray menu, off unless you turn
  it on. When a newer version exists you get one notification, and the menu says which version.
- Making a CSO or CHD from a folder now checks there is room for the temporary ISO and the result.
- A folder created for the result inside the folder being imaged (`--out-dir`) no longer appears in the
  image as an empty folder.
- Two names that are only the same once `;` is written as `_` no longer stop the image: the changed
  one gets a number. Names that differ only by unusual letter variants, which Windows allows side by
  side, are both kept.
- Folder listings are ordered exactly as the ISO 9660 standard asks.
- Cleaning up after a cancelled run can no longer be confused by another program that was given the
  same process number.

## 0.1.9

Fixes from a review of the code added in 0.1.6 to 0.1.8.

- Cancelling while a folder was being made into a CSO or CHD could leave the temporary ISO behind, as
  big as the whole folder. It is now removed like other half-written files.
- "Create ISO image" on a drive could make an image of whichever folder was current on that drive
  when run from a terminal, not of the drive.
- A file that was being written to, or that has more than one name (a hard link), could be recorded
  with an out-of-date size: cut short, or refused with "changed while it was being read".
- Folders that OneDrive and similar programs mark specially were left out as if they were links. Only
  real links (junctions and symbolic links) are left out now.
- Two runs writing the same file at once no longer write into each other's work: the second is
  refused. A file that appears under the result's name while it is being made is not replaced.
- A drive's image is saved to your real Desktop when OneDrive has moved it, and its name no longer
  breaks when the drive's name has characters a file name can't.
- Imaging a whole system drive skips Windows' page file and hibernation file.
- A disc name starting with `-` typed in the Create dialog no longer fails.
- `setup --no-menu` also removes a right-click menu left by an earlier setup.

## 0.1.8

- **A dialog for making disc images.** "Create disc image: more options..." on folders, and "Create ISO
  from folder..." in the tray menu, let you choose the format, the disc's name and where to save it.
- **Folder straight to CSO or CHD**, from that dialog or with `rustimgmount create <folder> --to chd`.
- **Whole drives.** "Create ISO image" is offered on drives as well as folders.
- **Long file names.** Names over 103 characters used to stop an image being made. Now the image gets
  the UDF file system too, which holds names of up to 254 characters.
- **Files that can't be read** are found before anything is written, and can be left out with a tick
  box or `--skip-unreadable`; they are listed at the end.
- **Check for updates...** in the tray menu.
- Fixed: after updating with `rustimgmount setup`, a tray app that was already running stayed the old
  version until restarted.
- Fixed: a program reading the output of `rustimgmount setup` could wait for ever when setup restarted
  the tray app.

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
