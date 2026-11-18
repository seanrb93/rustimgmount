param([string]$Exe, [string[]]$Files, [string]$Shot, [string]$Pick = "", [int[]]$Check = @(), [switch]$Cancel)
# Starts the Convert dialog for $Files, lists its controls, saves a picture of the window
# to $Shot, optionally selects a format / ticks boxes, then presses Convert (or Cancel).
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class W {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowW(string cls, string title);
    [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr parent, EnumProc cb, IntPtr l);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern int GetDlgCtrlID(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsWindowEnabled(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SendMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SendMessageW(IntPtr h, uint msg, IntPtr w, StringBuilder l);
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    public static List<IntPtr> Children(IntPtr p) { var l = new List<IntPtr>(); EnumChildWindows(p, (h, x) => { l.Add(h); return true; }, IntPtr.Zero); return l; }
    public static string Text(IntPtr h) { var sb = new StringBuilder(512); GetWindowTextW(h, sb, 512); return sb.ToString(); }
    public static string Class(IntPtr h) { var sb = new StringBuilder(128); GetClassNameW(h, sb, 128); return sb.ToString(); }
}
'@
[W]::SetProcessDPIAware() | Out-Null
$p = Start-Process -FilePath $Exe -ArgumentList (@("convert-ui") + ($Files | ForEach-Object { '"' + $_ + '"' })) -PassThru
$h = [IntPtr]::Zero
for ($i = 0; $i -lt 100 -and $h -eq [IntPtr]::Zero; $i++) { Start-Sleep -Milliseconds 100; $h = [W]::FindWindowW("rustimgmount_convert", [NullString]::Value) }
if ($h -eq [IntPtr]::Zero) { "dialog did not appear (process exited: $($p.HasExited))"; exit 1 }
Start-Sleep -Milliseconds 400
$wr = New-Object W+RECT; [W]::GetWindowRect($h, [ref]$wr) | Out-Null
"window: '$([W]::Text($h))' $($wr.R - $wr.L)x$($wr.B - $wr.T)"
$combo = [IntPtr]::Zero
foreach ($c in [W]::Children($h)) {
    $r = New-Object W+RECT; [W]::GetWindowRect($c, [ref]$r) | Out-Null
    $id = [W]::GetDlgCtrlID($c); $cls = [W]::Class($c)
    "  {0,-9} id={1,-4} x={2,-4} y={3,-4} w={4,-4} h={5,-3} enabled={6} '{7}'" -f $cls, $id, ($r.L - $wr.L), ($r.T - $wr.T), ($r.R - $r.L), ($r.B - $r.T), [W]::IsWindowEnabled($c), [W]::Text($c)
    if ($id -eq 100) { $combo = $c }
}
$n = [int][W]::SendMessageW($combo, 0x0146, [IntPtr]::Zero, [IntPtr]::Zero)   # CB_GETCOUNT
$items = @(); for ($i = 0; $i -lt $n; $i++) { $sb = New-Object System.Text.StringBuilder 256; [W]::SendMessageW($combo, 0x0148, [IntPtr]$i, $sb) | Out-Null; $items += $sb.ToString() }
"formats: " + ($items -join " | ")
"selected: " + $items[[int][W]::SendMessageW($combo, 0x0147, [IntPtr]::Zero, [IntPtr]::Zero)]
if ($Pick) { $idx = [array]::IndexOf($items, $Pick); if ($idx -lt 0) { "format '$Pick' is not offered" } else { [W]::SendMessageW($combo, 0x014E, [IntPtr]$idx, [IntPtr]::Zero) | Out-Null } }
foreach ($id in $Check) { foreach ($c in [W]::Children($h)) { if ([W]::GetDlgCtrlID($c) -eq $id) { [W]::SendMessageW($c, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null } } }   # BM_CLICK
Start-Sleep -Milliseconds 300
if ($Shot) {
    [W]::SetForegroundWindow($h) | Out-Null; Start-Sleep -Milliseconds 300
    $bmp = New-Object System.Drawing.Bitmap ($wr.R - $wr.L), ($wr.B - $wr.T)
    $g = [System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen($wr.L, $wr.T, 0, 0, $bmp.Size); $g.Dispose()
    $bmp.Save($Shot, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
}
[W]::PostMessageW($h, 0x0111, [IntPtr]$(if ($Cancel) { 2 } else { 1 }), [IntPtr]::Zero) | Out-Null   # WM_COMMAND: Cancel / Convert
$p.WaitForExit(120000) | Out-Null
"launcher exit code: $($p.ExitCode)"

