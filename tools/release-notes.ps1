<#
.SYNOPSIS
    Writes the release notes for one version: its section of CHANGELOG.md, followed by
    how to install. Used by the GitHub workflows; run it by hand to preview.

.EXAMPLE
    .\tools\release-notes.ps1 -Version 0.1.5 -Out notes.md
#>
param([Parameter(Mandatory)][string]$Version, [string]$Out = "")

$ErrorActionPreference = "Stop"
$Version = $Version.TrimStart("v")
$lines = Get-Content (Join-Path $PSScriptRoot "..\CHANGELOG.md") -Encoding UTF8

# The section runs from its "## <version>" heading to the next "## " heading.
$start = -1
for ($i = 0; $i -lt $lines.Count; $i++) {
    if ($lines[$i].Trim() -eq "## $Version") { $start = $i + 1; break }
}
if ($start -lt 0) { throw "CHANGELOG.md has no section for version $Version. Add a '## $Version' section before releasing." }
$end = $lines.Count
for ($i = $start; $i -lt $lines.Count; $i++) {
    if ($lines[$i].StartsWith("## ")) { $end = $i; break }
}
$section = ($lines[$start..($end - 1)] -join "`n").Trim()
if (-not $section) { throw "The section for version $Version in CHANGELOG.md is empty." }

$notes = @"
## What's new

$section

## Installing

Download ``rustimgmount-$Version-setup.exe`` below and run it. It installs for your user account only and needs no administrator rights. Windows may warn that the publisher is unknown, because the installer is not code-signed: choose "More info", then "Run anyway". Installing over an older version updates it.

Without the installer: unpack the zip anywhere and run ``rustimgmount.exe setup``.

To check that a download arrived intact, compare the output of ``Get-FileHash <file>`` in PowerShell with the line for that file in ``rustimgmount-$Version-sha256.txt``.

Needs Windows 10 or 11, 64-bit. The [README](https://github.com/seanrb93/rustimgmount#readme) explains what it does and how to use it.
"@

if ($Out) {
    [IO.File]::WriteAllText($Out, $notes, (New-Object Text.UTF8Encoding $false))
} else {
    $notes
}
