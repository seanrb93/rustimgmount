; Installer for rustimgmount (Inno Setup 6). Built by `build.ps1 -Installer`.
;
; It installs for the current user only, with no administrator rights, into the folder
; that `rustimgmount setup` uses. The file copying is done here; everything else (PATH,
; double-click associations, right-click menu, Start Menu shortcut) is done by running
; `rustimgmount setup` afterwards, so the installer and a manual setup behave the same.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{6E0B5C0E-3B6C-4E3D-9B1E-7A4B2D6F51C3}
AppName=rustimgmount
AppVersion={#AppVersion}
AppPublisher=rustimgmount
AppPublisherURL=https://github.com/seanrb93/rustimgmount
AppSupportURL=https://github.com/seanrb93/rustimgmount/issues
; `rustimgmount setup` registers this exact folder, so it is not offered as a choice.
DefaultDirName={localappdata}\Programs\rustimgmount
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
LicenseFile=..\LICENSE
OutputDir=..\dist
OutputBaseFilename=rustimgmount-{#AppVersion}-setup
SetupIconFile=..\assets\rustimgmount.ico
UninstallDisplayIcon={app}\rustimgmount.exe
UninstallDisplayName=rustimgmount
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; The tray app and the menu DLL may be in use; they are closed in PrepareToInstall below.
CloseApplications=no

[Tasks]
Name: "associate"; Description: "Mount disk images when I double-click them (associate the image file types with rustimgmount)"
; Off unless ticked. The same setting is "Start with Windows" in the tray app's menu.
Name: "autostart"; Description: "Start rustimgmount's tray app when I sign in to Windows"; Flags: unchecked

[Files]
Source: "..\target\release\rustimgmount.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\rustimgmountw.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\rustimgmount_shell.dll"; DestDir: "{app}"; Flags: ignoreversion restartreplace uninsrestartdelete
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE-UNRAR.txt"; DestDir: "{app}"; Flags: ignoreversion

[Run]
Filename: "{app}\rustimgmount.exe"; Parameters: "setup"; Flags: runhidden; StatusMsg: "Setting up file associations and the right-click menu..."; Tasks: associate
Filename: "{app}\rustimgmount.exe"; Parameters: "setup --no-associate"; Flags: runhidden; StatusMsg: "Setting up the right-click menu..."; Tasks: not associate
Filename: "{app}\rustimgmount.exe"; Parameters: "autostart on"; Flags: runhidden; Tasks: autostart
Filename: "{app}\rustimgmountw.exe"; Parameters: "--tray"; Description: "Start the tray app now"; Flags: postinstall nowait skipifsilent
; Windows leaves some types (.vhd, .img, ...) with the app that had them until the user picks.
Filename: "{app}\rustimgmount.exe"; Parameters: "associate"; Description: "Open Settings to choose rustimgmount for file types that other apps also open"; Flags: postinstall nowait skipifsilent runhidden unchecked; Tasks: associate

[UninstallRun]
; Removes the associations, menus, PATH entry and shortcut. Mounted images, records and
; the cache in %LOCALAPPDATA%\rustimgmount are left alone.
Filename: "{app}\rustimgmount.exe"; Parameters: "setup --remove --keep-files"; Flags: runhidden; RunOnceId: "rustimgmount-unsetup"

[UninstallDelete]
; Written by `rustimgmount setup`, not by this installer.
Type: files; Name: "{app}\AppxManifest.xml"
Type: filesandordirs; Name: "{app}\assets"
Type: filesandordirs; Name: "{app}\microsoft.system.package.metadata"
Type: files; Name: "{app}\*.old"

[Code]
// A running tray app keeps rustimgmountw.exe open, which would block replacing it.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM rustimgmountw.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Result := '';
end;
