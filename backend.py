import asyncio
import base64
import datetime
import email.utils
import json
import os
import re
import socket
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from xml.etree import ElementTree
from clipboards import payload_bytes
from processes import WINDOWS_PROCESSES

ROOT = Path(__file__).resolve().parent
CONFIG = Path(os.environ.get('ARCHFLEET_HOSTS') or Path.home() / '.config' / 'archfleet' / 'hosts.json')
# Where the helper lives on the other Linux machines (install.sh puts it here) and where
# windows/install.ps1 puts the clipboard client on a Windows machine. hosts.json can override both
# per machine with "helper" and "clipboard_client".
REMOTE_HELPER = '~/.local/share/archfleet/helper.py'


def load_nodes(config=CONFIG):
    """The machines in hosts.json (see hosts.example.json). An empty list when there is none yet."""
    try:
        nodes = json.loads(Path(config).read_text())
    except FileNotFoundError:
        return []
    for node in nodes:
        node.setdefault('label', node.get('hostname') or node['alias'])
        node.setdefault('hostname', node['alias'])
        node.setdefault('os', 'linux')
        node.setdefault('user', '')
        node.setdefault('address', '')
    return nodes


def clipboard_client(node):
    return node.get('clipboard_client') or f"C:/Users/{node['user']}/AppData/Local/archfleet/clipboard-client.ps1"


def lan_label(nodes):
    """'192.168.1.0/24' when every machine's address shares one /24, else just 'LAN'."""
    prefixes = {'.'.join(n['address'].split('.')[:3]) for n in nodes if re.fullmatch(r'\d+\.\d+\.\d+\.\d+', n.get('address') or '')}
    return f'{prefixes.pop()}.0/24' if len(prefixes) == 1 else 'LAN'


NODES = load_nodes()
MARQUEE_ENV = Path.home() / '.config' / 'marquee-house.env'
MARQUEE_SERVER_ENV = Path(r'C:\marquee\.env')


def read_env(path):
    try:
        lines = Path(path).read_text(encoding='utf-8', errors='replace').splitlines()
    except OSError:
        return {}
    values = {}
    for line in lines:
        key, sep, value = line.partition('=')
        if sep and not line.lstrip().startswith('#'):
            values[key.strip()] = value.strip().strip('"\'')
    return values


def marquee_config(env_file=MARQUEE_ENV, server_env=MARQUEE_SERVER_ENV):
    """Marquee's address and House token: marquee-house's config on the Arch machines,
    Marquee's own .env when Archfleet runs on the server. None when neither is there."""
    env = read_env(env_file)
    if env.get('MARQUEE_URL') and env.get('HOUSE_TOKEN'):
        return env['MARQUEE_URL'].rstrip('/'), env['HOUSE_TOKEN']
    env = read_env(server_env)
    if env.get('HOUSE_TOKEN'):
        return f"http://127.0.0.1:{env.get('PORT') or 8080}", env['HOUSE_TOKEN']
    return None
NEWS_FEED = 'https://archlinux.org/feeds/news/'
SPEED_BYTES = 64 * 1024 * 1024
# Screen Time (github.com/Tjabooo/screentime). SSH logins don't have ~/.local/bin on PATH.
# A machine can override it with "screentime_command" in hosts.json.
SCREENTIME_COMMAND = 'PATH="$HOME/.local/bin:$PATH" screentime today --json'
# On Windows, Git Bash's ssh can come first on PATH; only Windows OpenSSH reads the homelab aliases.
_WIN_OPENSSH = Path(r'C:\Windows\System32\OpenSSH')
SSH, SCP = ((str(_WIN_OPENSSH / 'ssh.exe'), str(_WIN_OPENSSH / 'scp.exe'))
            if sys.platform == 'win32' and (_WIN_OPENSSH / 'ssh.exe').exists() else ('ssh', 'scp'))
# OpenSSH 10 warns on every login to the server's older sshd ("not using a post-quantum key
# exchange"); keep that out of results. Windows' bundled client doesn't know the option.
SSH_BASE = [SSH] if sys.platform == 'win32' else [SSH, '-o', 'WarnWeakCrypto=no']

WINDOWS_STATUS = r'''
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
'''


def ps_quote(text):
    return "'" + str(text).replace("'", "''") + "'"


def vpn_config(node):
    return node.get('vpn') if isinstance(node.get('vpn'), dict) else {}


def _stamp(value):
    """ISO 8601 (PowerShell's 'o' format) -> unix seconds, or None."""
    if not value:
        return None
    try:
        return datetime.datetime.fromisoformat(str(value)).timestamp()
    except (TypeError, ValueError):
        return None


def windows_vpn(raw):
    """A Windows machine's VPN status file -> the same shape the Linux helper reports. Understands the
    torrent split-tunnel watchdog's status.json (tunnel/server/exit/problems/lastLeakAt/lastDropAt)."""
    if not isinstance(raw, dict):
        return None
    exit_info = raw.get('exit') if isinstance(raw.get('exit'), dict) else {}
    problems = [str(p) for p in (raw.get('problems') or []) if p]
    at = _stamp(raw.get('at'))
    if at and time.time() - at > 300:
        problems.insert(0, 'the watchdog has not updated its status for over 5 minutes')
    if exit_info.get('ok') and exit_info.get('provider') is False:
        problems.append(f"exit {exit_info.get('ip')} is not the VPN provider's")
    return {'kind': 'tunnel', 'state': 'connected' if raw.get('tunnel') == 'up' else 'down',
            'relay': raw.get('server'), 'city': exit_info.get('city') if exit_info.get('ok') else None,
            'exit_ip': exit_info.get('ip') if exit_info.get('ok') else None, 'lockdown': None,
            'last_drop': _stamp(raw.get('lastDropAt')), 'last_leak': _stamp(raw.get('lastLeakAt')),
            'problems': problems, 'ok': bool(raw.get('ok')) and not problems}


def magic_packet(mac):
    digits = re.sub(r'[^0-9a-fA-F]', '', mac or '')
    if len(digits) != 12:
        raise ValueError('This machine has no valid MAC address in hosts.json.')
    return b'\xff' * 6 + bytes.fromhex(digits) * 16


def parse_news(xml_text, limit=8):
    root = ElementTree.fromstring(xml_text)
    items = []
    for item in root.iter('item'):
        published = email.utils.parsedate_to_datetime(item.findtext('pubDate', ''))
        items.append({'title': (item.findtext('title') or '').strip(), 'link': (item.findtext('link') or '').strip(), 'date': published.isoformat()})
    items.sort(key=lambda entry: entry['date'], reverse=True)
    return items[:limit]


async def run(argv, timeout=15, input_data=None):
    process = await asyncio.create_subprocess_exec(*argv, stdin=asyncio.subprocess.PIPE if input_data is not None else asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    try:
        stdout, stderr = await asyncio.wait_for(process.communicate(input_data), timeout)
    except (asyncio.TimeoutError, asyncio.CancelledError):
        if process.returncode is None:
            process.kill()
        await process.communicate()
        raise
    return process.returncode, stdout.decode(errors='replace').strip(), stderr.decode(errors='replace').strip()


class Backend:
    def is_local(self, node):
        return node['hostname'].lower() == socket.gethostname().lower()

    async def call(self, node, action='status', value=None):
        request = json.dumps({'action': action, 'value': value}, ensure_ascii=False).encode('utf-8')
        if node['os'] != 'linux':
            if action in ('clipboard-get', 'clipboard-set', 'clipboard-history', 'clipboard-entry'):
                argv = [*SSH_BASE, '-T', '-o', 'BatchMode=yes', node['alias'], f'powershell -NoProfile -NonInteractive -File "{clipboard_client(node)}"']
                code, output, error = await run(argv, timeout=18, input_data=request)
                try:
                    result = json.loads(output)
                except json.JSONDecodeError:
                    raise RuntimeError('Windows clipboard helper did not return a valid response.')
                if not result.get('ok'):
                    raise RuntimeError(result.get('error', 'Windows clipboard helper is unavailable.'))
                return result['data']
            if action == 'processes':
                # The script is too long for a Windows command line, so it travels on stdin.
                loader = base64.b64encode('& ([ScriptBlock]::Create([Console]::In.ReadToEnd()))'.encode('utf-16-le')).decode()
                code, output, error = await run([*SSH_BASE, '-T', '-o', 'BatchMode=yes', node['alias'], 'powershell -NoProfile -NonInteractive -EncodedCommand ' + loader], timeout=25, input_data=WINDOWS_PROCESSES.encode())
                if code or not output.startswith('{'):
                    raise RuntimeError('Could not read Windows processes.')
                return json.loads(output)
            if action in ('vpn-restart', 'vpn-test'):
                script = vpn_config(node).get(action.removeprefix('vpn-'))
                if not script:
                    raise ValueError(f"No \"{action.removeprefix('vpn-')}\" command for this machine's VPN in hosts.json.")
                # every stream (Write-Host included) as plain text, not CLIXML
                script = "$ProgressPreference = 'SilentlyContinue'\n& {\n" + script + "\n} *>&1 | Out-String -Width 160"
                encoded = base64.b64encode(script.encode('utf-16-le')).decode()
                code, output, error = await run([*SSH_BASE, '-T', '-o', 'BatchMode=yes', node['alias'], 'powershell -NoProfile -NonInteractive -EncodedCommand ' + encoded], timeout=180)
                return {'code': code, 'text': (output + ('\n' + error if error else '')).strip()}
            if action != 'status':
                raise ValueError('Select an Arch machine for these controls.')
            script = f"$vpnFile = {ps_quote(vpn_config(node).get('status_file', ''))}\n" + WINDOWS_STATUS
            encoded = base64.b64encode(script.encode('utf-16-le')).decode()
            code, output, error = await run([*SSH_BASE, '-T', '-o', 'BatchMode=yes', node['alias'], 'powershell -NoProfile -NonInteractive -EncodedCommand ' + encoded], timeout=15)
            if code:
                raise RuntimeError(error or output or 'Could not read Windows status.')
            data = json.loads(output)
            data['vpn'] = windows_vpn(data.get('vpn'))
            return data
        if self.is_local(node):
            argv = [sys.executable, str(ROOT / 'helper.py'), '--stdin']
        else:
            argv = [*SSH_BASE, '-T', '-o', 'BatchMode=yes', node['alias'], 'python3 ' + node.get('helper', REMOTE_HELPER) + ' --stdin']
        code, output, error = await run(argv, timeout=100 if action == 'updates' else 15, input_data=request)
        if not output:
            raise RuntimeError(error or 'No response from host.')
        try:
            result = json.loads(output)
        except json.JSONDecodeError:
            if action.startswith('clipboard-'):
                raise RuntimeError('Clipboard helper did not return a valid response.') from None
            raise RuntimeError(error or output[:250])
        if not result.get('ok'):
            raise RuntimeError(result.get('error', error))
        return result['data']

    async def clipboard(self, node):
        return await self.call(node, 'clipboard-get')

    async def set_clipboard(self, node, payload):
        normalized, _ = payload_bytes(payload)
        return await self.call(node, 'clipboard-set', normalized)

    async def status(self, node):
        started = time.monotonic()
        try:
            value = {'vpn_state': vpn_config(node).get('state_file')} if node['os'] == 'linux' else None
            data = await self.call(node, 'status', value)
            return dict(data, online=True, elapsed_ms=round((time.monotonic() - started) * 1000))
        except Exception as error:
            return {'online': False, 'error': str(error) or type(error).__name__}

    async def transfer(self, node, local, remote, download=False):
        if not local.strip() or not remote.strip():
            raise ValueError('Enter both the local and remote path.')
        if any(ord(char) < 32 for char in local + remote):
            raise ValueError('Paths cannot contain control characters.')
        local_path = str(Path(local).expanduser().absolute())
        if not download and not Path(local_path).is_file():
            raise ValueError('Choose an existing local file to upload.')
        if download and Path(local_path).is_dir():
            raise ValueError('Use a local filename for the download destination.')
        source, target = (node['alias'] + ':' + remote, local_path) if download else (local_path, node['alias'] + ':' + remote)
        code, output, error = await run([SCP, '-o', 'BatchMode=yes', '-p', '--', source, target], timeout=3600)
        if code:
            raise RuntimeError(error or output or 'Transfer failed.')
        return f'Copied {source} → {target}'

    def wake(self, node):
        """Send a Wake-on-LAN magic packet to the LAN broadcast addresses."""
        packet = magic_packet(node.get('mac'))
        subnet_broadcast = '.'.join(node['address'].split('.')[:3] + ['255'])
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
            for target in (subnet_broadcast, '255.255.255.255'):
                for port in (9, 7):
                    try:
                        sock.sendto(packet, (target, port))
                    except OSError:
                        pass
        return f"Wake-on-LAN packet sent to {node['hostname']} ({node['mac']})."

    async def latency(self, node, attempts=5):
        """Median time to open a TCP connection to the machine's SSH port, in milliseconds."""
        samples = []
        for _ in range(attempts):
            started = time.perf_counter()
            try:
                _, writer = await asyncio.wait_for(asyncio.open_connection(node['address'], 22), 3)
            except (OSError, asyncio.TimeoutError):
                continue
            samples.append((time.perf_counter() - started) * 1000)
            writer.close()
            try:
                await writer.wait_closed()
            except OSError:
                pass
        if not samples:
            raise RuntimeError(f"{node['hostname']} does not answer on port 22.")
        samples.sort()
        return samples[len(samples) // 2]

    async def speed_test(self, node, size=SPEED_BYTES, progress=None):
        """Push and pull `size` bytes through SSH. Measures the encrypted link as Archfleet uses it.

        `progress(phase, results)` is called before each phase with what has been measured so far,
        so a failure late in the test still leaves the earlier numbers on screen."""
        ssh = [*SSH_BASE, '-T', '-o', 'BatchMode=yes', node['alias']]
        if node['os'] == 'linux':
            sink, source, payload = 'cat > /dev/null', f'head -c {size} /dev/zero', b'\0' * size
            sink_login = source_login = 'exit 0'
        else:
            # PowerShell never sees EOF on stdin under Windows OpenSSH, so a native program drains
            # the upload: findstr reads 1 KiB lines that never match and prints nothing.
            sink, sink_login = 'findstr x', 'exit 0'
            payload = (b'0' * 1023 + b'\n') * (size // 1024)
            source = ('powershell -NoProfile -NonInteractive -Command "$o=[Console]::OpenStandardOutput(); $b=New-Object byte[] 1MB; '
                      f'for($i=0;$i -lt {size // (1024 * 1024)};$i++){{$o.Write($b,0,$b.Length)}}; $o.Flush()"')
            source_login = 'powershell -NoProfile -NonInteractive -Command exit'
        results = {'bytes': size}
        report = progress or (lambda phase, results: None)
        report('latency', results)
        results['latency_ms'] = await self.latency(node)

        async def login_time(command):
            # Time an SSH login running the same kind of command, so it can be taken out of the transfer times.
            started = time.perf_counter()
            code, _, error = await run(ssh + [command], timeout=20)
            if code:
                raise RuntimeError(error or 'SSH login failed.')
            return time.perf_counter() - started
        rate = lambda seconds, login: size / max(0.05, seconds - login)

        handshake = await login_time(sink_login)
        results['handshake_ms'] = handshake * 1000
        report('upload', results)
        started = time.perf_counter()
        code, _, error = await run(ssh + [sink], timeout=180, input_data=payload)
        if code not in (0, 1 if node['os'] != 'linux' else 0):  # findstr exits 1 when nothing matched
            raise RuntimeError(error or 'Upload test failed.')
        results['upload'] = rate(time.perf_counter() - started, handshake)

        source_handshake = handshake if source_login == sink_login else await login_time(source_login)
        report('download', results)
        process = await asyncio.create_subprocess_exec(*ssh, source, stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL)
        received, started = 0, time.perf_counter()
        try:
            while chunk := await asyncio.wait_for(process.stdout.read(1 << 20), 30):
                received += len(chunk)
        finally:
            if process.returncode is None:
                process.kill()
            await process.wait()
        if received < size:
            raise RuntimeError(f'Download test received {received:,} of {size:,} bytes.')
        results['download'] = rate(time.perf_counter() - started, source_handshake)
        return results

    async def marquee_load(self):
        """What streams and encodes cost the server right now: Marquee's House → Sharing report."""
        config = marquee_config()
        if not config:
            raise RuntimeError('Marquee isn’t set up on this machine (no ~/.config/marquee-house.env: run marquee-house setup).')
        url, token = config

        def fetch():
            request = urllib.request.Request(url + '/api/house/load', headers={'X-Marquee-Token': token, 'User-Agent': 'archfleet'})
            try:
                with urllib.request.urlopen(request, timeout=6) as response:
                    return json.loads(response.read())
            except urllib.error.HTTPError as error:
                if error.code == 403:
                    raise RuntimeError('Marquee refused the House token (only accepted straight from the home network).') from None
                raise RuntimeError(f'Marquee answered {error.code}.') from None
            except (urllib.error.URLError, TimeoutError, OSError) as error:
                raise RuntimeError(f'Marquee isn’t answering at {url}.') from error
        return await asyncio.to_thread(fetch)

    async def screentime(self, node):
        """Today's Screen Time on an Arch machine: `screentime today --json` over SSH."""
        if node['os'] != 'linux':
            raise ValueError('Screen Time runs on the Arch machines.')
        command = node.get('screentime_command') or SCREENTIME_COMMAND
        if self.is_local(node):
            argv = ['/bin/sh', '-c', command]
        else:
            argv = [*SSH_BASE, '-T', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=5', node['alias'], command]
        code, output, error = await run(argv, timeout=20)
        if code == 127 or 'not found' in (error or ''):
            raise RuntimeError(f"screentime isn’t installed on {node['hostname']} (github.com/Tjabooo/screentime).")
        try:
            data = json.loads(output)
        except json.JSONDecodeError:
            raise RuntimeError((error or output or 'screentime gave no answer.').strip().splitlines()[-1][:200]) from None
        if not isinstance(data, dict) or 'total' not in data:
            raise RuntimeError('screentime answered something unexpected.')
        return data

    async def run_command(self, node, command, timeout=60):
        """Run one shell command on a machine: its login shell on Arch, cmd.exe on Windows."""
        started = time.monotonic()
        if self.is_local(node):
            argv = [os.environ.get('SHELL') or '/bin/sh', '-c', command]
        else:
            argv = [*SSH_BASE, '-T', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=5', node['alias'], command]
        try:
            code, output, error = await run(argv, timeout=timeout)
        except asyncio.TimeoutError:
            return {'code': None, 'output': f'Stopped after {timeout} s.', 'seconds': time.monotonic() - started}
        text = '\n'.join(part for part in (output, error) if part).replace('\r\n', '\n')
        return {'code': code, 'output': text, 'seconds': time.monotonic() - started}

    async def packages(self, node):
        return await self.call(node, 'packages')

    async def screenshot(self, node):
        return await self.call(node, 'screenshot')

    async def arch_news(self):
        def fetch():
            request = urllib.request.Request(NEWS_FEED, headers={'User-Agent': 'archfleet'})
            with urllib.request.urlopen(request, timeout=15) as response:
                return response.read()
        return parse_news(await asyncio.to_thread(fetch))
