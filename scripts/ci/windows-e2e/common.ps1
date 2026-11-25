# Shared by the steps that talk to a running Recouvrance.

# Waits until something listens on the MCP port, or `$process` exits.
function Wait-Port($process, $seconds = 90) {
  $deadline = (Get-Date).AddSeconds($seconds)
  while ($true) {
    try { (New-Object Net.Sockets.TcpClient('127.0.0.1', $env:PORT)).Close(); return }
    catch {
      if ($process.HasExited) { throw "it exited with $($process.ExitCode) before listening on $env:PORT" }
      if ((Get-Date) -gt $deadline) { throw "nothing listens on $env:PORT" }
      Start-Sleep 1
    }
  }
}

# A tool call through the MCP server, as an agent makes it: its text answer.
function Invoke-Tool([Parameter(ValueFromRemainingArguments)] $arguments) {
  $out = (python "$PSScriptRoot\mcp_call.py" $env:PORT $env:TOKEN @arguments) -join "`n"
  if ($LASTEXITCODE) { throw "$($arguments -join ' '): $out" }
  $out
}

# The lines login.ps1 wrote when `$process` ran it.
function Get-Logins($process) {
  Get-Content "$env:E2E\login.log" -ErrorAction SilentlyContinue | Where-Object { $_ -match "^parent=$($process.Id) " }
}
