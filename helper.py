"""Small, per-invocation Arch adapter. Uses only the Python standard library."""
import base64
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path


def command(argv, *, env=None, timeout=15):
    proc = subprocess.run(argv, capture_output=True, text=True, errors='replace', env=env, timeout=timeout)
    return {'code': proc.returncode, 'text': (proc.stdout + proc.stderr).strip()}


def cpu_sample():
    values = list(map(int, Path('/proc/stat').read_text().splitlines()[0].split()[1:9]))
    return sum(values), values[3] + values[4]


def net_sample():
    """Bytes received and sent on physical interfaces (no loopback, bridges or VPN tunnels)."""
    rx = tx = 0
    for line in Path('/proc/net/dev').read_text().splitlines()[2:]:
        name, _, fields = line.partition(':')
        name = name.strip()
        if not (Path('/sys/class/net') / name / 'device').exists():
            continue
        values = fields.split()
        rx += int(values[0])
        tx += int(values[8])
    return rx, tx


def read_int(path):
    try:
        return int(Path(path).read_text().strip())
    except (OSError, ValueError):
        return None


def sensors():
    """CPU and GPU temperature in °C plus GPU busy %, from hwmon and amdgpu sysfs. Missing values are None."""
    result = {'cpu_temp': None, 'gpu_temp': None, 'gpu_busy': None}
    gpus = []
    for hwmon in Path('/sys/class/hwmon').glob('hwmon*'):
        try:
            name = (hwmon / 'name').read_text().strip()
        except OSError:
            continue
        if name in ('k10temp', 'coretemp', 'zenpower') and result['cpu_temp'] is None:
            value = read_int(hwmon / 'temp1_input')
            if value is not None:
                result['cpu_temp'] = round(value / 1000, 1)
        elif name == 'amdgpu':
            device = hwmon / 'device'
            gpus.append((read_int(device / 'mem_info_vram_total') or 0, read_int(hwmon / 'temp1_input'), read_int(device / 'gpu_busy_percent')))
    if gpus:
        # The GPU with the most VRAM is the discrete card on a desktop, the only one on the laptop.
        _, temp, busy = max(gpus, key=lambda gpu: gpu[0])
        result['gpu_temp'] = None if temp is None else round(temp / 1000, 1)
        result['gpu_busy'] = busy
    return result


def last_upgrade(log=Path('/var/log/pacman.log')):
    """Timestamp of the newest completed `pacman -Syu`, read from the end of pacman.log."""
    try:
        with log.open('rb') as handle:
            handle.seek(0, 2)
            end = handle.tell()
            chunk = 1 << 20
            while True:
                start = max(0, end - chunk)
                handle.seek(start)
                lines = handle.read(end - start).decode(errors='replace').splitlines()
                for line in reversed(lines):
                    if '[PACMAN] starting full system upgrade' in line and line.startswith('['):
                        return line[1:line.index(']')]
                if start == 0 or chunk >= 64 << 20:
                    return None
                chunk *= 4
    except OSError:
        return None


def vpn_status(state_file=None):
    """The Mullvad app's state (None without one). state_file: optional JSON with "last_drop"
    (unix seconds), written by whatever watches the VPN on that machine; set in hosts.json."""
    if not shutil.which('mullvad'):
        return None
    try:
        out = subprocess.run(['mullvad', 'status', '--json'], capture_output=True, text=True, timeout=5).stdout.strip()
        raw = json.loads(out.splitlines()[-1])
    except (OSError, subprocess.TimeoutExpired, json.JSONDecodeError, IndexError):
        return {'kind': 'mullvad', 'state': 'unknown', 'problems': ['the Mullvad daemon is not answering'], 'ok': False}
    details = raw.get('details') if isinstance(raw.get('details'), dict) else {}
    loc = details.get('location') or {}
    state = raw.get('state', 'unknown')
    lock = subprocess.run(['mullvad', 'lockdown-mode', 'get'], capture_output=True, text=True, timeout=5).stdout
    vpn = {'kind': 'mullvad', 'state': state, 'relay': loc.get('hostname'), 'city': loc.get('city'),
           'exit_ip': loc.get('ipv4') or loc.get('ipv6'), 'lockdown': bool(re.search(r':\s*on\b', lock, re.I)),
           'last_drop': None, 'problems': []}
    if state == 'error':
        cause = details.get('cause')
        vpn['problems'].append('blocked: ' + (cause if isinstance(cause, str) else json.dumps(cause)))
    if state == 'connected' and loc.get('mullvad_exit_ip') is False:
        vpn['problems'].append(f"exit {vpn['exit_ip']} is not a Mullvad address")
    if state_file:
        try:
            drop = json.loads(Path(state_file).expanduser().read_text()).get('last_drop')
            vpn['last_drop'] = drop if isinstance(drop, (int, float)) else None
        except (OSError, json.JSONDecodeError, AttributeError):
            pass
    vpn['ok'] = not vpn['problems']
    return vpn


VPN_LOCATION = re.compile(r'^[a-z0-9-]{2,24}$')


def vpn_action(request):
    """connect / disconnect / reconnect / relay ("se sto", "se-sto-wg-001", ...) through the Mullvad CLI."""
    if not shutil.which('mullvad'):
        raise RuntimeError('Mullvad is not installed on this machine.')
    action = request['action'].removeprefix('vpn-')
    if action in ('connect', 'disconnect', 'reconnect'):
        return command(['mullvad', action], timeout=20)
    if action == 'relay':
        words = str(request.get('value') or '').lower().split()
        if not 1 <= len(words) <= 3 or not all(VPN_LOCATION.match(w) for w in words):
            raise ValueError('Relay: a country code, "country city", or a hostname like se-sto-wg-001.')
        result = command(['mullvad', 'relay', 'set', 'location', *words], timeout=20)
        if not result['code'] and (vpn_status() or {}).get('state') not in ('connected', 'connecting'):
            command(['mullvad', 'connect'], timeout=20)
        return result
    raise ValueError('Unsupported VPN action.')


def status(options=None):
    options = options or {}
    before, net_before, started = cpu_sample(), net_sample(), time.monotonic()
    time.sleep(0.25)
    after, net_after, elapsed = cpu_sample(), net_sample(), time.monotonic() - started
    total = after[0] - before[0]
    memory = {line.split(':')[0]: int(line.split()[1]) * 1024 for line in Path('/proc/meminfo').read_text().splitlines()}
    disk = shutil.disk_usage('/')
    batteries = []
    for folder in Path('/sys/class/power_supply').glob('*'):
        if (folder / 'present').exists() and (folder / 'present').read_text().strip() == '0':
            continue
        if (folder / 'type').read_text().strip() == 'Battery' and (folder / 'capacity').exists():
            batteries.append({'percent': int((folder / 'capacity').read_text()), 'state': (folder / 'status').read_text().strip()})
    return {
        'hostname': socket.gethostname(), 'kernel': os.uname().release,
        'cpu': round(100 * (1 - (after[1] - before[1]) / total), 1) if total else 0,
        'memory_used': memory['MemTotal'] - memory.get('MemAvailable', memory.get('MemFree', 0)),
        'memory_total': memory['MemTotal'], 'disk_used': disk.used, 'disk_total': disk.total,
        'uptime': float(Path('/proc/uptime').read_text().split()[0]),
        'load': list(os.getloadavg()), 'battery': batteries,
        'net_rx': max(0, net_after[0] - net_before[0]) / elapsed, 'net_tx': max(0, net_after[1] - net_before[1]) / elapsed,
        'last_upgrade': last_upgrade(), 'disks': [{'name': '/', 'used': disk.used, 'total': disk.total}],
        'vpn': vpn_status(options.get('vpn_state')),
        **sensors(),
    }


def hypr_environment():
    env = os.environ.copy()
    env['XDG_RUNTIME_DIR'] = f'/run/user/{os.getuid()}'
    proc = subprocess.run(['hyprctl', '-j', 'instances'], env=env, capture_output=True, text=True, timeout=5)
    instances = json.loads(proc.stdout)
    if not instances:
        raise RuntimeError('No Hyprland session is running for this user.')
    signature = env.get('HYPRLAND_INSTANCE_SIGNATURE')
    instance = next((item for item in instances if item['instance'] == signature), None)
    if instance is None:
        if len(instances) != 1:
            raise RuntimeError('Several Hyprland sessions are running. Set HYPRLAND_INSTANCE_SIGNATURE to choose one.')
        instance = instances[0]
    env['HYPRLAND_INSTANCE_SIGNATURE'] = instance['instance']
    env['WAYLAND_DISPLAY'] = instance['wl_socket']
    return env


def hypr_query(name, env):
    result = command(['hyprctl', '-j', name], env=env, timeout=5)
    if result['code']:
        raise RuntimeError(result['text'])
    return json.loads(result['text'])


def hypr_action(request):
    env = hypr_environment()
    action = request['action']
    if action == 'hypr':
        return {name: hypr_query(name, env) for name in ('version', 'monitors', 'workspaces', 'clients', 'activeworkspace')}
    if action == 'reload':
        return command(['hyprctl', 'reload'], env=env)
    # Both installed machines use Hyprland 0.57's Lua dispatcher API.
    if action == 'workspace':
        number = str(request.get('value', ''))
        if not re.fullmatch(r'[1-9][0-9]?', number):
            raise ValueError('Workspace must be a number from 1 to 99.')
        lua = 'hl.dispatch(hl.dsp.focus({workspace = ' + json.dumps(number) + '}))'
    elif action == 'focus':
        address = str(request.get('value', ''))
        if not re.fullmatch(r'0x[0-9a-fA-F]+', address):
            raise ValueError('Invalid window address.')
        lua = 'hl.dispatch(hl.dsp.focus({window = ' + json.dumps('address:' + address) + '}))'
    else:
        raise ValueError('Unknown Hyprland action.')
    return command(['hyprctl', 'eval', lua], env=env)


def graphical_session():
    """The logind session that runs this user's Hyprland, so it can be locked from SSH."""
    sessions = json.loads(command(['loginctl', 'list-sessions', '-j'])['text'] or '[]')
    for session in sessions:
        if session.get('uid') == os.getuid() and session.get('seat'):
            return session['session']
    raise RuntimeError('No graphical login session found for this user.')


def session_action(request):
    action, value = request['action'], request.get('value')
    env = hypr_environment()
    env.setdefault('DBUS_SESSION_BUS_ADDRESS', f"unix:path={env['XDG_RUNTIME_DIR']}/bus")
    if action == 'screenshot':
        proc = subprocess.run(['grim', '-t', 'jpeg', '-q', '85', '-'], env=env, capture_output=True, timeout=20)
        if proc.returncode or not proc.stdout:
            raise RuntimeError(proc.stderr.decode(errors='replace').strip() or 'grim could not capture the screen.')
        return {'mime': 'image/jpeg', 'data': base64.b64encode(proc.stdout).decode(), 'bytes': len(proc.stdout)}
    if action == 'lock':
        # hypridle answers logind's Lock signal with its lock_cmd (hyprlock).
        session = graphical_session()
        result = command(['loginctl', 'lock-session', session])
        result['text'] = result['text'] or 'Screen locked.'
        return result
    if action == 'notify':
        value = value if isinstance(value, dict) else {}
        body, sender = str(value.get('text', '')).strip(), str(value.get('from', 'Archfleet'))[:40]
        if not body or len(body) > 500 or any(ord(char) < 32 and char != '\n' for char in body + sender):
            raise ValueError('Write a message of 1–500 characters.')
        result = command(['notify-send', '-a', 'Archfleet', '-i', 'network-workgroup', f'From {sender}', body], env=env)
        result['text'] = result['text'] or 'Notification shown.'
        return result
    if action == 'open-url':
        url = str(value or '').strip()
        if not re.fullmatch(r'https?://[^\s\x00-\x1f\x7f]{1,2000}', url):
            raise ValueError('Enter a full http:// or https:// address.')
        subprocess.Popen(['xdg-open', url], env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        return {'code': 0, 'text': 'Opened ' + url}
    raise ValueError('Unknown session action.')


def dispatch(request):
    action = request.get('action', 'status')
    if action in ('clipboard-get', 'clipboard-set'):
        from clipboards import read_wayland, write_wayland
        env = hypr_environment()
        return read_wayland(env) if action == 'clipboard-get' else write_wayland(request.get('value') or {}, env)
    if action == 'clipboard-history':
        from clipboards import cliphist_history
        limit = request.get('value')
        return cliphist_history(max(1, min(limit, 1000)) if isinstance(limit, int) and not isinstance(limit, bool) else 10)
    if action == 'clipboard-entry':
        from clipboards import cliphist_entry
        return cliphist_entry(request.get('value'))
    if action == 'status':
        return status(request.get('value') if isinstance(request.get('value'), dict) else None)
    if action in ('vpn-connect', 'vpn-disconnect', 'vpn-reconnect', 'vpn-relay'):
        return vpn_action(request)
    if action == 'processes':
        from processes import collect
        return collect()
    if action == 'process-term':
        from processes import terminate
        return terminate(request.get('value'))
    if action in ('hypr', 'workspace', 'focus', 'reload'):
        return hypr_action(request)
    if action in ('screenshot', 'lock', 'notify', 'open-url'):
        return session_action(request)
    if action == 'packages':
        result = command(['pacman', '-Qqe'])
        if result['code']:
            raise RuntimeError(result['text'] or 'pacman -Qqe failed.')
        return {'packages': result['text'].split(), 'last_upgrade': last_upgrade()}
    if action == 'updates':
        if shutil.which('checkupdates'):
            result = command(['checkupdates'], timeout=90)
            if result['code'] == 2:
                return {'code': 0, 'text': 'No repository updates available.'}
            return result
        result = command(['pacman', '-Qu'])
        result['text'] = 'Cached package database (install pacman-contrib for a fresh check):\n' + result['text']
        return result
    commands = {
        'services': ['systemctl', '--failed', '--no-pager'],
        'journal': ['journalctl', '-b', '-p', 'err', '-n', '60', '--no-pager'],
        'storage': ['df', '-h', '-x', 'tmpfs', '-x', 'devtmpfs'],
        'sessions': ['loginctl', 'list-sessions', '--no-pager'],
    }
    if action in commands:
        return command(commands[action])
    raise ValueError('Unsupported action.')


if __name__ == '__main__':
    try:
        request = json.load(sys.stdin) if sys.argv[1] == '--stdin' else json.loads(base64.urlsafe_b64decode(sys.argv[1]))
        print(json.dumps({'ok': True, 'data': dispatch(request)}))
    except Exception as error:
        print(json.dumps({'ok': False, 'error': str(error)}))
        raise SystemExit(1)
