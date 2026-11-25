# The login command (exec credential plugin) of the `kwok-exec` context in the Windows end-to-end
# job. It prints the credential CI wrote next to it, and notes in login.log which process ran it
# and whether it had a console window that shows: the app must start it with none.
Add-Type -Namespace E2E -Name Win32 -MemberDefinition @'
[DllImport("kernel32.dll")] public static extern System.IntPtr GetConsoleWindow();
[DllImport("user32.dll")] public static extern bool IsWindowVisible(System.IntPtr hWnd);
'@
$window = [E2E.Win32]::GetConsoleWindow()
$visible = ($window -ne [IntPtr]::Zero) -and [E2E.Win32]::IsWindowVisible($window)
$parent = (Get-CimInstance Win32_Process -Filter "ProcessId=$PID").ParentProcessId
Add-Content -Path (Join-Path $PSScriptRoot 'login.log') -Value "parent=$parent window=$window visible=$visible"
[Console]::Out.Write((Get-Content -Raw (Join-Path $PSScriptRoot 'credential.json')))
