# The dev build: smoke checks, the UI driven headless by the developer hooks, and a plugin run
# through Git for Windows' sh.
$ErrorActionPreference = 'Stop'
$config = New-Item -ItemType Directory -Force "$env:XDG_CONFIG_HOME\recouvrance"
$downloads = New-Item -ItemType Directory -Force "$env:HOME\Downloads"
"{`"port`": $env:PORT, `"version`": 1}" | Set-Content "$config\agents.json"
Copy-Item "$PSScriptRoot\plugins.yaml" $config
$failed = @()

$env:RECOUVRANCE_SMOKE = 'kwok-recouvrance'
$smoke = (& $env:DEV 2>&1) -join "`n"
$smoke
if ($smoke -notmatch '(?m)^kinds=' -or $smoke -notmatch '(?m)^rows=') { $failed += 'smoke' }
$env:RECOUVRANCE_DASH = '1'
$dash = (& $env:DEV 2>&1) -join "`n"
$dash
if ($dash -notmatch 'Deployments: [0-9]+ Running' -or $dash -notmatch 'Pods: [0-9]+ Running') { $failed += 'Workloads overview' }
Remove-Item Env:RECOUVRANCE_SMOKE, Env:RECOUVRANCE_DASH

# gpui has no off-screen renderer on Windows: a snapshot runs every hook, then can't write its
# PNG. Each run must get that far.
function Snapshot($name) {
  $out = (& $env:DEV --snapshot "$env:E2E\$name.png" 6 2>&1) -join "`n"
  Write-Host $out
  if (-not (Test-Path "$env:E2E\$name.png") -and $out -notmatch 'render_to_image not implemented') {
    $script:failed += "snapshot $name stopped early"
  }
  $out
}

# Each kind's list, its first object and every tab, then every page.
$env:RECOUVRANCE_CONTEXT = 'kwok-recouvrance'
$env:RECOUVRANCE_QA = '1'
$env:RECOUVRANCE_QA_KINDS = 'pods,deployments.apps,replicasets.apps,configmaps,nodes,namespaces,events'
$qa = Snapshot qa
if ($qa -notmatch 'QA done: 0 problem') { $failed += 'QA found problems' }
if ($qa -notmatch 'QA pods: rows=[1-9]') { $failed += 'QA listed no pods' }
Remove-Item Env:RECOUVRANCE_QA, Env:RECOUVRANCE_QA_KINDS

# The pods of default, the first one's details, exports, an input event and a plugin.
$env:RECOUVRANCE_KIND = 'pods'
$env:RECOUVRANCE_NS = 'default'
$env:RECOUVRANCE_SELECT = '0'
$env:RECOUVRANCE_TAB = 'yaml'
$env:RECOUVRANCE_ACT = 'csv,yaml'
$env:RECOUVRANCE_INPUT = 'wait:1;selected:'
$env:RECOUVRANCE_PLUGIN = '0'
$env:PLUGIN_OUT = "$env:E2E\plugin.txt" -replace '\\', '/'
$hooks = Snapshot hooks
if ($hooks -notmatch 'selected text:') { $failed += 'RECOUVRANCE_INPUT' }
Get-ChildItem $downloads | Select-Object -ExpandProperty Name
$csv = Get-ChildItem $downloads -Filter 'pods-*.csv' | Select-Object -First 1
if (-not $csv -or (Get-Content -Raw $csv.FullName) -notmatch 'web-') { $failed += 'CSV export' }
if (-not (Get-ChildItem $downloads -Filter 'pod-default-web-*.yaml')) { $failed += 'YAML download' }
$plugin = Get-Content -Raw "$env:E2E\plugin.txt" -ErrorAction SilentlyContinue
"plugin wrote: $plugin"
if ($plugin -notmatch 'pods web-\S+ default' -or $plugin -notmatch 'kubeconfig') { $failed += 'plugin through sh' }
if ($failed) { throw ($failed -join '; ') }
exit 0
