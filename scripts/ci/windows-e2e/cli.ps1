# The app's command line, run as the GUI program it is: --version, --help (which names the
# config folder in %LOCALAPPDATA%), and agent-create, whose token the next steps use.
$ErrorActionPreference = 'Stop'
$config = New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\recouvrance\config"
New-Item -ItemType Directory -Force $env:APPDATA | Out-Null

# A command run the way docs/configuration.md says scripts wait for it: its stdout, and its exit
# code in $LASTEXITCODE.
function Run-App {
  $out = "$env:E2E\cli.out"
  $p = Start-Process $env:APP -ArgumentList $args -Wait -NoNewWindow -PassThru `
    -RedirectStandardOutput $out -RedirectStandardError "$env:E2E\cli.err"
  Get-Content "$env:E2E\cli.err"
  $global:LASTEXITCODE = $p.ExitCode
  Get-Content $out
}

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$out = Run-App --version
if ($LASTEXITCODE -or "$out" -ne "recouvrance $version") { throw "--version printed: $out" }
$help = (Run-App --help) -join "`n"
$help
if (-not $help.Contains("$config")) { throw "--help doesn't name $config" }

# Known failure (#1040): PowerShell doesn't wait for a GUI program whose output it captures or
# redirects; the app panics on the closed pipe. Said, not failed, until it is fixed.
$captured = & $env:APP --version 2>&1
"captured: $captured"
& $env:APP --version > "$env:E2E\redirected.out" 2>&1
"redirected: $(Get-Content -Raw "$env:E2E\redirected.out")"
if ("$captured" -ne "recouvrance $version") { '::warning::#1040: PowerShell lost the output of recouvrance --version' }

"{`"port`": $env:PORT, `"version`": 1}" | Set-Content "$config\agents.json"
$token = Run-App agent-create ci kwok-recouvrance kwok-exec
if ($LASTEXITCODE -or -not "$token".StartsWith('kv_')) { throw "agent-create: $token" }
"::add-mask::$token"
"TOKEN=$token" >> $env:GITHUB_ENV
Remove-Item "$env:E2E\cli.out"
