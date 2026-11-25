# Whether process `$Id` has a console, and whether that console has a window that shows: this
# process leaves its own console and attaches to that process's. Writes the answer to `$Out`.
param([int] $Id, [string] $Out)
Add-Type -Namespace E2E -Name Console -MemberDefinition @'
[DllImport("kernel32.dll")] public static extern bool FreeConsole();
[DllImport("kernel32.dll", SetLastError = true)] public static extern bool AttachConsole(uint pid);
[DllImport("kernel32.dll")] public static extern System.IntPtr GetConsoleWindow();
[DllImport("user32.dll")] public static extern bool IsWindowVisible(System.IntPtr hWnd);
'@
[E2E.Console]::FreeConsole() | Out-Null
if ([E2E.Console]::AttachConsole($Id)) {
  $window = [E2E.Console]::GetConsoleWindow()
  $visible = ($window -ne [IntPtr]::Zero) -and [E2E.Console]::IsWindowVisible($window)
  "console window=$window visible=$visible" | Set-Content $Out
} else {
  "no console (error $([Runtime.InteropServices.Marshal]::GetLastWin32Error()))" | Set-Content $Out
}
