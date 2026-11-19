# Checks of the parts with windows and menus

`cargo test` covers the formats and conversions. These PowerShell scripts check what it
can't: the Convert dialog, the queue's progress window, the tray menu, Explorer's
right-click menu, file icons and what setup leaves on the PC. They are run by hand,
because they open real windows on the desktop and read what Windows itself reports.
None of them is part of `cargo test` or the GitHub build.

They need rustimgmount to be set up (`rustimgmount setup` or the installer), except
where a script takes the program's path. Run them with Windows PowerShell:

```
powershell -NoProfile -ExecutionPolicy Bypass -File tests\ui\<script>.ps1 <arguments>
```

| Script | What it does |
|---|---|
| `explorer-menu.ps1 -Path <file>` | Prints the right-click menu Explorer builds for a file, submenus included. Nothing is clicked. Shows the rustimgmount flyout, and whether Mount or Unmount is offered. |
| `create-dialog.ps1 -Exe <rustimgmountw.exe> -Folders <folder>... [-Pick "<format label>"] [-Name "<disc name>"] [-Skip] [-Cancel]` | Opens the Create dialog, prints its controls and formats, optionally picks a format, types a disc name and ticks "leave out files that can't be read", then presses Create (or Cancel). |
| `convert-dialog.ps1 -Exe <rustimgmountw.exe> -Files <image>... [-Shot out.png] [-Pick "<format label>"] [-Check 103,105] [-Cancel]` | Opens the Convert dialog, prints its controls and the formats offered, optionally saves a picture, then presses Convert (or Cancel). Tick boxes: 103 verify, 104 recycle the original, 105 replace existing. |
| `queue-progress.ps1 -Exe <rustimgmountw.exe> -Dir <scratch folder> -Iso <any small ISO> -Shot out.png` | Makes two large images, converts both through the queue, saves a picture of the progress window, presses Cancel and checks that no half-written file is left. |
| `tray-menu.ps1` | Opens the tray app's menu, prints its entries and closes it without choosing anything. The tray app must be running. |
| `file-icons.ps1 -Dir <scratch folder> -Out icons.png` | Saves the icons Windows shows for sample files of several types, and prints each type's name. |
| `associations.ps1` | For a set of extensions: which program Windows will open them with, and what is registered where. |
| `installed-state.ps1` | Lists everything setup creates and whether it is there. After installing, all present; after uninstalling, all gone. |

Set `RUSTIMGMOUNT_DATA` to a scratch folder first if the conversions should stay out of
the real queue and records.
