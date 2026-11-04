"""Per-process CPU, GPU, memory, disk and network sampling. Standard library only.

Rates come from two samples taken SAMPLE seconds apart inside one helper call, so no
daemon is needed. Without root, disk and GPU counters of other users' processes are
unreadable and reported as None; per-process network covers TCP sockets only (ss).
"""
import os
import re
import signal
import subprocess
import time
from pathlib import Path

SAMPLE = 1.0
# backend.py imports this module on Windows too (for WINDOWS_PROCESSES), where these don't exist.
PAGE = os.sysconf('SC_PAGE_SIZE') if hasattr(os, 'sysconf') else 4096
TICKS = os.sysconf('SC_CLK_TCK') if hasattr(os, 'sysconf') else 100
SS_USERS = re.compile(r'\("((?:[^"\\]|\\.)*)",pid=(\d+),fd=\d+\)')
_users = {}


def user_name(uid):
    if uid not in _users:
        import pwd
        try:
            _users[uid] = pwd.getpwuid(uid).pw_name
        except KeyError:
            _users[uid] = str(uid)
    return _users[uid]


def read_stat(pid):
    """Return (name, ppid, cpu ticks, threads, start ticks, rss bytes) from /proc/pid/stat."""
    text = Path(f'/proc/{pid}/stat').read_text()
    close = text.rfind(')')
    fields = text[close + 2:].split()
    return (text[text.find('(') + 1:close], int(fields[1]), int(fields[11]) + int(fields[12]),
            int(fields[17]), int(fields[19]), int(fields[21]) * PAGE)


def read_io(pid):
    try:
        values = dict(line.split(': ') for line in Path(f'/proc/{pid}/io').read_text().splitlines())
        return int(values['read_bytes']), int(values['write_bytes'])
    except (OSError, KeyError, ValueError):
        return None


def read_gpu(pid):
    """Sum GPU engine time (ns) and VRAM per DRM client, counting each client once."""
    clients = {}
    try:
        fds = os.listdir(f'/proc/{pid}/fd')
    except OSError:
        return None
    for fd in fds:
        try:
            if not os.readlink(f'/proc/{pid}/fd/{fd}').startswith('/dev/dri/'):
                continue
            info = Path(f'/proc/{pid}/fdinfo/{fd}').read_text()
        except OSError:
            continue
        fields = {}
        for line in info.splitlines():
            key, _, value = line.partition(':')
            fields[key.strip()] = value.strip()
        if 'drm-client-id' not in fields:
            continue
        engines = {key[11:]: int(value.split()[0]) for key, value in fields.items() if key.startswith('drm-engine-') and not key.startswith('drm-engine-capacity')}
        vram = fields.get('drm-memory-vram') or fields.get('drm-resident-vram') or '0 KiB'
        number, _, unit = vram.partition(' ')
        clients[(fields.get('drm-pdev'), fields['drm-client-id'])] = (engines, int(number) * {'KiB': 1024, 'MiB': 1024**2, 'GiB': 1024**3}.get(unit, 1))
    engines = {}
    for client_engines, _ in clients.values():
        for name, value in client_engines.items():
            engines[name] = engines.get(name, 0) + value
    return engines, sum(vram for _, vram in clients.values())


def read_tcp():
    """Map each own TCP socket to (pid, bytes received, bytes sent)."""
    try:
        output = subprocess.run(['ss', '-tinpH'], capture_output=True, text=True, timeout=5).stdout
    except (OSError, subprocess.SubprocessError):
        return None
    sockets, current = {}, None
    for line in output.splitlines():
        if line and not line[0].isspace():
            parts = line.split()
            owner = SS_USERS.search(line)
            current = (parts[3], parts[4], int(owner.group(2))) if owner and len(parts) > 4 else None
        elif current:
            received = re.search(r'\bbytes_received:(\d+)', line)
            sent = re.search(r'\bbytes_sent:(\d+)', line)
            sockets[current[:2]] = (current[2], int(received.group(1)) if received else 0, int(sent.group(1)) if sent else 0)
            current = None
    return sockets


def read_cpu_total():
    values = list(map(int, Path('/proc/stat').read_text().splitlines()[0].split()[1:9]))
    return sum(values), values[3] + values[4]


def read_disks():
    """Sectors read/written by whole physical disks (no partitions, loop, zram or device-mapper)."""
    disks = {path.name for path in Path('/sys/block').iterdir() if (path / 'device').exists()}
    read = written = 0
    for line in Path('/proc/diskstats').read_text().splitlines():
        fields = line.split()
        if fields[2] in disks:
            read += int(fields[5]) * 512
            written += int(fields[9]) * 512
    return read, written


def read_network():
    received = sent = 0
    for line in Path('/proc/net/dev').read_text().splitlines()[2:]:
        name, _, values = line.partition(':')
        if name.strip() != 'lo':
            fields = values.split()
            received += int(fields[0])
            sent += int(fields[8])
    return received, sent


def gpu_card():
    for device in sorted(Path('/sys/class/drm').glob('card[0-9]*/device')):
        if (device / 'gpu_busy_percent').exists():
            return device
    return None


def read_int(path):
    try:
        return int(path.read_text())
    except (OSError, ValueError):
        return None


def snapshot():
    processes = {}
    for entry in os.scandir('/proc'):
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        try:
            name, ppid, ticks, threads, start, rss = read_stat(pid)
            uid = entry.stat().st_uid
        except (OSError, ValueError, IndexError):
            continue
        if pid == 2 or ppid == 2:
            continue  # kernel threads
        processes[pid] = {'name': name, 'ticks': ticks, 'threads': threads, 'start': start, 'rss': rss,
                          'uid': uid, 'io': read_io(pid), 'gpu': read_gpu(pid)}
    return processes


def display_name(pid, comm):
    try:
        argv = Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\0')
    except OSError:
        return comm, ''
    command = ' '.join(part.decode(errors='replace') for part in argv if part)[:300]
    base = os.path.basename(argv[0].decode(errors='replace')) if argv and argv[0] else ''
    # comm is cut at 15 characters; prefer the full executable name when it matches.
    return (base if len(comm) == 15 and base.startswith(comm) else comm), command


def rate(after, before, interval):
    return max(0.0, (after - before) / interval)


def collect(sample=SAMPLE):
    card = gpu_card()
    cpu_before, disk_before, net_before, tcp_before = read_cpu_total(), read_disks(), read_network(), read_tcp()
    gpu_busy = [read_int(card / 'gpu_busy_percent')] if card else []
    before = snapshot()
    started = time.monotonic()
    time.sleep(sample)
    after = snapshot()
    interval = time.monotonic() - started
    cpu_after, disk_after, net_after, tcp_after = read_cpu_total(), read_disks(), read_network(), read_tcp()
    if card:
        gpu_busy.append(read_int(card / 'gpu_busy_percent'))
    cpus = os.cpu_count() or 1
    own = os.getuid()

    tcp = {}
    if tcp_after is not None:
        for key, (pid, received, sent) in tcp_after.items():
            old = (tcp_before or {}).get(key)
            old_received, old_sent = (old[1], old[2]) if old and old[0] == pid else (0, 0)
            totals = tcp.setdefault(pid, [0, 0])
            totals[0] += max(0, received - old_received)
            totals[1] += max(0, sent - old_sent)

    rows = []
    for pid, new in after.items():
        old = before.get(pid)
        if not old or old['start'] != new['start']:
            old = dict(new, ticks=new['ticks'], io=new['io'], gpu=new['gpu'])
        mine = new['uid'] == own
        io = gpu = vram = None
        if new['io'] is not None and old['io'] is not None:
            io = (rate(new['io'][0], old['io'][0], interval), rate(new['io'][1], old['io'][1], interval))
        if new['gpu'] is not None:
            engines_old = (old['gpu'] or ({}, 0))[0]
            busy = [rate(value, engines_old.get(engine, value), interval) / 1e9 * 100 for engine, value in new['gpu'][0].items()]
            gpu, vram = min(100.0, sum(busy)), new['gpu'][1]
        network = tcp.get(pid, [0, 0]) if tcp_after is not None and mine else None
        name, command = display_name(pid, new['name'])
        rows.append({
            'pid': pid, 'name': name, 'user': user_name(new['uid']), 'own': mine, 'start': new['start'],
            'cpu': round(rate(new['ticks'], old['ticks'], interval) / TICKS * 100 / cpus, 2),
            'memory': new['rss'], 'gpu': None if gpu is None else round(gpu, 1), 'vram': vram,
            'disk_read': None if io is None else round(io[0]), 'disk_write': None if io is None else round(io[1]),
            'net_rx': None if network is None else round(network[0] / interval), 'net_tx': None if network is None else round(network[1] / interval),
            'threads': new['threads'], 'command': command,
        })

    total, idle = cpu_after[0] - cpu_before[0], cpu_after[1] - cpu_before[1]
    memory = {line.split(':')[0]: int(line.split()[1]) * 1024 for line in Path('/proc/meminfo').read_text().splitlines()}
    busy_values = [value for value in gpu_busy if value is not None]
    return {
        'platform': 'linux', 'interval': round(interval, 3), 'cpus': cpus, 'processes': rows,
        'tcp_available': tcp_after is not None,
        'totals': {
            'cpu': round(100 * (1 - idle / total), 1) if total else 0.0,
            'memory_used': memory['MemTotal'] - memory.get('MemAvailable', memory.get('MemFree', 0)), 'memory_total': memory['MemTotal'],
            'gpu': round(sum(busy_values) / len(busy_values), 1) if busy_values else None,
            'vram_used': read_int(card / 'mem_info_vram_used') if card else None,
            'vram_total': read_int(card / 'mem_info_vram_total') if card else None,
            'disk_read': round(rate(disk_after[0], disk_before[0], interval)), 'disk_write': round(rate(disk_after[1], disk_before[1], interval)),
            'net_rx': round(rate(net_after[0], net_before[0], interval)), 'net_tx': round(rate(net_after[1], net_before[1], interval)),
        },
    }


def terminate(value):
    """Send SIGTERM to one of this user's processes, after checking it is still the same process."""
    if not isinstance(value, dict):
        raise ValueError('Choose a process first.')
    try:
        pid, start = int(value.get('pid')), int(value.get('start'))
    except (TypeError, ValueError):
        raise ValueError('Invalid process.') from None
    if pid <= 1:
        raise ValueError('Invalid process.')
    try:
        name, _, _, _, current_start, _ = read_stat(pid)
        owner = os.stat(f'/proc/{pid}').st_uid
    except OSError:
        raise ValueError('That process has already exited.') from None
    if current_start != start:
        raise ValueError('That process has already exited (its PID now belongs to another process).')
    if owner != os.getuid():
        raise ValueError('Only your own processes can be ended without root.')
    os.kill(pid, signal.SIGTERM)
    return {'code': 0, 'text': f'Sent SIGTERM to {name} ({pid}).'}


# Windows: one Get-Counter sample. Process "IO" bytes include network and file I/O alike,
# and Windows has no per-process network counter, so those are reported as such.
WINDOWS_PROCESSES = r'''
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
'''
