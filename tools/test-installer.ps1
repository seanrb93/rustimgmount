<#
.SYNOPSIS
    Installs rustimgmount silently with each combination of the installer's tick boxes,
    checks what that leaves on the PC, and uninstalls again. Run by the GitHub workflows.

.DESCRIPTION
    This really installs and uninstalls for the current user, so run it on a machine where
    that is fine (a build runner), not on a PC with rustimgmount set up the way you like it.

.EXAMPLE
    .\tools\test-installer.ps1 -Installer dist\rustimgmount-0.1.6-setup.exe
#>
param([Parameter(Mandatory)][string]$Installer)

$ErrorActionPreference = "Stop"
$Installer = (Resolve-Path $Installer).Path
$dir = Join-Path $env:LOCALAPPDATA "Programs\rustimgmount"
$exe = Join-Path $dir "rustimgmount.exe"
$shortcut = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\rustimgmount.lnk"
$script:failed = 0

function Check([string]$What, [bool]$Ok, [string]$Detail = "") {
    if ($Ok) { Write-Host "  ok    $What" } else { Write-Host "  FAIL  $What $Detail" -ForegroundColor Red; $script:failed++ }
}
function RegValue([string]$Key, [string]$Name = "(default)") {
    $item = Get-ItemProperty -Path "HKCU:\$Key" -ErrorAction SilentlyContinue
    if ($null -eq $item) { return $null }
    $item.$Name
}
function OnPath { [bool](([Environment]::GetEnvironmentVariable("Path", "User") -split ";") | Where-Object { $_.TrimEnd("\") -ieq $dir }) }
# Either menu counts: the Windows 11 one needs Developer Mode, which a build runner may not have.
function MenuPresent {
    [bool](Get-AppxPackage -Name rustimgmount.ShellMenu) -or
        ((Test-Path "HKCU:\Software\Classes\SystemFileAssociations\.iso\shell\rustimgmount.mount") -and (Test-Path "HKCU:\Software\Classes\Directory\shell\rustimgmount.createiso"))
}
function MenuGone {
    -not (Get-AppxPackage -Name rustimgmount.ShellMenu) -and
        -not (Test-Path "HKCU:\Software\Classes\SystemFileAssociations\.iso\shell\rustimgmount.mount") -and
        -not (Test-Path "HKCU:\Software\Classes\SystemFileAssociations\.iso\shell\rustimgmount.convert") -and
        -not (Test-Path "HKCU:\Software\Classes\Directory\shell\rustimgmount.createiso")
}

function Install([string]$Tasks) {
    Write-Host "== Installing with tasks: '$Tasks'" -ForegroundColor Cyan
    $p = Start-Process $Installer -ArgumentList "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/TASKS=`"$Tasks`"" -Wait -PassThru
    Check "the installer finished without an error" ($p.ExitCode -eq 0) "(exit code $($p.ExitCode))"
}

# What every install leaves, whatever was ticked.
function CheckInstalled {
    foreach ($f in "rustimgmount.exe", "rustimgmountw.exe", "rustimgmount_shell.dll", "README.md", "LICENSE", "unins000.exe") {
        Check "installed $f" (Test-Path (Join-Path $dir $f))
    }
    $version = if (Test-Path $exe) { (& $exe --version) -join " " } else { "" }
    $wanted = [regex]::Match($Installer, "rustimgmount-(.+)-setup\.exe$").Groups[1].Value
    Check "the installed program runs and is version $wanted" ($version -eq "rustimgmount $wanted") "(got '$version')"
    Check "the install folder is on the user's PATH" (OnPath)
    Check "Start Menu shortcut" (Test-Path $shortcut)
    Check "right-click menu" (MenuPresent)
    $apps = Get-ChildItem "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall" -ErrorAction SilentlyContinue |
        ForEach-Object { Get-ItemProperty $_.PSPath } | Where-Object { $_.DisplayName -like "rustimgmount*" }
    Check "one entry in Settings > Apps" (@($apps).Count -eq 1) "(found $(@($apps).Count))"
    # The installed program works: make an ISO image of a folder and read it back.
    $work = Join-Path ([IO.Path]::GetTempPath()) "rustimgmount-installer-test"
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force (Join-Path $work "folder") | Out-Null
    Set-Content (Join-Path $work "folder\hello.txt") "hello"
    if (Test-Path $exe) { & $exe create (Join-Path $work "folder") | Out-Null }
    Check "the installed program makes an ISO image" (Test-Path (Join-Path $work "folder.iso"))
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}

# .chd stands for the file types: nothing else on a fresh Windows opens it.
function CheckOptions([bool]$Associate, [bool]$FormatIcons, [bool]$Autostart) {
    $type = RegValue "Software\Classes\.chd"
    if ($Associate) {
        Check "double-clicking a .chd mounts it" ($type -eq "rustimgmount.file.chd") "(.chd is '$type')"
        $command = RegValue "Software\Classes\rustimgmount.file.chd\shell\open\command"
        Check "the file type opens with the installed program" ("$command" -like "*$dir\rustimgmountw.exe*mount*") "(command: $command)"
        $icon = "$(RegValue 'Software\Classes\rustimgmount.file.chd\DefaultIcon')"
        if ($FormatIcons) {
            Check "each format has its own icon" ($icon -match ",-\d+$") "(icon: $icon)"
        } else {
            Check "every format has the rustimgmount icon" ($icon -match ",0$") "(icon: $icon)"
        }
    } else {
        Check "file types are left alone" ("$type" -notlike "rustimgmount.*") "(.chd is '$type')"
    }
    $run = RegValue "Software\Microsoft\Windows\CurrentVersion\Run" "rustimgmount"
    if ($Autostart) {
        Check "the tray app starts at sign-in" ("$run" -like "*$dir\rustimgmountw.exe*--tray*") "(entry: $run)"
    } else {
        Check "nothing starts at sign-in" ($null -eq $run) "(entry: $run)"
    }
}

function Uninstall {
    Write-Host "== Uninstalling" -ForegroundColor Cyan
    $p = Start-Process (Join-Path $dir "unins000.exe") -ArgumentList "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART" -Wait -PassThru
    Check "the uninstaller finished without an error" ($p.ExitCode -eq 0) "(exit code $($p.ExitCode))"
    # The uninstaller works from a copy of itself, which can outlive the process started here.
    for ($i = 0; $i -lt 60 -and (Test-Path $dir); $i++) { Start-Sleep -Milliseconds 500 }
    $left = if (Test-Path $dir) { (Get-ChildItem $dir -Recurse -Force | ForEach-Object Name) -join ", " } else { "" }
    Check "the install folder is gone" (-not (Test-Path $dir)) "(left: $left)"
    Check "the PATH entry is gone" (-not (OnPath))
    Check "the Start Menu shortcut is gone" (-not (Test-Path $shortcut))
    Check "the right-click menu is gone" (MenuGone)
    Check "the file types are gone" (-not (Test-Path "HKCU:\Software\Classes\rustimgmount.file.chd") -and "$(RegValue 'Software\Classes\.chd')" -notlike "rustimgmount.*")
    Check "the Default apps registration is gone" ($null -eq (RegValue "Software\RegisteredApplications" "rustimgmount"))
    Check "nothing starts at sign-in" ($null -eq (RegValue "Software\Microsoft\Windows\CurrentVersion\Run" "rustimgmount"))
    $apps = Get-ChildItem "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall" -ErrorAction SilentlyContinue |
        ForEach-Object { Get-ItemProperty $_.PSPath } | Where-Object { $_.DisplayName -like "rustimgmount*" }
    Check "the entry in Settings > Apps is gone" (@($apps).Count -eq 0)
}

if (Test-Path $dir) { throw "rustimgmount is already installed in $dir. This test installs and uninstalls it; run it on a machine without it." }

# The boxes as the installer shows them at first: associations with per-format icons.
Install "associate,associate\icons"
CheckInstalled
CheckOptions -Associate $true -FormatIcons $true -Autostart $false

# Installed over the first, as an update would be, with other choices: one icon for
# everything, and the tray app at sign-in.
Install "associate,autostart"
CheckInstalled
CheckOptions -Associate $true -FormatIcons $false -Autostart $true
Uninstall

# Nothing ticked: the program and its menu only.
Install ""
CheckInstalled
CheckOptions -Associate $false -FormatIcons $false -Autostart $false
Uninstall

if ($script:failed) { throw "$($script:failed) installer check(s) failed" }
Write-Host "All installer checks passed." -ForegroundColor Green
