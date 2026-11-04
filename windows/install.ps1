# Archfleet on Windows: lets your Linux machines read and set this PC's clipboard (including
# Windows clipboard history) over SSH, and adds the archfleet-clipboard command here.
#
#   powershell -ExecutionPolicy Bypass -File windows\install.ps1
#
# Run it as yourself (not elevated) from a normal PowerShell window. It copies the bridge scripts to
# %LOCALAPPDATA%\archfleet and registers the "Archfleet Clipboard Bridge" logon task, which keeps
# clipboard-bridge.ps1 running hidden in your desktop session. SSH sessions can't see the desktop
# clipboard, so clipboard-client.ps1 (what Archfleet calls over SSH) talks to the bridge through a
# named pipe only your own account can open. No network port is opened. Safe to re-run.
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $env:LOCALAPPDATA 'archfleet'
New-Item -ItemType Directory -Force -Path $dest | Out-Null

# A bridge from an older install holds the mutex: stop it so the new copy takes over.
Get-CimInstance Win32_Process -Filter "Name='powershell.exe'" |
    Where-Object { $_.CommandLine -match 'clipboard-bridge\.ps1' } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
foreach ($name in 'clipboard-bridge.ps1', 'clipboard-client.ps1') {
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination (Join-Path $dest $name) -Force
}

$taskName = 'Archfleet Clipboard Bridge'
$script = Join-Path $dest 'clipboard-bridge.ps1'
$user = [Security.Principal.WindowsIdentity]::GetCurrent().Name  # USERDOMAIN reads WORKGROUP over SSH
# conhost --headless runs PowerShell without opening (or flashing) a console window.
$action = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\conhost.exe" `
    -Argument "--headless powershell.exe -NoProfile -NonInteractive -Sta -ExecutionPolicy Bypass -File `"$script`"" `
    -WorkingDirectory $dest
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
# Interactive + Limited: runs in the signed-in desktop session, unelevated, no stored password.
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
    -MultipleInstances IgnoreNew -StartWhenAvailable
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal `
    -Settings $settings -Description 'Keeps Archfleet''s clipboard bridge running in the desktop session.' -Force | Out-Null
Start-ScheduledTask -TaskName $taskName
Write-Output "Clipboard bridge: task '$taskName' for $user runs $script"

# archfleet-clipboard here needs Python 3 (standard library only).
$python = (Get-Command py -ErrorAction SilentlyContinue).Source
$pyArgs = '-3 '
if (-not $python) { $python = (Get-Command python -ErrorAction SilentlyContinue).Source; $pyArgs = '' }
# Python install manager's own folder, which isn't always on PATH in SSH sessions
$managed = Join-Path $env:LOCALAPPDATA 'Python\bin\python.exe'
if ((-not $python -or $python -match 'WindowsApps') -and (Test-Path $managed)) { $python = $managed; $pyArgs = '' }
if ($python -and $python -notmatch 'WindowsApps') {
    $cmd = Join-Path $dest 'archfleet-clipboard.cmd'
    Set-Content -LiteralPath $cmd -Encoding ASCII -Value @"
@echo off
rem archfleet-clipboard get <alias> | set <alias> (stdin) | copy <from> <to>
set PYTHONUTF8=1
"$python" $pyArgs"$repo\clipboard_cli.py" %*
"@
    Write-Output "Clipboard command: $cmd (add $dest to PATH to type just archfleet-clipboard)"
} else {
    Write-Output 'Python 3 not found: skipped the archfleet-clipboard command (the bridge works without it).'
}
Write-Output ''
Write-Output 'Turn on clipboard history (Settings > System > Clipboard) so SUPER+V on Linux can show it.'
Write-Output 'Your Linux machines reach this PC over SSH: windows\setup-ssh-server.ps1 (admin) sets up OpenSSH Server.'
