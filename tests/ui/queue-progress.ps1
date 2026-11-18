param([string]$Exe, [string]$Dir, [string]$Iso, [string]$Shot)
# Starts two slow conversions through the queue, pictures the progress window, presses Cancel.
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class P {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    public static IntPtr WindowOf(uint pid) { IntPtr f = IntPtr.Zero; EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == pid && IsWindowVisible(h)) { f = h; return false; } return true; }, IntPtr.Zero); return f; }
}
'@
[P]::SetProcessDPIAware() | Out-Null
$rng = New-Object System.Random 7
foreach ($n in "big1", "big2") {
    $fs = [IO.File]::Create("$Dir\$n.iso"); $head = [IO.File]::ReadAllBytes($Iso); $fs.Write($head, 0, $head.Length)
    $buf = New-Object byte[] (4MB); for ($i = 0; $i -lt 100; $i++) { $rng.NextBytes($buf); $fs.Write($buf, 0, $buf.Length) }; $fs.Close()
}
$env:RUSTIMGMOUNTW_DIALOG_DELAY_MS = "300"
$p1 = Start-Process $Exe -ArgumentList 'convert', ('"' + "$Dir\big1.iso" + '"'), '--to', 'chd' -PassThru
$p2 = Start-Process $Exe -ArgumentList 'convert', ('"' + "$Dir\big2.iso" + '"'), '--to', 'chd' -PassThru
$h = [IntPtr]::Zero
for ($i = 0; $i -lt 80 -and $h -eq [IntPtr]::Zero; $i++) { Start-Sleep -Milliseconds 100; foreach ($p in $p1, $p2) { $w = [P]::WindowOf($p.Id); if ($w -ne [IntPtr]::Zero) { $h = $w } } }
if ($h -eq [IntPtr]::Zero) { "no progress window"; exit 1 }
Start-Sleep -Milliseconds 1500
[P]::SetForegroundWindow($h) | Out-Null; Start-Sleep -Milliseconds 300
$r = New-Object P+RECT; [P]::GetWindowRect($h, [ref]$r) | Out-Null
$bmp = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
$g = [System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen($r.L, $r.T, 0, 0, $bmp.Size); $g.Dispose(); $bmp.Save($Shot, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
"while converting: " + ((Get-ChildItem $Dir -File | Where-Object Name -like "big*" | ForEach-Object Name) -join ", ")
[P]::PostMessageW($h, 0x0111, [IntPtr]2, [IntPtr]::Zero) | Out-Null   # Cancel
$p1.WaitForExit(30000) | Out-Null; $p2.WaitForExit(30000) | Out-Null
"exit codes: $($p1.ExitCode), $($p2.ExitCode)"
"after cancel: " + ((Get-ChildItem $Dir -File | Where-Object Name -like "big*" | ForEach-Object Name) -join ", ")
