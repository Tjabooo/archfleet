#Requires -RunAsAdministrator
# Turns this Windows PC into an Archfleet machine your Linux machines can reach over SSH:
# installs OpenSSH Server, allows key-only logins for one user, and opens port 22 to your LAN only.
#
#   .\windows\setup-ssh-server.ps1 -PublicKeyFile C:\path\to\keys.pub [-LoginUser me] [-AllowFrom 192.168.1.0/24]
#
# -PublicKeyFile: plain Ed25519 public keys (one per line), e.g. each Linux machine's
#   ~/.ssh/id_ed25519_archfleet.pub from setup_ssh.py. Never pass a private key.
# -LoginUser: the Windows account Archfleet logs in as (default: you).
# -AllowFrom: the network allowed to connect (default: this PC's own /24 network).
# Safe to re-run to add keys. It refuses to replace an sshd_config it didn't write.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$PublicKeyFile,
    [ValidatePattern('^[A-Za-z0-9_.-]+$')][string]$LoginUser = $env:USERNAME,
    [string]$AllowFrom
)

$ErrorActionPreference = 'Stop'
function Protect-SshServerFile([string]$Path) {
    $acl = [Security.AccessControl.FileSecurity]::new()
    $acl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)')
    [IO.File]::SetAccessControl($Path, $acl)
}

if (-not $AllowFrom) {
    $ip = Get-NetIPAddress -AddressFamily IPv4 -PrefixOrigin Dhcp, Manual -ErrorAction SilentlyContinue |
        Where-Object { $_.IPAddress -notlike '169.254.*' -and $_.IPAddress -ne '127.0.0.1' } | Select-Object -First 1
    if (-not $ip) { throw 'Could not find this PC''s LAN address; pass -AllowFrom 192.168.x.0/24.' }
    $AllowFrom = (($ip.IPAddress -split '\.')[0..2] -join '.') + '.0/24'
}
if ($AllowFrom -notmatch '^\d+\.\d+\.\d+\.\d+/\d+$') { throw "-AllowFrom must look like 192.168.1.0/24, not '$AllowFrom'." }

$keys = @(Get-Content -LiteralPath $PublicKeyFile | Where-Object { $_.Trim() -and -not $_.StartsWith('#') })
if ($keys.Count -eq 0) { throw 'The public key file is empty.' }
foreach ($key in $keys) {
    if ($key -notmatch '^ssh-ed25519 [A-Za-z0-9+/]+={0,2}( .*)?$') {
        throw 'Expected plain Ed25519 public keys, one per line. Never supply a private key.'
    }
}

$hadService = $null -ne (Get-Service -Name sshd -ErrorAction SilentlyContinue)
$capability = Get-WindowsCapability -Online -Name 'OpenSSH.Server~~~~0.0.1.0'
if ($capability.State -ne 'Installed') {
    $install = Add-WindowsCapability -Online -Name 'OpenSSH.Server~~~~0.0.1.0'
    if ($install.RestartNeeded) { throw 'OpenSSH Server is installed but Windows needs a restart. Restart and run this again.' }
}

$sshDir = Join-Path $env:ProgramData 'ssh'
New-Item -ItemType Directory -Path $sshDir -Force | Out-Null
$configPath = Join-Path $sshDir 'sshd_config'
$marker = '# Managed by Archfleet windows/setup-ssh-server.ps1'
if (Test-Path -LiteralPath $configPath) {
    $old = [IO.File]::ReadAllText($configPath)
    if ($hadService -and -not $old.StartsWith($marker) -and $old -match '(?m)^\s*[^#\s]') {
        throw "$configPath already has settings this script didn't write. It was left alone; review it, or move it aside and run this again."
    }
    Copy-Item -LiteralPath $configPath -Destination ($configPath + '.backup-' + (Get-Date -Format 'yyyyMMdd-HHmmssfff'))
}

# One authorised-keys file for the allowed user, readable only by SYSTEM and Administrators.
$authorizedPath = Join-Path $sshDir 'administrators_authorized_keys'
$authorized = if (Test-Path -LiteralPath $authorizedPath) { [IO.File]::ReadAllText($authorizedPath) } else { '' }
foreach ($key in $keys) {
    $key = $key.Trim()
    $blob = ($key -split '\s+')[1]
    if ($authorized -notmatch ('(?m)(?:^|\s)' + [regex]::Escape($blob) + '(?:\s|$)')) {
        $authorized = $authorized.TrimEnd() + "`n" + "from=`"$AllowFrom,127.0.0.1`" " + $key + "`n"
    }
}
[IO.File]::WriteAllText($authorizedPath, $authorized.TrimStart(), [Text.UTF8Encoding]::new($false))
Protect-SshServerFile $authorizedPath

$config = @"
$marker
Port 22
AddressFamily inet
PubkeyAuthentication yes
PasswordAuthentication no
AuthenticationMethods publickey
PermitEmptyPasswords no
AllowUsers $LoginUser
AuthorizedKeysFile __PROGRAMDATA__/ssh/administrators_authorized_keys
AllowAgentForwarding no
Subsystem sftp sftp-server.exe
"@
[IO.File]::WriteAllText($configPath, $config + "`n", [Text.UTF8Encoding]::new($false))
& "$env:WINDIR\System32\OpenSSH\ssh-keygen.exe" -A
if ($LASTEXITCODE -ne 0) { throw 'Host key generation failed.' }
# Keys made by ssh-keygen -A are owned by whoever ran it; sshd (LocalSystem) wants SYSTEM/Administrators.
Get-ChildItem -LiteralPath $sshDir -Filter 'ssh_host_*_key' -File | ForEach-Object { Protect-SshServerFile $_.FullName }
& "$env:WINDIR\System32\OpenSSH\sshd.exe" -t
if ($LASTEXITCODE -ne 0) { throw 'The SSH configuration did not validate.' }

# The installer's own rule allows everyone; replace it with one for the LAN only.
if (Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -ErrorAction SilentlyContinue) {
    Disable-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' | Out-Null
}
if (Get-NetFirewallRule -Name 'Archfleet-SSH-LAN' -ErrorAction SilentlyContinue) {
    Set-NetFirewallRule -Name 'Archfleet-SSH-LAN' -Enabled True -Profile Private -RemoteAddress $AllowFrom | Out-Null
} else {
    New-NetFirewallRule -Name 'Archfleet-SSH-LAN' -DisplayName 'Archfleet SSH (LAN only)' -Enabled True -Profile Private `
        -Direction Inbound -Protocol TCP -LocalPort 22 -RemoteAddress $AllowFrom -Action Allow | Out-Null
}
Set-Service -Name sshd -StartupType Automatic
if ((Get-Service sshd).Status -eq 'Running') { Restart-Service sshd } else { Start-Service sshd }

Write-Output "SSH server ready: key-only logins as $LoginUser from $AllowFrom (Private network profile)."
Write-Output "Host key fingerprint (compare it the first time a Linux machine connects):"
& "$env:WINDIR\System32\OpenSSH\ssh-keygen.exe" -lf (Join-Path $sshDir 'ssh_host_ed25519_key.pub')
if ((Get-NetConnectionProfile | Select-Object -First 1).NetworkCategory -ne 'Private') {
    Write-Warning 'This network is set to Public, where the firewall rule does not apply. Set it to Private in Settings > Network.'
}
