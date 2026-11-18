# Opens the tray app's menu, reads its entries, and closes it again without choosing anything.
Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class T {
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowW(string cls, string title);
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern int GetMenuItemCount(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetMenuStringW(IntPtr h, uint item, StringBuilder sb, int max, uint flags);
    [DllImport("user32.dll")] public static extern IntPtr GetSubMenu(IntPtr h, int pos);
    public static void Walk(IntPtr m, string indent, StringBuilder o) {
        int n = GetMenuItemCount(m);
        for (int i = 0; i < n; i++) {
            var sb = new StringBuilder(260); GetMenuStringW(m, (uint)i, sb, 260, 0x400);
            IntPtr sub = GetSubMenu(m, i);
            if (sb.Length == 0) continue;
            o.AppendLine(indent + sb + (sub != IntPtr.Zero ? "  >" : ""));
            if (sub != IntPtr.Zero) Walk(sub, indent + "    ", o);
        }
    }
}
'@
$tray = [T]::FindWindowW("rustimgmount_tray", [NullString]::Value)
if ($tray -eq [IntPtr]::Zero) { "tray window not found"; exit 1 }
[T]::PostMessageW($tray, 0x8001, [IntPtr]::Zero, [IntPtr]0x205) | Out-Null   # WM_TRAY, right button up
$menu = [IntPtr]::Zero
for ($i = 0; $i -lt 40 -and $menu -eq [IntPtr]::Zero; $i++) { Start-Sleep -Milliseconds 100; $menu = [T]::FindWindowW("#32768", [NullString]::Value) }
if ($menu -eq [IntPtr]::Zero) { "menu did not open"; exit 1 }
$hmenu = [T]::SendMessageW($menu, 0x01E1, [IntPtr]::Zero, [IntPtr]::Zero)   # MN_GETHMENU
$o = New-Object System.Text.StringBuilder; [T]::Walk($hmenu, "", $o); $o.ToString()
[T]::PostMessageW($tray, 0x001F, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null   # WM_CANCELMODE closes the menu
