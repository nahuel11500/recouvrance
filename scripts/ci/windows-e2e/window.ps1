# The app as a user starts it: its window opens and renders, its MCP server answers, the login
# command of its context runs with no console window that shows, its files are in
# %LOCALAPPDATA%\recouvrance, and closing the window quits it.
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\common.ps1"
Add-Type -AssemblyName System.Drawing
Add-Type -Namespace E2E -Name Window -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool GetWindowRect(System.IntPtr hWnd, out RECT rect);
public struct RECT { public int Left, Top, Right, Bottom; }
'@

# The probe sees windows: a console program started the usual way, with a console of its own,
# shows it. If it says it doesn't, this session shows no windows and the check proves nothing.
Start-Process powershell.exe -Wait -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "$env:E2E\login.ps1"
$probe = Get-Content "$env:E2E\login.log" | Where-Object { $_ -match "^parent=$PID " } | Select-Object -Last 1
"probe: $probe"
if ($probe -notmatch 'visible=True') { '::warning::this session shows no console window: the login command check proves nothing' }

# Started as Explorer starts it (ShellExecute): no standard handles, so the app borrows its hidden
# console (`console::hidden_for_gui`). Start-Process would hand it this step's pipes, and the app
# leaves a console alone when its output is redirected.
$start = [Diagnostics.ProcessStartInfo]::new($env:APP)
$start.UseShellExecute = $true
$app = [Diagnostics.Process]::Start($start)
$failed = @()
try {
  $deadline = (Get-Date).AddSeconds(90)
  while ($true) {
    $app.Refresh()
    if ($app.MainWindowHandle -ne 0) { break }
    if ($app.HasExited) { throw "the app exited with $($app.ExitCode) without a window" }
    if ((Get-Date) -gt $deadline) { throw 'the app opened no window' }
    Start-Sleep 1
  }
  "window: '$($app.MainWindowTitle)'"
  # The console the app borrowed, which the login command shares.
  Start-Process powershell.exe -Wait -WindowStyle Hidden -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', `
    '-File', "$PSScriptRoot\has-console.ps1", '-Id', $app.Id, '-Out', "$env:E2E\app-console.txt"
  "the app's console: $(Get-Content "$env:E2E\app-console.txt" -ErrorAction SilentlyContinue)"
  Get-CimInstance Win32_Process -Filter "ParentProcessId=$($app.Id)" | ForEach-Object { "child: $($_.Name) $($_.ProcessId)" }
  Wait-Port $app
  $pods = Invoke-Tool list_resources context=kwok-exec kind=pods namespace=default
  $pods
  if ($pods -notmatch 'web-') { $failed += 'list_resources through the app listed no pod' }
  # The window lists the pods of its context meanwhile.
  Start-Sleep 10
  $rect = New-Object E2E.Window+RECT
  [E2E.Window]::GetWindowRect($app.MainWindowHandle, [ref]$rect) | Out-Null
  $w = $rect.Right - $rect.Left
  $h = $rect.Bottom - $rect.Top
  "window at $($rect.Left),$($rect.Top), ${w}x$h"
  $shot = New-Object Drawing.Bitmap $w, $h
  [Drawing.Graphics]::FromImage($shot).CopyFromScreen($rect.Left, $rect.Top, 0, 0, $shot.Size)
  $shot.Save("$env:E2E\window.png")
  $colors = @{}
  for ($x = 0; $x -lt $w; $x += 7) { for ($y = 0; $y -lt $h; $y += 7) { $colors[$shot.GetPixel($x, $y).ToArgb()] = 1 } }
  "$($colors.Count) colors in the window"
  if ($colors.Count -lt 20) { $failed += "the window looks blank ($($colors.Count) colors)" }
} finally {
  if (-not $app.HasExited) {
    $app.CloseMainWindow() | Out-Null
    if (-not $app.WaitForExit(30000)) {
      Stop-Process $app
      $failed += 'the app did not quit when its window closed'
    }
  }
}
"exit code: $($app.ExitCode)"
$logins = Get-Logins $app
$logins
if (-not $logins) { $failed += 'the app never ran the login command' }
if ($logins -match 'visible=True') { $failed += 'the login command had a console window that shows' }

# Config and data in %LOCALAPPDATA%\recouvrance; what lands in %APPDATA% is listed.
$root = "$env:LOCALAPPDATA\recouvrance"
Get-ChildItem -Recurse -Force $root, $env:APPDATA | Select-Object -ExpandProperty FullName
foreach ($f in 'config\agents.json', 'data\audit.jsonl') {
  if (-not (Test-Path "$root\$f")) { $failed += "no $root\$f" }
}
if ($failed) { throw ($failed -join '; ') }
