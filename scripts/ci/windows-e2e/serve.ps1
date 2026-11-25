# `recouvrance serve`: the MCP server without a window answers tools/list and tool calls on
# both contexts (the second runs the login command).
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\common.ps1"
$serve = Start-Process $env:APP -ArgumentList serve -PassThru `
  -RedirectStandardError "$env:E2E\serve.log" -RedirectStandardOutput "$env:E2E\serve.out"
try {
  Wait-Port $serve 60
  $tools = (Invoke-Tool tools/list) -split "`n"
  "$($tools.Count) tools"
  if ($tools -notcontains 'list_resources' -or $tools -notcontains 'find_problems') { throw "tools/list: $tools" }
  $contexts = (Invoke-Tool list_contexts) -split "`n"
  if ($contexts -notcontains 'kwok-recouvrance' -or $contexts -notcontains 'kwok-exec') { throw "list_contexts: $contexts" }
  foreach ($context in 'kwok-recouvrance', 'kwok-exec') {
    $pods = Invoke-Tool list_resources context=$context kind=pods namespace=default
    $pods
    if ($pods -notmatch 'web-') { throw "list_resources on $context listed no pod" }
  }
} finally {
  Stop-Process $serve -ErrorAction SilentlyContinue
  Get-Content "$env:E2E\serve.log"
}
$logins = Get-Logins $serve
$logins
if (-not $logins) { throw 'serve never ran the login command' }
if ($logins -match 'visible=True') { '::warning::serve ran the login command with a console window that shows' }
