# Builds the app (a GUI program, as released) and the dev build (a console program, with the
# developer tools), and tells the next steps where they are.
$ErrorActionPreference = 'Stop'
$dir = New-Item -ItemType Directory -Force "$env:RUNNER_TEMP\e2e"
cargo build --locked --profile ci --features dev
if ($LASTEXITCODE) { exit 1 }
Copy-Item target\ci\recouvrance.exe "$dir\recouvrance-dev.exe"
cargo build --locked --profile ci
if ($LASTEXITCODE) { exit 1 }
Copy-Item target\ci\recouvrance.exe "$dir\recouvrance.exe"
"E2E=$dir" >> $env:GITHUB_ENV
"APP=$dir\recouvrance.exe" >> $env:GITHUB_ENV
"DEV=$dir\recouvrance-dev.exe" >> $env:GITHUB_ENV
"KUBECONFIG=$dir\kubeconfig.json" >> $env:GITHUB_ENV

# The PE header's subsystem: 2 is a GUI program (no console window), 3 a console one.
function Subsystem($exe) {
  $bytes = [IO.File]::ReadAllBytes($exe)
  [BitConverter]::ToUInt16($bytes, [BitConverter]::ToInt32($bytes, 0x3C) + 0x5C)
}
if ((Subsystem "$dir\recouvrance.exe") -ne 2) { throw 'the app is not a GUI program' }
if ((Subsystem "$dir\recouvrance-dev.exe") -ne 3) { throw 'the dev build is not a console program' }
