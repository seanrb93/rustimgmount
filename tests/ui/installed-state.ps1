# Lists everything rustimgmount's setup creates and whether it is present. Run it after
# installing (everything PRESENT) or after uninstalling (everything gone).
$r = @()
foreach ($p in "$env:LOCALAPPDATA\Programs\rustimgmount", "$env:LOCALAPPDATA\rustimgmount", "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\rustimgmount.lnk") {
    $r += "{0,-8} {1}" -f $(if (Test-Path $p) { "PRESENT" } else { "gone" }), $p
    if (Test-Path $p -PathType Container) { Get-ChildItem $p -Recurse -Force | ForEach-Object { $r += "           " + $_.FullName } }
}
foreach ($k in "HKCU:\Software\Classes\rustimgmount.file.iso", "HKCU:\Software\Classes\rustimgmount.image", "HKCU:\Software\rustimgmount",
    "HKCU:\Software\Classes\SystemFileAssociations\.iso\shell\rustimgmount.mount", "HKCU:\Software\Classes\SystemFileAssociations\.iso\shell\rustimgmount.convert") {
    $r += "{0,-8} {1}" -f $(if (Test-Path $k) { "PRESENT" } else { "gone" }), $k
}
$run = (Get-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -ErrorAction SilentlyContinue).rustimgmount
$r += "{0,-8} start-at-sign-in entry" -f $(if ($run) { "PRESENT" } else { "gone" })
$reg = (Get-ItemProperty "HKCU:\Software\RegisteredApplications" -ErrorAction SilentlyContinue).rustimgmount
$r += "{0,-8} Default apps registration" -f $(if ($reg) { "PRESENT" } else { "gone" })
$path = [Environment]::GetEnvironmentVariable("Path", "User")
$r += "{0,-8} PATH entry" -f $(if ($path -match "rustimgmount") { "PRESENT" } else { "gone" })
$pkg = Get-AppxPackage -Name rustimgmount.ShellMenu
$r += "{0,-8} Windows 11 menu package" -f $(if ($pkg) { "PRESENT" } else { "gone" })
$apps = Get-ChildItem "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall" -ErrorAction SilentlyContinue | ForEach-Object { Get-ItemProperty $_.PSPath } | Where-Object { $_.DisplayName -like "*rustimgmount*" }
$r += "{0,-8} Settings > Apps entry {1}" -f $(if ($apps) { "PRESENT" } else { "gone" }), (($apps | ForEach-Object { $_.DisplayVersion + " -> " + $_.UninstallString }) -join "; ")
$procs = Get-Process rustimgmount, rustimgmountw -ErrorAction SilentlyContinue
$r += "{0,-8} running rustimgmount processes" -f $(if ($procs) { "PRESENT" } else { "gone" })
$exts = Get-ChildItem "HKCU:\Software\Classes" -ErrorAction SilentlyContinue | Where-Object { $_.PSChildName -like ".*" -and (Get-ItemProperty $_.PSPath -ErrorAction SilentlyContinue).'(default)' -like "rustimgmount.*" }
$r += "{0,-8} file types pointing at rustimgmount: {1}" -f $(if ($exts) { "PRESENT" } else { "gone" }), (($exts | ForEach-Object PSChildName) -join " ")
$r
