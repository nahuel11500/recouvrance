# The app's command line, run as the GUI program it is: --version, --help (which names the
# config folder in %LOCALAPPDATA%), and agent-create, whose token the next steps use.
$ErrorActionPreference = 'Stop'
$config = New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\recouvrance\config"
New-Item -ItemType Directory -Force $env:APPDATA | Out-Null
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$out = & $env:APP --version
if ("$out" -ne "recouvrance $version") { throw "--version printed: $out" }
$help = (& $env:APP --help) -join "`n"
$help
if (-not $help.Contains("$config")) { throw "--help doesn't name $config" }
"{`"port`": $env:PORT, `"version`": 1}" | Set-Content "$config\agents.json"
$token = & $env:APP agent-create ci kwok-recouvrance kwok-exec
if ($LASTEXITCODE -or -not "$token".StartsWith('kv_')) { throw "agent-create: $token" }
"::add-mask::$token"
"TOKEN=$token" >> $env:GITHUB_ENV
