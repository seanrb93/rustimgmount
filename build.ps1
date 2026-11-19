<#
.SYNOPSIS
    Builds everything rustimgmount consists of, in one step.

.DESCRIPTION
    Builds rustimgmount.exe and rustimgmountw.exe, then the Windows 11 menu DLL (a separate
    crate in shellext\), and puts the DLL next to the programs in target\release so that
    `rustimgmount setup` installs all three.

.PARAMETER Test
    Also run the test suite (needs Python 3 for the format tests).

.PARAMETER Package
    Also make dist\rustimgmount-<version>-windows-x64.zip.

.PARAMETER Installer
    Also make dist\rustimgmount-<version>-setup.exe. Needs Inno Setup 6 (ISCC.exe).

    With -Package or -Installer, dist\rustimgmount-<version>-sha256.txt is written too: the
    SHA-256 checksum of each download, for checking that a copy arrived intact.

.EXAMPLE
    .\build.ps1
    .\build.ps1 -Test -Package -Installer
#>
param([switch]$Test, [switch]$Package, [switch]$Installer)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

function Run([string]$What, [scriptblock]$Command) {
    Write-Host "== $What" -ForegroundColor Cyan
    & $Command
    if ($LASTEXITCODE -ne 0) { throw "$What failed (exit code $LASTEXITCODE)" }
}

Run "Building rustimgmount.exe and rustimgmountw.exe" { cargo build --release --bins }
Run "Building the Windows 11 menu DLL" { cargo build --release --manifest-path shellext/Cargo.toml --target-dir target/shellext }
Copy-Item target\shellext\release\rustimgmount_shell.dll target\release\ -Force

if ($Test) {
    Run "Running the tests" { cargo test --release }
}

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"(.+)"' | Select-Object -First 1).Matches.Groups[1].Value
$files = "target\release\rustimgmount.exe", "target\release\rustimgmountw.exe", "target\release\rustimgmount_shell.dll", "README.md", "LICENSE", "LICENSE-UNRAR.txt"

if ($Package) {
    New-Item -ItemType Directory -Force dist | Out-Null
    $zip = "dist\rustimgmount-$version-windows-x64.zip"
    if (Test-Path $zip) { Remove-Item $zip }
    Compress-Archive -Path $files -DestinationPath $zip
    Write-Host "Wrote $zip"
}

if ($Installer) {
    $iscc = (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source
    if (-not $iscc) {
        $iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe" | Where-Object { Test-Path $_ } | Select-Object -First 1
    }
    if (-not $iscc) { throw "Inno Setup 6 was not found. Install it from https://jrsoftware.org/isinfo.php, or leave out -Installer." }
    Run "Building the installer" { & $iscc /Qp "/DAppVersion=$version" installer\rustimgmount.iss }
    Write-Host "Wrote dist\rustimgmount-$version-setup.exe"
}

if ($Package -or $Installer) {
    # One line per download, in the layout `sha256sum -c` reads.
    $sums = "dist\rustimgmount-$version-sha256.txt"
    $lines = Get-ChildItem dist -File -Filter "rustimgmount-$version-*" | Where-Object { $_.Name -notlike "*-sha256.txt" } | Sort-Object Name |
        ForEach-Object { "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name }
    [IO.File]::WriteAllText((Join-Path $PSScriptRoot $sums), ($lines -join "`n") + "`n")
    Write-Host "Wrote $sums"
}

Write-Host "Done. The programs are in target\release; install them with: target\release\rustimgmount.exe setup" -ForegroundColor Green
