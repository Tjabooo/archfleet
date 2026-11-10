# Archfleet process list for a Windows machine: sent over SSH on stdin (too long for a command line).
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$cpus = [Environment]::ProcessorCount
$paths = '\Process(*)\ID Process', '\Process(*)\% Processor Time', '\Process(*)\Working Set - Private',
    '\Process(*)\IO Read Bytes/sec', '\Process(*)\IO Write Bytes/sec', '\Process(*)\Thread Count',
    '\Processor(_Total)\% Processor Time', '\PhysicalDisk(_Total)\Disk Read Bytes/sec', '\PhysicalDisk(_Total)\Disk Write Bytes/sec',
    '\Network Interface(*)\Bytes Received/sec', '\Network Interface(*)\Bytes Sent/sec',
    '\GPU Engine(*)\Utilization Percentage', '\GPU Process Memory(*)\Dedicated Usage', '\GPU Adapter Memory(*)\Dedicated Usage'
$watch = [Diagnostics.Stopwatch]::StartNew()
$samples = (Get-Counter -Counter $paths -SampleInterval 1 -MaxSamples 1 -ErrorAction SilentlyContinue).CounterSamples
$procs = @{}; $engines = @{}; $vram = @{}
$totals = @{ cpu = 0.0; disk_read = 0.0; disk_write = 0.0; net_rx = 0.0; net_tx = 0.0; vram_used = 0.0 }
foreach ($s in $samples) {
    $p = $s.Path; $v = [double]$s.CookedValue
    if ($p -match '\\process\((.+)\)\\([^\\]+)$') {
        if ($Matches[1] -in '_total', 'idle') { continue }
        if (-not $procs.ContainsKey($Matches[1])) { $procs[$Matches[1]] = @{} }
        $procs[$Matches[1]][$Matches[2]] = $v
    } elseif ($p -match '\\gpu engine\(pid_(\d+)_.*_engtype_(.+)\)\\') {
        $key = $Matches[1] + '|' + $Matches[2]; $engines[$key] = [double]$engines[$key] + $v
    } elseif ($p -match '\\gpu process memory\(pid_(\d+)_') { $vram[$Matches[1]] = [double]$vram[$Matches[1]] + $v }
    elseif ($p -like '*\gpu adapter memory(*') { $totals.vram_used += $v }
    elseif ($p -like '*\processor(_total)\*') { $totals.cpu = $v }
    elseif ($p -like '*\disk read bytes/sec') { $totals.disk_read = $v }
    elseif ($p -like '*\disk write bytes/sec') { $totals.disk_write = $v }
    elseif ($p -like '*\bytes received/sec') { $totals.net_rx += $v }
    elseif ($p -like '*\bytes sent/sec') { $totals.net_tx += $v }
}
$gpuByPid = @{}; $gpuByEngine = @{}
foreach ($key in $engines.Keys) {
    $pid_, $engine = $key -split '\|', 2
    if ($engines[$key] -gt [double]$gpuByPid[$pid_]) { $gpuByPid[$pid_] = $engines[$key] }
    $gpuByEngine[$engine] = [double]$gpuByEngine[$engine] + $engines[$key]
}
$info = @{}
foreach ($item in Get-Process) { $info[[string]$item.Id] = $item }
$rows = foreach ($name in $procs.Keys) {
    $c = $procs[$name]; $id = [string][int]$c['id process']; $item = $info[$id]
    [pscustomobject]@{
        pid = [int]$id; name = if ($item) { $item.ProcessName } else { $name -replace '#\d+$', '' }; user = $null; own = $false; start = 0
        cpu = [math]::Round($c['% processor time'] / $cpus, 2); memory = [double]$c['working set - private']
        gpu = if ($gpuByPid.ContainsKey($id)) { [math]::Round([math]::Min(100, $gpuByPid[$id]), 1) } else { 0 }
        vram = [double]$vram[$id]
        disk_read = [math]::Round($c['io read bytes/sec']); disk_write = [math]::Round($c['io write bytes/sec'])
        net_rx = $null; net_tx = $null; threads = [int]$c['thread count']; command = if ($item -and $item.Path) { [string]$item.Path } else { '' }
    }
}
$os = Get-CimInstance Win32_OperatingSystem
$adapter = Get-CimInstance Win32_VideoController | Sort-Object AdapterRAM -Descending | Select-Object -First 1
$gpuTotal = ($gpuByEngine.Values | Measure-Object -Maximum).Maximum
[pscustomobject]@{
    platform = 'windows'; interval = [math]::Round($watch.Elapsed.TotalSeconds, 3); cpus = $cpus; tcp_available = $false
    processes = @($rows)
    totals = [pscustomobject]@{
        cpu = [math]::Round($totals.cpu, 1)
        memory_used = ([double]$os.TotalVisibleMemorySize - [double]$os.FreePhysicalMemory) * 1024; memory_total = [double]$os.TotalVisibleMemorySize * 1024
        gpu = if ($null -ne $gpuTotal) { [math]::Round([math]::Min(100, $gpuTotal), 1) } else { $null }
        vram_used = $totals.vram_used; vram_total = if ($adapter.AdapterRAM) { [double]$adapter.AdapterRAM } else { $null }
        disk_read = [math]::Round($totals.disk_read); disk_write = [math]::Round($totals.disk_write)
        net_rx = [math]::Round($totals.net_rx); net_tx = [math]::Round($totals.net_tx)
    }
} | ConvertTo-Json -Compress -Depth 4
