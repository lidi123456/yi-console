# A real mouse click at a point of a process's main window, given in CSS px of its main webview (zoom 1),
# for windows-webtab-e2e.mjs: unlike CDP input, it goes wherever the OS sends it (native view or a hole in it)
param([int]$procId, [double]$x, [double]$y)
Add-Type @"
using System; using System.Runtime.InteropServices;
public class WtClick {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
  public struct POINT { public int X, Y; }
}
"@
[WtClick]::SetProcessDPIAware() | Out-Null
$h = (Get-Process -Id $procId).MainWindowHandle
[WtClick]::SetForegroundWindow($h) | Out-Null
$s = [WtClick]::GetDpiForWindow($h) / 96.0
$p = New-Object WtClick+POINT
$p.X = [int]($x * $s); $p.Y = [int]($y * $s)
[WtClick]::ClientToScreen($h, [ref]$p) | Out-Null
[WtClick]::SetCursorPos($p.X, $p.Y) | Out-Null
Start-Sleep -Milliseconds 120
[WtClick]::mouse_event(2, 0, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 40
[WtClick]::mouse_event(4, 0, 0, 0, [UIntPtr]::Zero)
