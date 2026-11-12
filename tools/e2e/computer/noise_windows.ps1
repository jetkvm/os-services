# perf's noise window on Windows (see tools/e2e/probe): a borderless window
# on the screen that OS Services captures, the one whose monitor shows the
# KVM's EDID (PnP ID JET). It redraws a grid of gray blocks, each one of 16
# grays, every interval, so the screen changes at each refresh and the
# capture, not the window, sets the frame rate. A WinForms timer would end on the 15.6 ms Windows timer
# tick, and waiting for each composition with DwmFlush reached 43 redraws a
# second on a VM with two screens. The window is C#, and its noise frames
# are small bitmaps, made once at the start and drawn scaled without
# smoothing, so a redraw costs little CPU. It appends "ready" to the window
# log once it shows.
# Per-monitor DPI awareness keeps the screen's bounds and the window in
# pixels also on a screen that Windows scales.
param([double]$Left, [double]$Top, [double]$Width, [double]$Height, [int]$Columns, [int]$Rows, [int]$IntervalMs, [string]$Log)
Add-Type -Name Native -Namespace E2E -MemberDefinition @'
[StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
public struct DisplayDevice {
    public int Size;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string Name;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string Description;
    public int Flags;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string Id;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string Key;
}
[DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr value);
[DllImport("user32.dll", CharSet = CharSet.Unicode)]
public static extern bool EnumDisplayDevices(string device, int index, ref DisplayDevice info, int flags);
'@
[void][E2E.Native]::SetProcessDpiAwarenessContext([IntPtr]-4)
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

function Get-MonitorId([string]$DeviceName) {
    $Monitor = New-Object E2E.Native+DisplayDevice
    $Monitor.Size = [System.Runtime.InteropServices.Marshal]::SizeOf($Monitor)
    if ([E2E.Native]::EnumDisplayDevices($DeviceName, 0, [ref]$Monitor, 0)) { $Monitor.Id } else { '' }
}
$Screens = @([System.Windows.Forms.Screen]::AllScreens | Where-Object { (Get-MonitorId $_.DeviceName) -like 'MONITOR\JET*' })
if ($Screens.Count -ne 1) { throw "$($Screens.Count) screens show the KVM's EDID, not one" }
$Screen = $Screens[0]

Add-Type -ReferencedAssemblies System.Windows.Forms, System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Imaging;
using System.IO;
using System.Runtime.InteropServices;
using System.Windows.Forms;

namespace E2E {
    public class Noise : Form {
        [DllImport("winmm.dll")] static extern uint timeBeginPeriod(uint period);
        readonly Bitmap[] frames = new Bitmap[60];
        readonly int interval;
        readonly string log;
        int next;

        public Noise(Rectangle bounds, int columns, int rows, int interval, string log) {
            this.interval = interval;
            this.log = log;
            var random = new Random(1);
            var pixels = new byte[columns * rows * 4];
            for (int n = 0; n < frames.Length; n++) {
                for (int i = 0; i < pixels.Length; i += 4) pixels[i] = pixels[i + 1] = pixels[i + 2] = (byte)(random.Next(16) * 17);
                frames[n] = new Bitmap(columns, rows, PixelFormat.Format32bppRgb);
                var data = frames[n].LockBits(new Rectangle(0, 0, columns, rows), ImageLockMode.WriteOnly, frames[n].PixelFormat);
                Marshal.Copy(pixels, 0, data.Scan0, pixels.Length);
                frames[n].UnlockBits(data);
            }
            FormBorderStyle = FormBorderStyle.None;
            ShowInTaskbar = false;
            TopMost = true;
            StartPosition = FormStartPosition.Manual;
            Bounds = bounds;
            DoubleBuffered = true;
        }

        protected override void OnPaint(PaintEventArgs e) {
            e.Graphics.InterpolationMode = InterpolationMode.NearestNeighbor;
            e.Graphics.PixelOffsetMode = PixelOffsetMode.Half;
            e.Graphics.DrawImage(frames[next % frames.Length], 0, 0, ClientSize.Width, ClientSize.Height);
        }

        protected override void OnShown(EventArgs e) {
            base.OnShown(e);
            File.AppendAllText(log, "ready" + Environment.NewLine);
            timeBeginPeriod(1);
            while (Visible) {
                next++;
                Refresh();
                Application.DoEvents();
                System.Threading.Thread.Sleep(interval);
            }
        }
    }
}
'@
$Bounds = New-Object System.Drawing.Rectangle(
    [int]($Screen.Bounds.X + $Left * $Screen.Bounds.Width), [int]($Screen.Bounds.Y + $Top * $Screen.Bounds.Height),
    [int]($Width * $Screen.Bounds.Width), [int]($Height * $Screen.Bounds.Height))
[System.Windows.Forms.Application]::Run((New-Object E2E.Noise $Bounds, $Columns, $Rows, $IntervalMs, $Log))
