# The app as a user starts it, from Explorer, in the runner's own profile: its window opens and
# renders, its MCP server answers, the login command of its context runs with no console window
# that shows, its files are in %LOCALAPPDATA%\recouvrance, and closing the window quits it.
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

# Explorer starts the app with its own environment: the defaults, so the kubeconfig goes to
# ~\.kube\config and the config the earlier steps made (port, agent) to the real %LOCALAPPDATA%.
$local = [Environment]::GetFolderPath('LocalApplicationData')
$roaming = [Environment]::GetFolderPath('ApplicationData')
$root = "$local\recouvrance"
if (Test-Path $root) { throw "$root exists before the app ever ran" }
New-Item -ItemType Directory -Force $root, "$env:USERPROFILE\.kube" | Out-Null
Copy-Item -Recurse "$env:RUNNER_TEMP\e2e\local\recouvrance\config" $root
Remove-Item "$root\config\agents.lock", "$root\config\main-agent" -ErrorAction SilentlyContinue
Copy-Item $env:KUBECONFIG "$env:USERPROFILE\.kube\config"
$before = @(Get-Process recouvrance -ErrorAction SilentlyContinue | ForEach-Object Id)
explorer.exe $env:APP
$failed = @()
$app = $null
try {
  $deadline = (Get-Date).AddSeconds(90)
  while ($true) {
    $app = Get-Process recouvrance -ErrorAction SilentlyContinue | Where-Object { $before -notcontains $_.Id } | Select-Object -First 1
    if ($app -and $app.MainWindowHandle -ne 0) { break }
    if ((Get-Date) -gt $deadline) { throw 'the app opened no window' }
    Start-Sleep 1
  }
  "window: '$($app.MainWindowTitle)', parent: $((Get-CimInstance Win32_Process -Filter "ProcessId=$($app.Id)").ParentProcessId)"
  # The console the app borrowed, which the login command shares.
  Start-Process powershell.exe -Wait -WindowStyle Hidden -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', `
    '-File', "$PSScriptRoot\has-console.ps1", '-Id', $app.Id, '-Out', "$env:E2E\app-console.txt"
  "the app's console: $(Get-Content "$env:E2E\app-console.txt" -ErrorAction SilentlyContinue)"
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
  if ($app -and -not $app.HasExited) {
    $app.CloseMainWindow() | Out-Null
    if (-not $app.WaitForExit(30000)) {
      Stop-Process $app
      $failed += 'the app did not quit when its window closed'
    }
  }
}
$logins = Get-Logins $app
$logins
if (-not $logins) { $failed += 'the app never ran the login command' }
# Known failure (#1047): said, not failed, until it is fixed. Then make it fail again.
if ($logins -match 'visible=True') { '::warning::#1047: the login command had a console window that shows' }

# Config and data in %LOCALAPPDATA%\recouvrance; what lands in %APPDATA% is listed.
Get-ChildItem -Recurse -Force $root, "$roaming\recouvrance" -ErrorAction SilentlyContinue | Select-Object -ExpandProperty FullName
foreach ($f in 'config\settings.json', 'data\audit.jsonl') {
  if (-not (Test-Path "$root\$f")) { $failed += "no $root\$f" }
}
if ($failed) { throw ($failed -join '; ') }
