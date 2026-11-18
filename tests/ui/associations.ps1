# For each extension: who Windows says opens it, and what is registered where.
Add-Type -TypeDefinition @'
using System; using System.Text; using System.Runtime.InteropServices;
public static class A {
    [DllImport("shlwapi.dll", CharSet = CharSet.Unicode)] static extern int AssocQueryStringW(uint flags, int str, string assoc, string extra, StringBuilder buf, ref uint len);
    public static string Q(string ext, int what) { var sb = new StringBuilder(1024); uint n = 1024; return AssocQueryStringW(0, what, ext, null, sb, ref n) == 0 ? sb.ToString() : ""; }
}
'@
foreach ($e in "iso", "chd", "cue", "img", "mds", "vhd", "vhdx", "vmdk", "vdi", "001", "raw", "bin") {
    $uc = (Get-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.$e\UserChoice" -ErrorAction SilentlyContinue).ProgId
    $hkcu = (Get-ItemProperty "HKCU:\Software\Classes\.$e" -ErrorAction SilentlyContinue).'(default)'
    $hklm = (Get-ItemProperty "HKLM:\Software\Classes\.$e" -ErrorAction SilentlyContinue).'(default)'
    $owp = ((Get-Item "HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.$e\OpenWithProgids" -ErrorAction SilentlyContinue).Property) -join ","
    ".{0,-5} userchoice=[{1}] hkcu=[{2}] hklm=[{3}]`n        progid=[{4}] name=[{5}] icon=[{6}]`n        cmd=[{7}]`n        openwithprogids=[{8}]" -f $e, $uc, $hkcu, $hklm, [A]::Q(".$e", 20), [A]::Q(".$e", 3), [A]::Q(".$e", 15), [A]::Q(".$e", 1), $owp
}
