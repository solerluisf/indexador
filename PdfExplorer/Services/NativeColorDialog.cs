using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Interop;
using System.Windows.Media;

namespace PdfExplorer.Services;

internal static class NativeColorDialog
{
    public static Color? Show(Window owner, Color initial)
    {
        var custColors = new int[16];
        var cc = new CHOOSECOLOR
        {
            lStructSize = Marshal.SizeOf<CHOOSECOLOR>(),
            hwndOwner = owner is not null ? new WindowInteropHelper(owner).Handle : IntPtr.Zero,
            rgbResult = initial.R | (initial.G << 8) | (initial.B << 16),
            lpCustColors = Marshal.AllocCoTaskMem(16 * 4),
            Flags = CC_ANYCOLOR | CC_FULLOPEN | CC_RGBINIT
        };
        Marshal.Copy(custColors, 0, cc.lpCustColors, 16);

        bool ok = ChooseColorW(ref cc);

        Marshal.Copy(cc.lpCustColors, custColors, 0, 16);
        Marshal.FreeCoTaskMem(cc.lpCustColors);

        if (!ok) return null;

        int c = cc.rgbResult;
        return Color.FromRgb((byte)c, (byte)(c >> 8), (byte)(c >> 16));
    }

    [DllImport("comdlg32.dll", SetLastError = true, CharSet = CharSet.Unicode, ExactSpelling = true)]
    private static extern bool ChooseColorW(ref CHOOSECOLOR cc);

    private const int CC_ANYCOLOR = 0x00000100;
    private const int CC_FULLOPEN = 0x00000002;
    private const int CC_RGBINIT = 0x00000001;

    [StructLayout(LayoutKind.Sequential)]
    private struct CHOOSECOLOR
    {
        public int lStructSize;
        public IntPtr hwndOwner;
        public IntPtr hInstance;
        public int rgbResult;
        public IntPtr lpCustColors;
        public int Flags;
        public IntPtr lCustData;
        public IntPtr lpfnHook;
        public IntPtr lpTemplateName;
    }
}
