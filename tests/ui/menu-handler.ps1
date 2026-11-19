param([string]$Path, [string]$Clsid = "7b3f5e2a-9c41-4d6b-a8e3-52c1d0f4b916")
# Asks rustimgmount's Windows 11 menu handler directly what it would show for the image
# $Path, the way Explorer does: its title, whether it is shown, and its flyout. Nothing is
# invoked. Explorer sometimes asks with no selection at all, so that is tried as well.
# This tests the handler alone. Whether Explorer offers the entry at all is a separate
# question: explorer-menu.ps1 answers that (run it twice; packaged entries load late).
# To see what Explorer asks the handler, create an empty file menu-debug.log in
# %LOCALAPPDATA%\rustimgmount; delete it again afterwards.
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public static class MenuHandler {
    [ComImport, Guid("a08ce4d0-fa25-44ab-b57c-c7b1c323e0b9"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IExplorerCommand {
        [PreserveSig] int GetTitle(IntPtr items, [MarshalAs(UnmanagedType.LPWStr)] out string name);
        [PreserveSig] int GetIcon(IntPtr items, [MarshalAs(UnmanagedType.LPWStr)] out string icon);
        [PreserveSig] int GetToolTip(IntPtr items, [MarshalAs(UnmanagedType.LPWStr)] out string tip);
        [PreserveSig] int GetCanonicalName(out Guid name);
        [PreserveSig] int GetState(IntPtr items, bool okToBeSlow, out uint state);
        [PreserveSig] int Invoke(IntPtr items, IntPtr bindCtx);
        [PreserveSig] int GetFlags(out uint flags);
        [PreserveSig] int EnumSubCommands(out IEnumExplorerCommand e);
    }
    [ComImport, Guid("a88826f8-186f-4987-aade-ea0cef8fbfe8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IEnumExplorerCommand {
        [PreserveSig] int Next(uint count, [MarshalAs(UnmanagedType.Interface)] out IExplorerCommand cmd, out uint fetched);
        [PreserveSig] int Skip(uint count);
        [PreserveSig] int Reset();
        [PreserveSig] int Clone(out IEnumExplorerCommand e);
    }
    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    static extern int SHCreateItemFromParsingName(string path, IntPtr bc, ref Guid riid, out IntPtr item);
    [DllImport("shell32.dll")]
    static extern int SHCreateShellItemArrayFromShellItem(IntPtr item, ref Guid riid, out IntPtr array);

    static void Describe(IExplorerCommand c, IntPtr items, string indent, List<string> lines) {
        string title; uint state, flags;
        int ht = c.GetTitle(items, out title);
        int hs = c.GetState(items, false, out state);
        c.GetFlags(out flags);
        string shown = hs != 0 ? "GetState failed 0x" + hs.ToString("x8") : (state & 2) != 0 ? "HIDDEN" : (state & 1) != 0 ? "greyed" : "shown";
        lines.Add(indent + (ht == 0 ? "'" + title + "'" : "GetTitle failed 0x" + ht.ToString("x8")) + "  " + shown + ((flags & 1) != 0 ? "  (flyout)" : (flags & 8) != 0 ? "  (separator)" : ""));
        if ((flags & 1) == 0) return;
        IEnumExplorerCommand e;
        if (c.EnumSubCommands(out e) != 0 || e == null) { lines.Add(indent + "    (no flyout entries)"); return; }
        IExplorerCommand sub; uint n;
        while (e.Next(1, out sub, out n) == 0 && n == 1) Describe(sub, items, indent + "    ", lines);
    }

    public static string[] Ask(string clsid, string path) {
        var lines = new List<string>();
        object o = Activator.CreateInstance(Type.GetTypeFromCLSID(new Guid(clsid)));
        var cmd = (IExplorerCommand)o;
        Guid iidItem = new Guid("43826d1e-e718-42ee-bc55-a1e261c37bfe"), iidArray = new Guid("b63ea76d-1f85-456f-a19c-48159efa858b");
        IntPtr item, array;
        Marshal.ThrowExceptionForHR(SHCreateItemFromParsingName(path, IntPtr.Zero, ref iidItem, out item));
        Marshal.ThrowExceptionForHR(SHCreateShellItemArrayFromShellItem(item, ref iidArray, out array));
        lines.Add("asked about " + path + ":");
        Describe(cmd, array, "    ", lines);
        lines.Add("asked with no selection:");
        Describe(cmd, IntPtr.Zero, "    ", lines);
        return lines.ToArray();
    }
}
'@
[MenuHandler]::Ask($Clsid, $Path)
