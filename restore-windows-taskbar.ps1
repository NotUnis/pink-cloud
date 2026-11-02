# Brings the normal Windows taskbar back if Pink Cloud ever closes without cleaning up
# (for example after a crash). Right-click this file > Run with PowerShell.

Get-Process pink-cloud -ErrorAction SilentlyContinue | Stop-Process -Force

Add-Type -Namespace PinkCloud -Name Taskbar -MemberDefinition @'
[StructLayout(LayoutKind.Sequential)]
public struct APPBARDATA { public int cbSize; public System.IntPtr hWnd; public uint uCallbackMessage; public uint uEdge; public int left, top, right, bottom; public System.IntPtr lParam; }
[DllImport("shell32.dll")] public static extern System.UIntPtr SHAppBarMessage(uint msg, ref APPBARDATA data);
[DllImport("user32.dll")] public static extern System.IntPtr FindWindow(string cls, string name);
[DllImport("user32.dll")] public static extern System.IntPtr FindWindowEx(System.IntPtr parent, System.IntPtr after, string cls, string name);
[DllImport("user32.dll")] public static extern bool ShowWindow(System.IntPtr h, int cmd);
[DllImport("user32.dll")] public static extern uint RegisterWindowMessage(string name);
[DllImport("user32.dll")] public static extern bool SendNotifyMessage(System.IntPtr h, uint msg, System.IntPtr w, System.IntPtr l);
'@

$stateFile = Join-Path $env:LOCALAPPDATA 'PinkCloud\windows-taskbar-state.txt'
$original = 0
if (Test-Path $stateFile) { $original = [int](Get-Content $stateFile) }

$tray = [PinkCloud.Taskbar]::FindWindow('Shell_TrayWnd', $null)
$abd = New-Object PinkCloud.Taskbar+APPBARDATA
$abd.cbSize = [Runtime.InteropServices.Marshal]::SizeOf($abd)
$abd.hWnd = $tray
$abd.lParam = [IntPtr]$original
[void][PinkCloud.Taskbar]::SHAppBarMessage(10, [ref]$abd)   # restore auto-hide setting

[void][PinkCloud.Taskbar]::ShowWindow($tray, 8)
$s = [IntPtr]::Zero
while (($s = [PinkCloud.Taskbar]::FindWindowEx([IntPtr]::Zero, $s, 'Shell_SecondaryTrayWnd', $null)) -ne [IntPtr]::Zero) {
    [void][PinkCloud.Taskbar]::ShowWindow($s, 8)
}

# ask background apps to put their tray icons back on the Windows taskbar
$taskbarCreated = [PinkCloud.Taskbar]::RegisterWindowMessage('TaskbarCreated')
[void][PinkCloud.Taskbar]::SendNotifyMessage([IntPtr]0xffff, $taskbarCreated, [IntPtr]::Zero, [IntPtr]::Zero)

if (Test-Path $stateFile) { Remove-Item $stateFile }
Write-Host "The Windows taskbar is back."
