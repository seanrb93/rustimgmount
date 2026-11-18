param([string]$Dir, [string]$Out)
# Saves the icon Windows shows for one (empty) file of several types, side by side, in
# $Out (a PNG), and prints the type name and the registered file type of each.
# $Dir is a scratch folder for the sample files.
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Runtime.InteropServices;
public static class I {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct SHFILEINFO { public IntPtr hIcon; public int iIcon; public uint dwAttributes;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string szDisplayName;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 80)] public string szTypeName; }
    [DllImport("shell32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SHGetFileInfoW(string path, uint attrs, ref SHFILEINFO info, uint size, uint flags);
    [DllImport("shell32.dll")] public static extern int SHGetImageList(int list, ref Guid riid, out IImageList ppv);
    [ComImport, Guid("46EB5926-582E-4017-9FDF-E8998DAA0950"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IImageList { int Add(IntPtr a, IntPtr b, ref int c); int ReplaceIcon(int i, IntPtr h, ref int r); int SetOverlayImage(int a, int b);
        int Replace(int i, IntPtr a, IntPtr b); int AddMasked(IntPtr a, int c, ref int r); int Draw(IntPtr p); int Remove(int i);
        [PreserveSig] int GetIcon(int i, int flags, out IntPtr icon); }
    // The 48 px ("extra large") icon and the type name Explorer uses for this file.
    public static Bitmap Icon48(string path, out string type) {
        var info = new SHFILEINFO();
        SHGetFileInfoW(path, 0, ref info, (uint)Marshal.SizeOf(info), 0x4000 | 0x400);   // SYSICONINDEX | TYPENAME
        type = info.szTypeName;
        Guid iid = new Guid("46EB5926-582E-4017-9FDF-E8998DAA0950"); IImageList list;
        SHGetImageList(2, ref iid, out list);   // SHIL_EXTRALARGE
        IntPtr h; list.GetIcon(info.iIcon, 1, out h);
        return Icon.FromHandle(h).ToBitmap();
    }
}
'@
$exts = "iso", "chd", "cue", "bin", "cso", "gdi", "vhd", "vhdx", "vmdk", "qcow2", "img", "e01", "d88", "hdi"
New-Item -ItemType Directory -Force $Dir | Out-Null
$sheet = New-Object System.Drawing.Bitmap (64 * $exts.Count), 64
$g = [System.Drawing.Graphics]::FromImage($sheet); $g.Clear([System.Drawing.Color]::FromArgb(245, 245, 247))
$i = 0
foreach ($e in $exts) {
    $f = Join-Path $Dir "sample.$e"; if (-not (Test-Path $f)) { [IO.File]::WriteAllBytes($f, [byte[]]@()) }
    $type = ""; $bmp = [I]::Icon48($f, [ref]$type)
    $g.DrawImage($bmp, 8 + 64 * $i, 8)
    $progid = (Get-ItemProperty "HKCU:\Software\Classes\.$e" -ErrorAction SilentlyContinue).'(default)'
    "{0,-6} {1,-34} {2}" -f ".$e", $type, $progid
    $i++
}
$g.Dispose(); $sheet.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
