# Archfleet status for a Windows machine: sent over SSH with -EncodedCommand. $vpnFile is set before it.
$ErrorActionPreference = 'Stop'
$os = Get-CimInstance Win32_OperatingSystem
$cpu = Get-CimInstance Win32_Processor
$disk = Get-CimInstance Win32_LogicalDisk -Filter 'DeviceID="C:"'
$drives = @(Get-CimInstance Win32_LogicalDisk -Filter 'DriveType=3' | Where-Object Size | ForEach-Object {
    [pscustomobject]@{ name = $_.DeviceID; used = [double]$_.Size - [double]$_.FreeSpace; total = [double]$_.Size } })
$net = @(Get-CimInstance Win32_PerfFormattedData_Tcpip_NetworkInterface -ErrorAction SilentlyContinue)
[pscustomobject]@{
    hostname = $env:COMPUTERNAME
    kernel = 'Windows ' + $os.Version
    cpu = ($cpu | Measure-Object -Property LoadPercentage -Average).Average
    memory_used = ([double]$os.TotalVisibleMemorySize - [double]$os.FreePhysicalMemory) * 1024
    memory_total = [double]$os.TotalVisibleMemorySize * 1024
    disk_used = [double]$disk.Size - [double]$disk.FreeSpace
    disk_total = [double]$disk.Size
    uptime = ((Get-Date) - $os.LastBootUpTime).TotalSeconds
    battery = @()
    disks = $drives
    vpn = $(if ($vpnFile -and (Test-Path -LiteralPath $vpnFile)) { try { [IO.File]::ReadAllText($vpnFile).TrimStart([char]0xFEFF) | ConvertFrom-Json } catch { $null } } else { $null })
    net_rx = [double]($net | Measure-Object -Property BytesReceivedPersec -Sum).Sum
    net_tx = [double]($net | Measure-Object -Property BytesSentPersec -Sum).Sum
} | ConvertTo-Json -Compress -Depth 6
