# Run elevated: mounts the disk-type fixtures (they become VHDs, which need admin),
# reads HELLO.TXT through Windows' own file system driver, then unmounts everything.
param([string]$Root = (Resolve-Path "$PSScriptRoot\..\.."), [string]$Only = "")
$exe = Join-Path $Root "target\release\rustimgmount.exe"
$d = Join-Path $Root "testdata\more"
$log = Join-Path $d "admin_log.txt"
"admin mount test $(Get-Date)" | Set-Content $log
$cases = @(
    @{ f = "pc98.fdi";        text = "Hello from a PC-98 floppy!" },
    @{ f = "pc98.hdi";        text = "Hello from a PC-98 hard disk!" },
    @{ f = "pc98.nhd";        text = "Hello from a PC-98 hard disk!" },
    @{ f = "atari.st";        text = "Hello from an Atari ST disk!" },
    @{ f = "atari.msa";       text = "Hello from an Atari ST disk!" },
    @{ f = "e01a\disk.E01";   text = "Hello from the raw disk!" },
    @{ f = "e01c\multi.E01";  text = "Hello from the raw disk!" },
    # Twins: the second of each pair has the same disk ID as the first (still mounted).
    @{ f = "twins\mbrA.img";  text = "Hello from the raw disk!" },
    @{ f = "twins\mbrB.img";  text = "Hello from the raw disk!" },
    @{ f = "twins\gptA.img";  text = "Hello from a GPT disk!" },
    @{ f = "twins\gptB.img";  text = "Hello from a GPT disk!" },
    # 2.5 TiB sparse disk: too big for VHD, mounted as VHDX.
    @{ f = "huge.qcow2";      text = "Hello from the raw disk!" }
)
$pass = 0
foreach ($c in $cases) {
    if ($Only -and $c.f -notlike "*$Only*") { continue }
    $out = & $exe mount (Join-Path $d $c.f) 2>&1 | Out-String
    $drive = [regex]::Match($out, '-> ([A-Z]:\\)').Groups[1].Value
    $res = "FAIL"
    $detail = ""
    if ($drive) {
        Start-Sleep -Milliseconds 500
        $vol = Get-Volume -DriveLetter $drive[0] -ErrorAction SilentlyContinue
        try {
            $line = Get-Content (Join-Path $drive "HELLO.TXT") -TotalCount 1 -ErrorAction Stop
            if ($line -eq $c.text) { $res = "PASS"; $pass++ } else { $detail = "content: $line" }
        } catch { $detail = "read failed: $($_.Exception.Message)" }
        $detail = "$drive fs=$($vol.FileSystemType) label=$($vol.FileSystemLabel) $detail"
    } else {
        $detail = "no drive letter. rustimgmount said: " + ($out -replace '\s+', ' ')
    }
    "$res $($c.f): $detail" | Add-Content $log
}
"--- list before unmount" | Add-Content $log
& $exe list 2>&1 | Out-String | Add-Content $log
& $exe unmount all 2>&1 | Out-String | Add-Content $log
"--- list after unmount" | Add-Content $log
& $exe list 2>&1 | Out-String | Add-Content $log
"passed $pass" | Add-Content $log
