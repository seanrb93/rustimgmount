param([string]$Path)
# Lists the Explorer right-click menu of a file, including submenus, by asking the shell
# for the file's context menu the way Explorer does. Nothing is invoked.
Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public static class CtxMenu {
    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    static extern int SHParseDisplayName(string name, IntPtr bc, out IntPtr pidl, uint sfgaoIn, out uint sfgaoOut);
    [DllImport("shell32.dll")]
    static extern int SHBindToParent(IntPtr pidl, ref Guid riid, out IShellFolder folder, out IntPtr child);
    [DllImport("user32.dll")] static extern IntPtr CreatePopupMenu();
    [DllImport("user32.dll")] static extern bool DestroyMenu(IntPtr h);
    [DllImport("user32.dll")] static extern int GetMenuItemCount(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetMenuStringW(IntPtr h, uint item, StringBuilder sb, int max, uint flags);
    [DllImport("user32.dll")] static extern IntPtr GetSubMenu(IntPtr h, int pos);

    [ComImport, Guid("000214E6-0000-0000-C000-000000000046"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IShellFolder {
        void ParseDisplayName(); void EnumObjects(); void BindToObject(); void BindToStorage(); void CompareIDs();
        void CreateViewObject(); void GetAttributesOf();
        [PreserveSig] int GetUIObjectOf(IntPtr hwnd, uint count, [MarshalAs(UnmanagedType.LPArray)] IntPtr[] pidls, ref Guid riid, IntPtr reserved, [MarshalAs(UnmanagedType.IUnknown)] out object obj);
    }
    [ComImport, Guid("000214F4-0000-0000-C000-000000000046"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IContextMenu2 {
        [PreserveSig] int QueryContextMenu(IntPtr hmenu, uint index, uint first, uint last, uint flags);
        [PreserveSig] int InvokeCommand(IntPtr info);
        [PreserveSig] int GetCommandString(UIntPtr id, uint type, IntPtr reserved, IntPtr name, uint cch);
        [PreserveSig] int HandleMenuMsg(uint msg, IntPtr w, IntPtr l);
    }

    static void Walk(IContextMenu2 cm, IntPtr menu, string indent, List<string> lines) {
        int n = GetMenuItemCount(menu);
        for (int i = 0; i < n; i++) {
            var sb = new StringBuilder(260);
            GetMenuStringW(menu, (uint)i, sb, 260, 0x400);
            IntPtr sub = GetSubMenu(menu, i);
            if (sb.Length == 0 && sub == IntPtr.Zero) continue;
            lines.Add(indent + sb.ToString().Replace("&", "") + (sub != IntPtr.Zero ? "  >" : ""));
            if (sub != IntPtr.Zero) {
                cm.HandleMenuMsg(0x0117, sub, (IntPtr)i); // WM_INITMENUPOPUP: fill the submenu
                Walk(cm, sub, indent + "    ", lines);
            }
        }
    }

    public static string[] List(string path) {
        IntPtr pidl; uint attrs;
        Marshal.ThrowExceptionForHR(SHParseDisplayName(path, IntPtr.Zero, out pidl, 0, out attrs));
        Guid iidFolder = new Guid("000214E6-0000-0000-C000-000000000046");
        IShellFolder folder; IntPtr child;
        Marshal.ThrowExceptionForHR(SHBindToParent(pidl, ref iidFolder, out folder, out child));
        Guid iidCm = new Guid("000214E4-0000-0000-C000-000000000046");
        object o;
        Marshal.ThrowExceptionForHR(folder.GetUIObjectOf(IntPtr.Zero, 1, new[] { child }, ref iidCm, IntPtr.Zero, out o));
        var cm = (IContextMenu2)o;
        IntPtr menu = CreatePopupMenu();
        cm.QueryContextMenu(menu, 0, 1, 0x7FFF, 0x100); // CMF_EXTENDEDVERBS
        var lines = new List<string>();
        Walk(cm, menu, "", lines);
        DestroyMenu(menu);
        Marshal.FreeCoTaskMem(pidl);
        return lines.ToArray();
    }
}
'@
[CtxMenu]::List($Path)
