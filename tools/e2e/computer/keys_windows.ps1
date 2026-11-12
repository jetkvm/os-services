# The perf check's key window on Windows: a borderless window on the screen
# with the KVM's EDID (PnP ID JET), at the place that tools/e2e/probe
# gives, whose five blocks show the last letter typed (a is 1, z is 26, the
# first block is the highest bit, white is 1). It stays in front, takes the
# keyboard focus back when it loses it, and appends lines to the window log:
# "ready" once it shows, and one each time it lost the focus.
param([double]$Left, [double]$Top, [double]$Width, [double]$Height, [string]$Log)
Add-Type -Name Native -Namespace E2EKeys -MemberDefinition @'
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
[void][E2EKeys.Native]::SetProcessDpiAwarenessContext([IntPtr]-4)
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

function Get-MonitorId([string]$DeviceName) {
    $Monitor = New-Object E2EKeys.Native+DisplayDevice
    $Monitor.Size = [System.Runtime.InteropServices.Marshal]::SizeOf($Monitor)
    if ([E2EKeys.Native]::EnumDisplayDevices($DeviceName, 0, [ref]$Monitor, 0)) { $Monitor.Id } else { '' }
}
$Screens = @([System.Windows.Forms.Screen]::AllScreens | Where-Object { (Get-MonitorId $_.DeviceName) -like 'MONITOR\JET*' })
if ($Screens.Count -ne 1) { throw "$($Screens.Count) screens show the KVM's EDID, not one" }
$Screen = $Screens[0].Bounds

Add-Type -ReferencedAssemblies System.Windows.Forms, System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.IO;
using System.Windows.Forms;

namespace E2EKeys {
    public class KeyWindow : Form {
        int value;
        readonly string log;

        public KeyWindow(Rectangle bounds, string log) {
            this.log = log;
            FormBorderStyle = FormBorderStyle.None;
            ShowInTaskbar = false;
            TopMost = true;
            StartPosition = FormStartPosition.Manual;
            Bounds = bounds;
            BackColor = Color.FromArgb(128, 128, 128);
            DoubleBuffered = true;
        }

        protected override void OnPaint(PaintEventArgs e) {
            int side = (int)(0.12 * ClientSize.Width);
            for (int i = 0; i < 5; i++) {
                int cx = (int)((0.1 + 0.2 * i) * ClientSize.Width), cy = ClientSize.Height / 2;
                bool white = (value >> (4 - i) & 1) == 1;
                e.Graphics.FillRectangle(white ? Brushes.White : Brushes.Black, cx - side / 2, cy - side / 2, side, side);
            }
        }

        protected override void OnKeyPress(KeyPressEventArgs e) {
            char c = char.ToLowerInvariant(e.KeyChar);
            if (c >= 'a' && c <= 'z') {
                value = c - 'a' + 1;
                Invalidate();
                Update();
            }
        }

        protected override void OnShown(EventArgs e) {
            base.OnShown(e);
            Activate();
            File.AppendAllText(log, "ready" + Environment.NewLine);
        }

        protected override void OnDeactivate(EventArgs e) {
            base.OnDeactivate(e);
            File.AppendAllText(log, "focus lost at " + DateTime.Now.ToString("o") + Environment.NewLine);
            BeginInvoke((Action)(() => Activate()));
        }
    }
}
'@
$Bounds = New-Object System.Drawing.Rectangle(
    [int]($Screen.X + $Left * $Screen.Width), [int]($Screen.Y + $Top * $Screen.Height),
    [int]($Width * $Screen.Width), [int]($Height * $Screen.Height))
[System.Windows.Forms.Application]::Run((New-Object E2EKeys.KeyWindow $Bounds, $Log))
