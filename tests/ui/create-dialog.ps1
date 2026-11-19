param([string]$Exe, [string[]]$Folders, [string]$Pick = "", [string]$Name = "", [switch]$Skip, [switch]$Cancel)
# Starts the Create dialog for $Folders, lists its controls, optionally selects a format
# (-Pick, part of its text), types a disc name (-Name) and ticks "leave out files that
# can't be read" (-Skip), then presses Create (or Cancel).
Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class CW {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowW(string cls, string title);
    [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr parent, EnumProc cb, IntPtr l);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern bool SetWindowTextW(IntPtr h, string text);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern int GetDlgCtrlID(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsWindowEnabled(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SendMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SendMessageW(IntPtr h, uint msg, IntPtr w, StringBuilder l);
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
    public static List<IntPtr> Children(IntPtr p) { var l = new List<IntPtr>(); EnumChildWindows(p, (h, x) => { l.Add(h); return true; }, IntPtr.Zero); return l; }
    public static string Text(IntPtr h) { var sb = new StringBuilder(512); GetWindowTextW(h, sb, 512); return sb.ToString(); }
    public static string Class(IntPtr h) { var sb = new StringBuilder(128); GetClassNameW(h, sb, 128); return sb.ToString(); }
}
'@
$p = Start-Process -FilePath $Exe -ArgumentList (@("create-ui") + ($Folders | ForEach-Object { '"' + $_ + '"' })) -PassThru
$h = [IntPtr]::Zero
for ($i = 0; $i -lt 100 -and $h -eq [IntPtr]::Zero; $i++) { Start-Sleep -Milliseconds 100; $h = [CW]::FindWindowW("rustimgmount_create", [NullString]::Value) }
if ($h -eq [IntPtr]::Zero) { "dialog did not appear (process exited: $($p.HasExited))"; exit 1 }
Start-Sleep -Milliseconds 400
"window: '$([CW]::Text($h))'"
$byId = @{}
foreach ($c in [CW]::Children($h)) {
    $id = [CW]::GetDlgCtrlID($c)
    "  {0,-9} id={1,-4} enabled={2} '{3}'" -f [CW]::Class($c), $id, [CW]::IsWindowEnabled($c), [CW]::Text($c)
    $byId[$id] = $c
}
$combo = $byId[100]
$n = [int][CW]::SendMessageW($combo, 0x0146, [IntPtr]::Zero, [IntPtr]::Zero)   # CB_GETCOUNT
for ($i = 0; $i -lt $n; $i++) {
    $sb = New-Object System.Text.StringBuilder 256
    [CW]::SendMessageW($combo, 0x0148, [IntPtr]$i, $sb) | Out-Null               # CB_GETLBTEXT
    "  format $i`: $sb"
    if ($Pick -and $sb.ToString() -like "*$Pick*") { [CW]::SendMessageW($combo, 0x014E, [IntPtr]$i, [IntPtr]::Zero) | Out-Null; "  picked: $sb"; $Pick = "" }
}
if ($Name) { [CW]::SetWindowTextW($byId[110], $Name) | Out-Null }
if ($Skip) { [CW]::SendMessageW($byId[111], 0x00F1, [IntPtr]1, [IntPtr]::Zero) | Out-Null }   # BM_SETCHECK
[CW]::PostMessageW($h, 0x0111, [IntPtr]$(if ($Cancel) { 2 } else { 1 }), [IntPtr]::Zero) | Out-Null   # WM_COMMAND
$p.WaitForExit(120000) | Out-Null
"exit code: $($p.ExitCode)"
