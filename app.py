#!/usr/bin/env python3
"""Archfleet — a home-network cockpit for Arch, Hyprland, and Windows."""
import asyncio
import datetime
import json
import os
import subprocess
import tempfile
import time
from collections import defaultdict
from pathlib import Path

from rich.markup import escape
from rich.text import Text
from textual import on, work
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Button, Checkbox, DataTable, Footer, Header, Input, Label, RichLog, Sparkline, Static, TabbedContent, TabPane, TextArea

from backend import Backend, NODES, lan_label
from clipboards import payload_bytes


def gib(value):
    return f'{float(value) / 1024**3:.1f} GiB'


def uptime(seconds):
    minutes = int(float(seconds)) // 60
    days, hours = divmod(minutes // 60, 24)
    return f'{days}d {hours}h' if days else f'{hours}h {minutes % 60}m'


def meter(percent, width=16):
    count = min(width, max(0, round(percent / 100 * width)))
    return '━' * count + '─' * (width - count)


def size(value):
    """Memory in binary units, short enough for a table cell."""
    if value is None:
        return '—'
    if value >= 1024**3:
        return f'{value / 1024**3:.1f} GiB'
    return f'{value / 1024**2:.0f} MiB'


def speed(value):
    """A byte rate in decimal units: 0, 820 B/s, 12 kB/s, 1.4 MB/s."""
    if value is None:
        return '—'
    for unit, scale in (('GB/s', 1e9), ('MB/s', 1e6), ('kB/s', 1e3)):
        if value >= scale:
            number = value / scale
            return f'{number:.1f} {unit}' if number < 10 else f'{number:.0f} {unit}'
    return f'{value:.0f} B/s' if value else '0'


def percent_text(value):
    if value is None:
        return '—'
    if value < 0.05:
        return '0%'
    return f'{value:.1f}%' if value < 10 else f'{value:.0f}%'


def celsius(value):
    return '—' if value is None else f'{value:.0f}°C'


def mbit(rate):
    return f'{rate * 8 / 1e6:.0f} Mbit/s'


def ago(stamp, now=None):
    """How long ago an ISO timestamp was, in whole days: today, yesterday, 4 days ago."""
    if not stamp:
        return 'unknown'
    when = datetime.datetime.fromisoformat(stamp)
    now = now or datetime.datetime.now(when.tzinfo)
    days = (now.date() - when.astimezone(now.tzinfo).date()).days
    return 'today' if days <= 0 else 'yesterday' if days == 1 else f'{days} days ago'


# Sharing: Marquee's House → Sharing report for the server (streams, encodes, upload, GPU, drives).
MUTED, GOOD, BAD = '#91a3bf', '#79e2c2', '#ed8796'
WHERE = {'internet': 'internet', 'lan': 'home network', 'tv': 'TV'}


def mbits(rate):
    """A byte rate as a bare Mbit/s number: 0, 0.7, 27."""
    value = (rate or 0) * 8 / 1e6
    return '0' if value < 0.05 else f'{value:.1f}' if value < 10 else f'{value:.0f}'


def clock(ms):
    return datetime.datetime.fromtimestamp(ms / 1000).strftime('%H:%M')


def lasted(ms):
    seconds = max(0, round(ms / 1000))
    if seconds < 60:
        return f'{seconds} s'
    if seconds < 3600:
        return f'{seconds // 60} min'
    return f'{seconds // 3600} h {seconds % 3600 // 60} min'


def load_bar(value, maximum, width=14):
    if not maximum:
        return ' ' * width
    percent = 100 * (value or 0) / maximum
    filled = min(width, max(1 if value else 0, round(min(100, percent) / 100 * width)))
    return f"[{BAD if percent >= 85 else GOOD}]{'━' * filled}[/][#2a3c55]{'─' * (width - filled)}[/]"


def stream_is_slow(stream, now_ms):
    """Arriving slower than the file needs, judged by Marquee (older Marquee: the average, for over a minute)."""
    if 'behind' in stream:
        return bool(stream['behind'])
    needs = stream.get('needs')
    return bool(needs) and not stream.get('waiting') and stream.get('avg', 0) < needs * 0.9 and now_ms - stream.get('since', now_ms) > 60000


def sharing_text(load, now_ms=None):
    """Marquee's load report as Rich markup for the server's Overview."""
    now_ms = now_ms or time.time() * 1000
    limits = load.get('limits') or {}
    lines = [f"[{MUTED}]Marquee on the server  ·  checked {clock(load['at'])}[/]"]
    lines += [f'[{BAD}]⚠ {escape(w)}[/]' for w in load.get('warnings') or []]

    def row(name, bar, reading):
        lines.append(f'{name:<13}{bar}  {reading}')
    internet = load.get('internet') or {}
    up = (internet.get('streams') or 0) + (internet.get('torrents') or 0)
    cap = limits.get('uploadMbps')
    parts = [f"streams {mbits(internet.get('streams'))}"]
    if internet.get('torrents') is not None:
        parts.append(f"torrents {mbits(internet['torrents'])}")
    parts.append(f"home network {mbits((load.get('lan') or {}).get('streams'))}")
    row('Internet up', load_bar(up, cap and cap * 1e6 / 8), f"{mbits(up)}{f' / {cap}' if cap else ''} Mbit/s  [{MUTED}]{' · '.join(parts)}[/]")
    net = load.get('net')
    if net:
        link = limits.get('linkMbps')
        row('Network card', load_bar(net['up'], link and link * 1e6 / 8), f"↑ {mbits(net['up'])}  ↓ {mbits(net['down'])}{f' / {link:.0f}' if link else ''} Mbit/s")
    if load.get('cpu') is not None:
        own = load.get('marqueeCpu')
        row('CPU', load_bar(load['cpu'], 100), f"{load['cpu']}%" + (f"  [{MUTED}]Marquee {own}%[/]" if own is not None else ''))
    gpu = load.get('gpu')
    if gpu:
        row('GPU encoder', load_bar(gpu.get('enc'), 100), f"{gpu.get('enc') or 0}%  [{MUTED}]{gpu.get('sessions') or 0} of {limits.get('nvencSessions', 8)} NVENC encodes · GPU {gpu.get('busy') or 0}% · {celsius(gpu.get('temp'))}[/]")
    for disk in sorted((d for d in load.get('disks') or [] if d.get('label')), key=lambda d: d['label'] != 'Media'):
        row(f"{disk['label']} {disk['drive']}:", load_bar(disk['busy'], 100), f"{disk['busy']}% busy  [{MUTED}]reads {speed(disk['read'])} · writes {speed(disk['write'])}[/]")
    if not load.get('sampled'):
        lines.append(f'[{MUTED}]Machine readings are starting up; streams and jobs are counted already.[/]')

    streams = load.get('streams') or []
    lines.append('')
    lines.append(f'[b]STREAMING NOW[/b]  {len(streams)}' if streams else f'[b]STREAMING NOW[/b]  [{MUTED}]nothing is being watched[/]')
    for s in streams:
        slow = stream_is_slow(s, now_ms)
        where = 'internet (from home, via Cloudflare)' if s.get('where') == 'internet' and s.get('fromHome') else WHERE.get(s.get('where'), s.get('where'))
        info = [escape(' · '.join(filter(None, (s.get('who'), s.get('device'))))), escape(str(where)),
                'buffered ahead' if s.get('waiting') else f"{mbits(s.get('rate'))} now"]
        if s.get('speed'):
            info.append(f"arrives at {mbits(s['speed'])}")
        info.append(f"avg {mbits(s.get('avg'))}")
        if s.get('needs'):
            info.append(f"[{BAD}]file needs {mbits(s['needs'])}[/]" if slow else f"needs {mbits(s['needs'])}")
        info.append(f"{size_text(s.get('sent'))} · {lasted(now_ms - s['since'])}")
        lines.append(f"[{BAD if slow else GOOD}]●[/] {escape(s.get('label') or '')}  [{MUTED}]{' · '.join(info)}[/]")

    jobs = load.get('jobs') or []
    lines.append(f'[b]ENCODING NOW[/b]  {len(jobs)}' if jobs else f'[b]ENCODING NOW[/b]  [{MUTED}]no clip copies, conversions or previews[/]')
    for j in jobs:
        info = [j.get('encoder'), f"CPU {j['cpu']}%" if j.get('cpu') is not None else 'CPU …',
                j.get('mem') and f"RAM {size(j['mem'])}", j.get('percent') is not None and f"{j['percent']}%" + (f" · {j['speed']}" if j.get('speed') else ''),
                f"running {lasted(now_ms - j['since'])}"]
        lines.append(f"[{GOOD}]●[/] {escape(j.get('kind') or '')} · {escape(j.get('label') or '')}  [{MUTED}]{escape(' · '.join(filter(None, info)))}[/]")

    top = load.get('top') or []
    if top:
        busiest = ' · '.join('{} {}%'.format(p['name'], p['cpu']) for p in top)
        lines.append(f"[b]BUSIEST[/b]  [{MUTED}]{escape(busiest)}[/]")
    recent = (load.get('recent') or [])[:4]
    if recent:
        lines.append('[b]RECENTLY[/b]')
        for r in recent:
            when = f"{clock(r['start'])}–{clock(r['end'])}"
            if r.get('type') == 'job':
                extra = ' · '.join(filter(None, (r.get('encoder'), r.get('cpu') is not None and f"CPU avg {r['cpu']}%", r.get('ok') is False and 'FAILED')))
                lines.append(f"  [{MUTED}]{when}[/]  {escape(r.get('kind') or '')} · {escape(r.get('label') or '')}  [{MUTED}]{escape(extra)}[/]")
            else:
                extra = f"{r.get('who')} · {WHERE.get(r.get('where'), r.get('where'))} · avg {mbits(r.get('avg'))}, peak {mbits(r.get('peak'))} Mbit/s"
                lines.append(f"  [{MUTED}]{when}[/]  {escape(r.get('label') or '')}  [{MUTED}]{escape(extra)}[/]")
    return '\n'.join(lines)


# Screen Time: `screentime today --json` from each Arch machine (github.com/Tjabooo/screentime).
CATEGORY_COLORS = {'Social': '#e2668a', 'Games': '#ee8a4a', 'Entertainment': '#9b7fe8', 'Web': '#4f92e3',
                   'Development': '#2fb3a4', 'Productivity': '#e0ae37', 'Creativity': '#5dbb6e',
                   'Education': '#c8955a', 'Utilities': '#8e98ab', 'Other': MUTED}
BLOCKS = ' ▁▂▃▄▅▆▇█'


def since(epoch, now=None):
    """Unix seconds -> '40 s ago', '12 min ago', '3 h ago', '2 d ago' (VPN drops need finer steps than ago())."""
    if not epoch:
        return '—'
    seconds = max(0, (now or time.time()) - epoch)
    if seconds < 90:
        return f'{seconds:.0f} s ago'
    if seconds < 5400:
        return f'{seconds / 60:.0f} min ago'
    if seconds < 172800:
        return f'{seconds / 3600:.0f} h ago'
    return f'{seconds / 86400:.0f} d ago'


def npu_line(npu):
    """One short line for the GPU card (fits its ~22 columns): a dot coloured by state, then what the
    NPU (npukit) holds. '' when the machine has no npukit."""
    if not npu:
        return ''
    where = npu.get('runs_on') or 'NPU'  # npukit's GPU backend (llama.cpp) reports 'GPU'
    state = npu.get('state') or '?'
    if npu.get('activity'):
        what = {'rec': 'REC', 'stt': 'speech', 'ocr': 'OCR', 'index': 'indexing'}.get(npu['activity'], npu['activity'])
    else:
        labels = {'asr': 'STT', 'embed': 'EMB'}
        comps = npu.get('components') or []
        parts = [(npu.get('chat_model') or '').replace(':', ' ')] if 'chat' in comps else []
        what = '+'.join(parts + [labels[c] for c in comps if c in labels])
    if state == 'off' and not npu.get('activity'):
        return f'[#596a85]{where} idle[/]'
    color = ('#ed8796' if state == 'error' or npu.get('activity') == 'rec' else
             '#79e2c2' if state == 'ready' and not npu.get('activity') else '#f5c26b')
    return f"{where} [{color}]●[/] {what or state}"


def vpn_short(vpn):
    """One fleet-table cell: 'on · Stockholm', 'off', 'BLOCKED', '—' (no VPN)."""
    if not vpn:
        return '—'
    state = vpn.get('state')
    if state == 'connected':
        text = 'on · ' + (vpn.get('city') or vpn.get('relay') or '?')
    elif state == 'error':
        text = 'BLOCKED'
    elif state in ('connecting', 'disconnecting'):
        text = state + '…'
    else:
        text = 'DOWN' if vpn.get('kind') == 'tunnel' else ('off · lockdown' if vpn.get('lockdown') else 'off')
    return text + (' !' if vpn.get('problems') else '')


def vpn_lines(hostname, vpn, now=None):
    """The VPN tab's detail for one machine."""
    if not vpn:
        return f'{hostname}: no VPN client reported.'
    kind = 'torrent split tunnel (only the torrent client uses it)' if vpn.get('kind') == 'tunnel' else 'Mullvad app'
    lines = [f"{hostname}  ·  {kind}",
             f"state {vpn.get('state')}" + (f"  ·  relay {vpn['relay']}" if vpn.get('relay') else '')
             + (f"  ·  exit {vpn['exit_ip']}" if vpn.get('exit_ip') else '') + (f" ({vpn['city']})" if vpn.get('city') else '')]
    extra = []
    if vpn.get('lockdown') is not None:
        extra.append('lockdown ' + ('on' if vpn['lockdown'] else 'off'))
    extra.append('last drop ' + (since(vpn['last_drop'], now) if vpn.get('last_drop') else 'none recorded'))
    if vpn.get('last_leak'):
        extra.append('last leak alarm ' + since(vpn['last_leak'], now))
    lines.append('  ·  '.join(extra))
    lines += ['! ' + problem for problem in vpn.get('problems') or []]
    return '\n'.join(lines)


def st_duration(seconds):
    """3h 42m · 12m · <1m · 0m"""
    seconds = round(seconds or 0)
    if not seconds:
        return '0m'
    if seconds < 60:
        return '<1m'
    hours, minutes = divmod(seconds // 60, 60)
    return f'{hours}h {minutes:02d}m' if hours else f'{minutes}m'


def screentime_text(data, width=48):
    """One machine's Screen Time today as Rich markup: total, pickups, hourly bars, most used, categories."""
    color = lambda category: CATEGORY_COLORS.get(category, MUTED)
    lines = []
    daemon = data.get('daemon') or {}
    if daemon and not daemon.get('running'):
        lines.append(f"[{BAD}]The screentime daemon isn’t running (systemctl --user start screentime): numbers stop at its last write.[/]")
    head = [f"[b]{st_duration(data.get('total'))}[/b] today"]
    if data.get('pickups') is not None:
        head.append(f"{data['pickups']} pickup{'' if data['pickups'] == 1 else 's'}" + (f", first at {data['first_pickup']}" if data.get('first_pickup') else ''))
    longest = data.get('longest_session')
    if longest:
        head.append(f"longest {st_duration(longest['seconds'])} ({longest['start']}–{longest['end']})")
    if daemon.get('running') and daemon.get('state'):
        app = next((a['name'] for a in data.get('apps') or [] if a['app'] == daemon.get('app')), daemon.get('app'))
        head.append(f"now {escape(daemon['state'])}" + (f" in {escape(app)}" if app and daemon['state'] == 'active' else ''))
    lines.append('  ·  '.join(head))
    hourly = data.get('hourly') or []
    top = max((h.get('seconds') or 0 for h in hourly), default=0)
    if top:
        cols = []
        for hour in hourly:
            seconds = hour.get('seconds') or 0
            cats = hour.get('categories') or {}
            lead = max(cats, key=cats.get) if cats else 'Other'
            block = BLOCKS[max(1, min(8, round(8 * seconds / top)))] if seconds else ' '
            cols.append(f'[{color(lead)}]{block * 2}[/]' if seconds else '  ')
        lines.append(''.join(cols))
        lines.append(f'[{MUTED}]' + ''.join(f'{h:02d}'.ljust(12) for h in (0, 6, 12, 18)) + '[/]')
    apps = data.get('apps') or []
    if apps:
        best = apps[0]['seconds'] or 1
        for app in apps[:5]:
            filled = max(1, round(16 * app['seconds'] / best))
            lines.append(f"{escape(app['name'][:18]):<18} {st_duration(app['seconds']):>7}  [{color(app['category'])}]{'━' * filled}[/][#2a3c55]{'─' * (16 - filled)}[/]  [{MUTED}]{escape(app['category'])}[/]")
    else:
        lines.append(f'[{MUTED}]Nothing used yet today.[/]')
    categories = data.get('categories') or []
    if categories:
        lines.append('   '.join(f"[{color(c['name'])}]●[/] {escape(c['name'])} {st_duration(c['seconds'])}" for c in categories[:4]))
    return '\n'.join(lines)


def size_text(value):
    """Bytes sent, decimal: 858 MB, 3.6 GB."""
    value = value or 0
    for unit, scale in (('GB', 1e9), ('MB', 1e6), ('kB', 1e3)):
        if value >= scale:
            return f'{value / scale:.1f} {unit}' if value / scale < 10 else f'{value / scale:.0f} {unit}'
    return f'{value:.0f} B'


WAKE_TIMEOUT = 120
WAKE_POLL = 5
RUN_TIMEOUT = 60


def pair_sum(first, second):
    return None if first is None or second is None else first + second


# Process table: key, header, width, numeric. Numeric columns sort highest first.
PROCESS_COLUMNS = [
    ('pid', 'PID', 7, False), ('name', 'Process', 16, False), ('user', 'User', 8, False),
    ('cpu', 'CPU', 6, True), ('gpu', 'GPU', 6, True), ('memory', 'Memory', 9, True),
    ('disk', 'Disk', 10, True), ('net', 'Net TCP', 10, True),
]
PROCESS_LIMIT = 200
PROCESS_INTERVAL = 3
SCREENTIME_INTERVAL = 60  # seconds between Screen Time reads of the selected Arch machine


def process_value(process, key):
    if key == 'disk':
        return pair_sum(process.get('disk_read'), process.get('disk_write'))
    if key == 'net':
        return pair_sum(process.get('net_rx'), process.get('net_tx'))
    return process.get(key)


def process_rows(processes, text='', mine=False, sort='cpu', descending=True):
    """Filter by name, command, user or PID, then sort. Unreadable values sort last."""
    needle = text.strip().lower()
    rows = [p for p in processes if (not mine or p.get('own')) and (not needle or needle in ' '.join(
        (str(p['pid']), p.get('name') or '', p.get('command') or '', p.get('user') or '')).lower())]
    numeric = next(column[3] for column in PROCESS_COLUMNS if column[0] == sort)
    if numeric:
        present = sorted((p for p in rows if process_value(p, sort) is not None), key=lambda p: (process_value(p, sort), -p['pid']), reverse=descending)
        return present + [p for p in rows if process_value(p, sort) is None]
    key = (lambda p: p['pid']) if sort == 'pid' else (lambda p: ((p.get(sort) or '').lower(), p['pid']))
    return sorted(rows, key=key, reverse=descending)


class CPUHistory(Sparkline):
    """Plot CPU on a fixed 0–100% scale, including the first sample."""
    def render(self):
        data = self.data or []
        if not data:
            return Text('Waiting for CPU samples…', style='#91a3bf')
        width = max(1, self.size.width)
        levels = ' ▁▂▃▄▅▆▇█'
        values = [max(0, min(24, round(data[min(len(data) - 1, x * len(data) // width)] * 24 / 100))) for x in range(width)]
        rows = [''.join(levels[max(0, min(8, value - (2 - row) * 8))] for value in values) for row in range(3)]
        return Text('\n'.join(rows), style='#79e2c2')


class UploadHistory(Sparkline):
    """Internet upload per minute, scaled to its own peak."""
    def render(self):
        data = self.data or []
        if len(data) < 2:
            return Text('History builds up a minute at a time…', style=MUTED)
        width = max(1, self.size.width)
        top = max(data) or 1
        levels = ' ▁▂▃▄▅▆▇█'
        values = [max(0, min(24, round(data[min(len(data) - 1, x * len(data) // width)] * 24 / top))) for x in range(width)]
        rows = [''.join(levels[max(0, min(8, value - (2 - row) * 8))] for value in values) for row in range(3)]
        return Text('\n'.join(rows), style=GOOD)


class Confirm(ModalScreen[bool]):
    BINDINGS = [('escape', 'cancel', 'Cancel')]

    def __init__(self, message, action='Continue'):
        super().__init__()
        self.message = message
        self.action_label = action

    def compose(self) -> ComposeResult:
        with Vertical(id='confirm-dialog'):
            yield Static(self.message, markup=False)
            with Horizontal(classes='actions'):
                yield Button('Cancel', id='cancel')
                yield Button(self.action_label, variant='primary', id='continue')

    def on_mount(self):
        self.query_one('#cancel', Button).focus()

    @on(Button.Pressed)
    def answer(self, event):
        self.dismiss(event.button.id == 'continue')

    def action_cancel(self):
        self.dismiss(False)


class Prompt(ModalScreen):
    """Ask for one line of text. Dismisses with the text, or None when cancelled or empty."""
    BINDINGS = [('escape', 'cancel', 'Cancel')]

    def __init__(self, title, placeholder, action, value=''):
        super().__init__()
        self.title_text, self.placeholder, self.action_label, self.value = title, placeholder, action, value

    def compose(self) -> ComposeResult:
        with Vertical(id='prompt-dialog'):
            yield Static(self.title_text, markup=False, classes='dialog-title')
            yield Input(self.value, placeholder=self.placeholder, id='prompt-input')
            with Horizontal(classes='actions'):
                yield Button('Cancel', id='prompt-cancel')
                yield Button(self.action_label, variant='primary', id='prompt-ok')

    def on_mount(self):
        self.query_one('#prompt-input', Input).focus()

    def submit(self):
        self.dismiss(self.query_one('#prompt-input', Input).value.strip() or None)

    @on(Input.Submitted)
    def submitted(self, event):
        event.stop()
        self.submit()

    @on(Button.Pressed)
    def answer(self, event):
        event.stop()
        self.submit() if event.button.id == 'prompt-ok' else self.dismiss(None)

    def action_cancel(self):
        self.dismiss(None)


class HistoryInput(Input):
    """An input that steps through earlier entries with Up and Down."""
    BINDINGS = [Binding('up', 'older', show=False), Binding('down', 'newer', show=False)]

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.history, self.position = [], 0

    def remember(self, text):
        if text and (not self.history or self.history[-1] != text):
            self.history.append(text)
        self.position = len(self.history)

    def action_older(self):
        if self.position > 0:
            self.position -= 1
            self.value = self.history[self.position]
            self.cursor_position = len(self.value)

    def action_newer(self):
        if self.position < len(self.history):
            self.position += 1
            self.value = self.history[self.position] if self.position < len(self.history) else ''
            self.cursor_position = len(self.value)


class Archfleet(App):
    TITLE = 'ARCHFLEET'
    SUB_TITLE = 'home network / ' + (['no machines', 'one machine', 'two machines', 'three machines', 'four machines', 'five machines'][len(NODES)] if len(NODES) < 6 else f'{len(NODES)} machines')
    CSS_PATH = 'style.tcss'
    BINDINGS = [
        *((str(i), f"select_node('{n['alias']}')", n['label']) for i, n in enumerate(NODES[:9], 1)),
        ('r', 'refresh', 'Refresh'), ('s', 'shell', 'SSH'),
        ('f', 'sftp', 'SFTP'), ('p', 'processes', 'Processes'), Binding('o', 'process_sort', 'Sort processes', show=False),
        ('c', 'clipboards', 'Clipboard'), ('w', 'wake', 'Wake'), ('q', 'quit', 'Quit'),
    ]

    def __init__(self, backend=None, initial_tab='overview'):
        super().__init__()
        self.backend = backend or Backend()
        self.selected = next((n for n in NODES if self.backend.is_local(n)), NODES[0])
        self.statuses = {}
        self.histories = defaultdict(list)
        self.hypr_data = {}
        self.transfer_running = False
        self.local_node = next((n for n in NODES if self.backend.is_local(n)), NODES[0])
        self.initial_tab = initial_tab
        self.clipboard_values = {}
        self.clipboard_generations = defaultdict(int)
        self.clipboard_busy = False
        self.clipboard_previews = []
        self.process_data = None
        self.process_node = None
        self.process_sort = ('cpu', True)
        self.process_busy = False
        self.process_generation = 0
        self.process_error = None
        self.process_updated = ''
        self.waking = {}
        self.speed_results = {}
        self.speed_running = None
        self.run_running = False
        self.prompt_values = {}
        self.sharing = None
        self.sharing_error = None
        self.screentime = {}  # alias -> {'data', 'error', 'at'}

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        with Horizontal(id='body'):
            with Vertical(id='sidebar'):
                yield Static('YOUR MACHINES', classes='eyebrow')
                for index, node in enumerate(NODES, 1):
                    yield Button(f"{index}  {node['label']}\n{node['hostname']}", id='select-' + node['alias'], classes='node')
                yield Static('PRIVATE LAN\n' + lan_label(NODES), id='network-label')
                yield Static('SSH keys stay on\ntheir own machines.', classes='muted')
            with Vertical(id='main'):
                with Horizontal(id='heading-row'):
                    yield Static('', id='node-heading', markup=False)
                    yield Static('', id='wake-state', markup=False)
                    yield Button('Wake  W', id='wake', variant='primary')
                with TabbedContent(initial=self.initial_tab, id='tabs'):
                    with TabPane('Overview', id='overview'):
                        with VerticalScroll():
                            with Horizontal(id='metrics'):
                                yield Static('CPU\nWaiting…', id='cpu', classes='metric')
                                yield Static('GPU\nWaiting…', id='gpu', classes='metric')
                                yield Static('MEMORY\nWaiting…', id='memory', classes='metric')
                                yield Static('STORAGE\nWaiting…', id='disk', classes='metric')
                            yield Static('CPU HISTORY  ·  0–100%  ·  15 s/sample', classes='eyebrow')
                            yield CPUHistory([], id='history')
                            yield Static('Connecting to your machines…', id='details', markup=False)
                            with Horizontal(classes='actions'):
                                yield Button('SSH shell', id='shell', variant='primary')
                                yield Button('SFTP browser', id='sftp')
                                yield Button('tmux session', id='tmux', classes='arch-action')
                                yield Button('Test link', id='speed')
                            yield Static('', id='speed-result', markup=False)
                            with Vertical(id='sharing-box'):
                                yield Static('SHARING  ·  what streams and encodes cost the server', classes='eyebrow')
                                yield Static('Reading Marquee…', id='sharing')
                                yield Static('INTERNET UPLOAD  ·  last hour', classes='eyebrow', id='upload-label')
                                yield UploadHistory([], id='upload-history')
                            with Vertical(id='screentime-box'):
                                yield Static('SCREEN TIME  ·  today', classes='eyebrow', id='screentime-label')
                                yield Static('Reading Screen Time…', id='screentime')
                            yield DataTable(id='fleet', cursor_type='row', zebra_stripes=True)
                    with TabPane('Processes', id='processes'):
                        with Vertical(id='process-pane'):
                            with Horizontal(id='process-summary'):
                                for key in ('cpu', 'gpu', 'memory', 'disk', 'net'):
                                    yield Static('—', id='proc-' + key, classes='proc-metric')
                            with Horizontal(id='process-controls'):
                                yield Input(placeholder='Filter name, command, user, PID', id='proc-filter')
                                yield Checkbox('Only mine', id='proc-mine')
                                yield Button('End process…', id='proc-end', disabled=True)
                            yield Static('Open this tab to read the selected machine’s processes.', id='proc-status', markup=False)
                            yield DataTable(id='proc-table', cursor_type='row', zebra_stripes=True)
                            yield Static('', id='proc-detail', markup=False)
                            yield Static('', id='proc-note', markup=False)
                    with TabPane('Session', id='hyprland'):
                        with VerticalScroll():
                            with Horizontal(classes='actions'):
                                yield Button('Peek screen', id='peek', classes='arch-action', variant='primary')
                                yield Button('Lock screen…', id='lock', classes='arch-action')
                                yield Button('Notify…', id='notify', classes='arch-action')
                                yield Button('Open URL…', id='open-url', classes='arch-action')
                            yield Static('Select an Arch machine to inspect its active session.', id='hypr-summary', markup=False)
                            with Horizontal(classes='actions'):
                                yield Button('Refresh session', id='hypr-refresh', classes='arch-action', variant='primary')
                                yield Button('Reload config', id='hypr-reload', classes='arch-action')
                            yield DataTable(id='workspaces', cursor_type='row', zebra_stripes=True)
                            with Horizontal(classes='actions'):
                                yield Input(placeholder='Workspace 1–99', id='workspace', classes='arch-action', type='integer')
                                yield Button('Go to workspace', id='workspace-go', classes='arch-action')
                            yield DataTable(id='windows', cursor_type='row', zebra_stripes=True)
                            yield Button('Focus selected window', id='window-focus', classes='arch-action')
                    with TabPane('VPN', id='vpn'):
                        with VerticalScroll():
                            yield Static('VPN  ·  every machine, read with the status every 15 s', classes='eyebrow')
                            yield DataTable(id='vpn-table', cursor_type='row', zebra_stripes=True)
                            yield Static('Waiting for the first status…', id='vpn-detail', markup=False)
                            with Horizontal(classes='actions'):
                                yield Button('Connect', id='vpn-connect', classes='vpn-linux', variant='primary')
                                yield Button('Disconnect…', id='vpn-disconnect', classes='vpn-linux')
                                yield Button('Reconnect', id='vpn-reconnect', classes='vpn-linux')
                                yield Button('Switch relay…', id='vpn-relay', classes='vpn-linux')
                                yield Button('Restart tunnel…', id='vpn-restart', classes='vpn-windows')
                                yield Button('Test tunnel', id='vpn-test', classes='vpn-windows')
                            yield RichLog(id='vpn-log', wrap=True, markup=False)
                    with TabPane('Files', id='files'):
                        with VerticalScroll():
                            yield Static('TRANSFER A FILE', classes='eyebrow')
                            yield Static('Paths below are relative to the selected machine’s home directory unless absolute.\nThe local path is on the machine running Archfleet.', classes='explanation')
                            yield Label('Local file')
                            yield Input(placeholder='~/Downloads/example.zip', id='local-path')
                            yield Label('Path on selected machine')
                            yield Input(placeholder='Downloads/example.zip (Windows also accepts C:/...)', id='remote-path')
                            with Horizontal(classes='actions'):
                                yield Button('Upload →', id='upload', variant='primary')
                                yield Button('← Download', id='download')
                                yield Button('Open SFTP', id='file-sftp')
                            yield Static('Ready to transfer.', id='transfer-status', markup=False)
                    with TabPane('Clipboard', id='clipboards'):
                        with VerticalScroll():
                            with Horizontal(classes='actions'):
                                yield Button('Refresh all', id='clipboard-refresh-all', variant='primary')
                                yield Static('Text and PNG · Copy here copies the displayed snapshot.', classes='clipboard-help')
                            yield Static('Open this tab or press Refresh to read each machine’s clipboard.', id='clipboard-result', markup=False)
                            for node in NODES:
                                alias = node['alias']
                                with Vertical(id='clip-card-' + alias, classes='clipboard-card'):
                                    yield Static(f"{node['label']} / {node['hostname']}", classes='clipboard-title', markup=False)
                                    yield TextArea('', read_only=True, soft_wrap=True, id='clip-text-' + alias, classes='clipboard-text')
                                    yield Static('Not fetched yet.', id='clip-state-' + alias, classes='clipboard-state', markup=False)
                                    with Horizontal(classes='actions'):
                                        yield Button('Refresh', id='clip-refresh-' + alias)
                                        yield Button('Copy here', id='clip-copy-' + alias, variant='primary', disabled=True)
                                        yield Button('Send local here', id='clip-send-' + alias, disabled=self.backend.is_local(node))
                                        yield Button('View image', id='clip-view-' + alias, disabled=True)
                    with TabPane('Run', id='run'):
                        with Vertical(id='run-pane'):
                            yield Static('RUN ON FLEET  ·  the same command on every ticked machine', classes='eyebrow')
                            with Horizontal(id='run-controls'):
                                yield HistoryInput(placeholder='Command, e.g. uptime   (↑ ↓ earlier commands)', id='run-command')
                                yield Button('Run on 2', id='run-go', variant='primary')
                            with Horizontal(id='run-targets'):
                                for node in NODES:
                                    label = node['hostname'] + (' · cmd' if node['os'] != 'linux' else '')
                                    yield Checkbox(label, node['os'] == 'linux', id='run-target-' + node['alias'])
                            yield Static('Linux machines run it in your login shell, Windows machines in cmd.exe. Each stops after 60 s.', id='run-status', markup=False)
                            yield RichLog(id='run-output', wrap=True, markup=False)
                    with TabPane('Maintenance', id='maintenance'):
                        with VerticalScroll():
                            with Horizontal(classes='actions'):
                                yield Static('UPDATES', classes='group-label')
                                yield Button('Check updates', id='updates', classes='arch-action', variant='primary')
                                yield Button('Arch news', id='news')
                                yield Button('Upgrade system…', id='upgrade', classes='arch-action')
                            with Horizontal(classes='actions'):
                                yield Static('DIAGNOSE', classes='group-label')
                                yield Button('Failed services', id='services', classes='arch-action')
                                yield Button('Boot errors', id='journal', classes='arch-action')
                                yield Button('Disk usage', id='storage', classes='arch-action')
                                yield Button('Login sessions', id='sessions', classes='arch-action')
                            with Horizontal(classes='actions'):
                                yield Static('FLEET', classes='group-label')
                                yield Button('Compare packages', id='packages')
                                yield Static('Explicitly installed packages, first two Arch machines', classes='group-help')
                            yield RichLog(id='report', wrap=True, markup=False)
                yield RichLog(id='activity', wrap=True, markup=False, max_lines=150)
        yield Footer()

    def on_mount(self):
        self.query_one('#fleet', DataTable).add_columns('Machine', 'Link', 'CPU', 'Memory', 'Uptime', 'VPN')
        self.query_one('#vpn-table', DataTable).add_columns('Machine', 'VPN', 'State', 'Relay', 'Exit IP', 'Last drop')
        self.query_one('#vpn-log', RichLog).border_title = 'VPN RESULTS'
        self.query_one('#workspaces', DataTable).add_columns('Workspace', 'Monitor', 'Windows')
        self.query_one('#windows', DataTable).add_columns('App', 'Window title', 'Workspace')
        table = self.query_one('#proc-table', DataTable)
        for key, label, width, _ in PROCESS_COLUMNS:
            table.add_column(label, width=width, key=key)
        for key, title in (('cpu', 'CPU'), ('gpu', 'GPU'), ('memory', 'MEMORY'), ('disk', 'DISK'), ('net', 'NETWORK')):
            self.query_one('#proc-' + key, Static).border_title = title
        self.query_one('#activity', RichLog).border_title = 'ACTIVITY'
        self.query_one('#report', RichLog).border_title = 'RESULTS'
        self.query_one('#report', RichLog).auto_scroll = False
        self.action_select_node(self.selected['alias'])
        self.log_event('Welcome home. Select a machine to get started.')
        self.refresh_status()
        self.set_interval(15, self.refresh_status)
        self.set_interval(PROCESS_INTERVAL, self.process_tick)
        self.set_interval(1, self.wake_tick)
        self.set_interval(SCREENTIME_INTERVAL, self.screentime_tick)

    def log_event(self, text):
        timestamp = datetime.datetime.now().strftime('%H:%M:%S')
        self.query_one('#activity', RichLog).write(Text(f'{timestamp}  {text}'))

    def action_select_node(self, alias):
        self.selected = next(n for n in NODES if n['alias'] == alias)
        node = self.selected
        for item in NODES:
            self.query_one('#select-' + item['alias'], Button).set_class(item == node, 'selected')
        for widget in self.query('.arch-action'):
            widget.disabled = node['os'] != 'linux'
        self.query_one('#node-heading', Static).update(f"{node['label']}  /  {node['hostname']}     {node['user']}@{node['address']}")
        self.query_one('#hypr-summary', Static).update('Refresh to inspect this Hyprland session.' if node['os'] == 'linux' else 'Hyprland controls are available on the desktop and laptop.')
        self.query_one('#windows', DataTable).clear()
        self.query_one('#workspaces', DataTable).clear()
        self.query_one('#report', RichLog).clear()
        self.hypr_data = {}
        self.render_status()
        self.render_speed()
        self.screentime_tick()
        if self.query_one('#tabs', TabbedContent).active == 'hyprland' and node['os'] == 'linux':
            self.refresh_hypr()
        if self.process_node != node:
            self.process_data, self.process_node, self.process_error = None, None, None
            self.render_processes('')
        if self.query_one('#tabs', TabbedContent).active == 'processes':
            self.refresh_processes()

    @work(exclusive=True, group='status')
    async def refresh_status(self):
        async def sharing():
            loader = getattr(self.backend, 'marquee_load', None)
            if not loader or not any(n.get('marquee') for n in NODES):
                return
            try:
                self.sharing, self.sharing_error = await loader(), None
            except Exception as error:
                self.sharing_error = str(error) or type(error).__name__
        *results, _ = await asyncio.gather(*(self.backend.status(n) for n in NODES), sharing())
        for node, result in zip(NODES, results):
            self.statuses[node['alias']] = result
            if result.get('online'):
                history = self.histories[node['alias']]
                history.append(result.get('cpu') or 0)
                del history[:-40]
        self.render_status()

    def render_status(self):
        table = self.query_one('#fleet', DataTable)
        table.clear()
        for node in NODES:
            data = self.statuses.get(node['alias'], {})
            online = data.get('online')
            state = Text('online' if online else ('offline' if data else 'checking'), style='#79e2c2' if online else '#ed8796')
            vpn = data.get('vpn') if online else None
            vpn_cell = Text(vpn_short(vpn), style='#ed8796' if vpn and not vpn.get('ok', True) else '#79e2c2' if vpn and vpn.get('state') == 'connected' else '#91a3bf')
            table.add_row(node['hostname'], state, f"{data.get('cpu') or 0:.0f}%" if online else '—', gib(data['memory_used']) if online else '—', uptime(data['uptime']) if online else '—', vpn_cell, key=node['alias'])
            self.query_one('#select-' + node['alias'], Button).label = f"{'●' if online else '○'}  {node['label']}\n{node['hostname']}"
        data = self.statuses.get(self.selected['alias'], {})
        if data.get('online'):
            cpu = data.get('cpu') or 0
            cpu_line = f'{cpu:.1f}%' + (f"  ·  {celsius(data['cpu_temp'])}" if data.get('cpu_temp') is not None else '')
            self.query_one('#cpu', Static).update(f'[b]CPU[/b]\n{cpu_line}\n[#79e2c2]{meter(cpu)}[/]')
            busy, gpu_temp = data.get('gpu_busy'), data.get('gpu_temp')
            if busy is None and gpu_temp is None and self.selected.get('marquee') and (self.sharing or {}).get('gpu'):
                busy, gpu_temp = self.sharing['gpu'].get('busy'), self.sharing['gpu'].get('temp')  # read by Marquee (nvidia-smi)
            if busy is None and gpu_temp is None:
                npu = npu_line(data.get('npu'))
                self.query_one('#gpu', Static).update('[b]GPU[/b]\n[#596a85]No readings[/]' + (f'\n{npu}' if npu else ''))
            else:
                line = '  ·  '.join(part for part in (None if busy is None else f'{busy}% busy', None if gpu_temp is None else celsius(gpu_temp)) if part)
                npu = npu_line(data.get('npu'))
                self.query_one('#gpu', Static).update(f'[b]GPU[/b]\n{line}\n[#79e2c2]{meter(busy or 0)}[/]' + (f'\n{npu}' if npu else ''))
            used = 100 * data['memory_used'] / max(1, data['memory_total'])
            self.query_one('#memory', Static).update(f"[b]MEMORY[/b]\n{gib(data['memory_used'])} / {gib(data['memory_total'])}\n[#79e2c2]{meter(used)}[/]")
            disks = data.get('disks') or [{'name': 'C:' if self.selected['os'] != 'linux' else '/', 'used': data['disk_used'], 'total': data['disk_total']}]
            whole = lambda value: f'{value / 1024**3:.0f}' if value >= 100 * 1024**3 else f'{value / 1024**3:.1f}'
            if len(disks) == 1:
                disk = disks[0]
                text = f"[b]STORAGE[/b]\n{whole(disk['used'])} / {whole(disk['total'])} GiB\n[#79e2c2]{meter(100 * disk['used'] / max(1, disk['total']))}[/]"
            else:
                rows = [f"{d['name']} {whole(d['used'])}/{whole(d['total'])}G [#91a3bf]{100 * d['used'] / max(1, d['total']):.0f}%[/]" for d in disks[:3]]
                text = '[b]STORAGE[/b]\n' + '\n'.join(rows)
            self.query_one('#disk', Static).update(text)
            first = [f"↓ {speed(data.get('net_rx'))}  ↑ {speed(data.get('net_tx'))}" if data.get('net_rx') is not None else None,
                     f"Uptime {uptime(data['uptime'])}", data['kernel']]
            second = []
            if self.selected['os'] == 'linux':
                second.append(f"Last full upgrade {ago(data.get('last_upgrade'))}")
            batteries = data.get('battery') or []
            if isinstance(batteries, dict):
                batteries = [batteries]
            if batteries:
                second.append('Battery ' + ', '.join(f"{b['percent']}% {b['state']}" for b in batteries))
            second.append(f"status read in {data.get('elapsed_ms', 0)} ms")
            self.query_one('#details', Static).update('   ·   '.join(filter(None, first)) + '\n' + '   ·   '.join(second))
        else:
            for metric, label in [('cpu', 'CPU'), ('gpu', 'GPU'), ('memory', 'MEMORY'), ('disk', 'STORAGE')]:
                self.query_one('#' + metric, Static).update(label + '\n—')
            self.query_one('#details', Static).update(data.get('error', 'Checking connection…'))
        self.query_one('#history', Sparkline).data = list(self.histories[self.selected['alias']])
        self.render_sharing()
        self.render_screentime()
        self.render_wake()
        self.render_speed()
        self.render_vpn()

    def render_vpn(self):
        table = self.query_one('#vpn-table', DataTable)
        table.clear()
        for node in NODES:
            data = self.statuses.get(node['alias'], {})
            vpn = data.get('vpn') if data.get('online') else None
            kind = '' if not vpn else ('torrent tunnel' if vpn.get('kind') == 'tunnel' else 'Mullvad')
            state = Text(vpn_short(vpn) if data.get('online') else ('offline' if data else 'checking'),
                         style='#ed8796' if vpn and not vpn.get('ok', True) else '#79e2c2' if vpn and vpn.get('state') == 'connected' else '#91a3bf')
            table.add_row(node['hostname'], kind or '—', state, (vpn or {}).get('relay') or '—', (vpn or {}).get('exit_ip') or '—',
                          since(vpn.get('last_drop')) if vpn else '—', key=node['alias'])
        node = self.selected
        data = self.statuses.get(node['alias'], {})
        vpn = data.get('vpn') if data.get('online') else None
        self.query_one('#vpn-detail', Static).update(vpn_lines(node['hostname'], vpn) if data.get('online') else f"{node['hostname']} is offline." if data else 'Checking…')
        linux_ok = node['os'] == 'linux' and bool(vpn) and vpn.get('kind') == 'mullvad'
        for widget in self.query('.vpn-linux'):
            widget.disabled = not linux_ok
        cfg = node.get('vpn') if isinstance(node.get('vpn'), dict) else {}
        self.query_one('#vpn-restart', Button).disabled = node['os'] == 'linux' or not cfg.get('restart')
        self.query_one('#vpn-test', Button).disabled = node['os'] == 'linux' or not cfg.get('test')

    @work(group='vpn')
    async def vpn_do(self, node, action, value=None):
        log = self.query_one('#vpn-log', RichLog)
        log.write(Text(f"{node['hostname']}: {action.removeprefix('vpn-')}{' ' + value if value else ''}…", style='#91a3bf'))
        try:
            result = await self.backend.call(node, action, value)
            text = (result.get('text') or '').strip()
            log.write(Text(text or 'done', style='#ed8796' if result.get('code') else ''))
            self.log_event(f"{node['hostname']} / {action}: {'failed' if result.get('code') else 'done'}")
            if result.get('code'):
                self.notify(text[:250] or 'VPN action failed', severity='error')
        except Exception as error:
            log.write(Text(str(error), style='#ed8796'))
            self.notify(str(error), severity='error')
        await asyncio.sleep(2)   # let the tunnel settle before the table re-reads it
        self.refresh_status()

    def screentime_tick(self):
        """Refresh the selected Arch machine's Screen Time when it's older than a minute."""
        node = self.selected
        if node['os'] != 'linux' or not hasattr(self.backend, 'screentime'):
            return
        entry = self.screentime.get(node['alias'])
        if not entry or time.monotonic() - entry['at'] >= SCREENTIME_INTERVAL - 1:
            self.refresh_screentime(node)

    @work(group='screentime')
    async def refresh_screentime(self, node):
        entry = self.screentime.setdefault(node['alias'], {'data': None, 'error': None, 'at': 0})
        entry['at'] = time.monotonic()
        try:
            entry['data'], entry['error'] = await self.backend.screentime(node), None
            entry['read'] = datetime.datetime.now().strftime('%H:%M')
        except Exception as error:
            entry['error'] = str(error) or type(error).__name__
        if self.selected['alias'] == node['alias']:
            self.render_screentime()

    def render_screentime(self):
        box = self.query_one('#screentime-box')
        node = self.selected
        box.display = node['os'] == 'linux' and hasattr(self.backend, 'screentime')
        if not box.display:
            return
        entry = self.screentime.get(node['alias']) or {}
        data, error = entry.get('data'), entry.get('error')
        label = f"SCREEN TIME  ·  today on {node['hostname']}" + (f"  ·  read {entry['read']}" if data and entry.get('read') else '')
        self.query_one('#screentime-label', Static).update(label)
        if data:
            text = screentime_text(data)
            if error:
                text = f'[{BAD}]Last read failed: {escape(error)}[/]\n' + text
        else:
            text = f'[{BAD}]{escape(error)}[/]' if error else 'Reading Screen Time…'
        self.query_one('#screentime', Static).update(text)

    def render_sharing(self):
        box = self.query_one('#sharing-box')
        box.display = bool(self.selected.get('marquee'))  # "marquee": true in hosts.json
        if not box.display:
            return
        load = self.sharing
        if load:
            text = sharing_text(load)
            if self.sharing_error:
                text = f'[{BAD}]Last read failed: {escape(self.sharing_error)} (showing {clock(load["at"])})[/]\n' + text
        else:
            text = f'[{BAD}]{escape(self.sharing_error)}[/]' if self.sharing_error else 'Reading Marquee…'
        self.query_one('#sharing', Static).update(text)
        history = [((p.get('inet') or 0) + (p.get('tor') or 0)) * 8 / 1e6 for p in (load or {}).get('history') or []]
        peak = max(history, default=0)
        self.query_one('#upload-label', Static).update(f"INTERNET UPLOAD  ·  last hour  ·  peak {peak:.0f} Mbit/s" + (f"  ·  limit {load['limits']['uploadMbps']}" if load and (load.get('limits') or {}).get('uploadMbps') else ''))
        self.query_one('#upload-history', Sparkline).data = history

    # Wake-on-LAN ---------------------------------------------------------------------------

    def is_offline(self, node):
        data = self.statuses.get(node['alias'])
        return bool(data) and not data.get('online')

    def render_wake(self):
        node = self.selected
        state, button = self.query_one('#wake-state', Static), self.query_one('#wake', Button)
        waking = self.waking.get(node['alias'])
        if waking and waking['state'] == 'waiting':
            elapsed = int(time.monotonic() - waking['started'])
            state.update(Text(f'◌ Waking · {elapsed // 60}:{elapsed % 60:02d} / 2:00', style='#c7b4f8'))
            button.display = False
        elif self.is_offline(node) and node.get('mac'):
            timed_out = bool(waking) and waking['state'] == 'timeout'
            state.update(Text('No answer after 2 minutes' if timed_out else '○ Offline', style='#ed8796'))
            button.label = 'Retry wake' if timed_out else 'Wake  W'
            button.display = True
        elif self.is_offline(node):
            state.update(Text('○ Offline · Wi-Fi only, wake it by hand', style='#ed8796'))
            button.display = False
        else:
            state.update('')
            button.display = False

    def wake_tick(self):
        if any(w['state'] == 'waiting' for w in self.waking.values()):
            self.render_wake()

    def action_wake(self):
        node = self.selected
        if not self.is_offline(node):
            self.notify(f"{node['hostname']} is already online." if self.statuses.get(node['alias']) else 'Still checking this machine.')
            return
        if not node.get('mac'):
            self.notify(f"{node['hostname']} can't be woken over the network (no wired MAC in hosts.json).", severity='warning')
            return
        if self.waking.get(node['alias'], {}).get('state') == 'waiting':
            return
        try:
            message = self.backend.wake(node)
        except Exception as error:
            self.notify(str(error), severity='error')
            return
        self.log_event(message)
        self.waking[node['alias']] = {'started': time.monotonic(), 'state': 'waiting'}
        self.render_wake()
        self.wait_for_wake(node)

    @work(group='wake')
    async def wait_for_wake(self, node):
        alias = node['alias']
        started = self.waking[alias]['started']
        while time.monotonic() - started < WAKE_TIMEOUT:
            await asyncio.sleep(WAKE_POLL)
            result = await self.backend.status(node)
            if result.get('online'):
                self.waking.pop(alias, None)
                self.statuses[alias] = result
                self.log_event(f"{node['hostname']} is awake.")
                self.notify(f"{node['hostname']} is awake")
                self.render_status()
                return
        self.waking[alias]['state'] = 'timeout'
        self.log_event(f"{node['hostname']} did not answer within 2 minutes of the wake packet.")
        self.render_wake()

    # Link speed test ------------------------------------------------------------------------

    def render_speed(self):
        node = self.selected
        button = self.query_one('#speed', Button)
        button.disabled = self.backend.is_local(node) or self.is_offline(node) or self.speed_running is not None
        if self.backend.is_local(node):
            text = Text('Test link measures SSH speed to another machine; this is the one you are on.', style='#596a85')
        else:
            text = self.speed_results.get(node['alias']) or Text('Test link: latency and SSH speed both ways, 64 MB each (takes a few seconds).', style='#596a85')
        self.query_one('#speed-result', Static).update(text)

    @work(exclusive=True, group='speed')
    async def run_speed_test(self, node):
        alias, name = node['alias'], node['hostname']
        self.speed_running = alias
        phases = {'latency': 'Measuring latency…', 'upload': f'Sending 64 MB to {name}…', 'download': f'Receiving 64 MB from {name}…'}

        def describe(results, tail=None, style='#a6b7d0'):
            parts = []
            if 'latency_ms' in results:
                parts.append(f"ping {results['latency_ms']:.1f} ms")
            if 'upload' in results:
                parts.append(f"to {name} {mbit(results['upload'])}")
            if 'download' in results:
                parts.append(f"from {name} {mbit(results['download'])}")
            if tail:
                parts.append(tail)
            return Text('   ·   '.join(parts), style=style)

        measured = {}

        def progress(phase, results):
            measured.update(results)
            self.speed_results[alias] = describe(results, phases[phase], '#c7b4f8')
            self.render_speed()

        try:
            results = await self.backend.speed_test(node, progress=progress)
            stamp = datetime.datetime.now().strftime('%H:%M')
            self.speed_results[alias] = describe(results, f'tested {stamp}', '#79e2c2')
            self.log_event(f"{name} link: {describe(results).plain}")
        except Exception as error:
            self.speed_results[alias] = describe(measured, f'test failed: {error}', '#ed8796')
            self.log_event(f"{name} link test failed: {error}")
        finally:
            self.speed_running = None
            self.render_speed()

    def action_refresh(self):
        self.refresh_status()
        if self.query_one('#tabs', TabbedContent).active == 'hyprland' and self.selected['os'] == 'linux':
            self.refresh_hypr()
        if self.query_one('#tabs', TabbedContent).active == 'clipboards':
            self.refresh_clipboards()
        if self.query_one('#tabs', TabbedContent).active == 'processes':
            self.refresh_processes()

    def action_processes(self):
        tabs = self.query_one('#tabs', TabbedContent)
        if tabs.active == 'processes':
            self.refresh_processes()
        else:
            tabs.active = 'processes'
        self.query_one('#proc-table', DataTable).focus()

    def action_process_sort(self):
        if self.query_one('#tabs', TabbedContent).active != 'processes':
            return
        keys = [column[0] for column in PROCESS_COLUMNS]
        key = keys[(keys.index(self.process_sort[0]) + 1) % len(keys)]
        self.process_sort = (key, next(column[3] for column in PROCESS_COLUMNS if column[0] == key))
        self.render_processes()

    def process_tick(self):
        if self.query_one('#tabs', TabbedContent).active == 'processes' and not self.process_busy:
            self.refresh_processes()

    @work(exclusive=True, group='processes')
    async def refresh_processes(self):
        node = self.selected
        self.process_generation += 1
        generation = self.process_generation
        self.process_busy = True
        if self.process_data is None and self.process_error is None:
            self.render_processes(f"Reading processes on {node['hostname']}… (takes {'about 5 s' if node['os'] != 'linux' else 'a second'})")
        try:
            data = await self.backend.call(node, 'processes')
            if self.selected == node:
                self.process_data, self.process_node, self.process_error = data, node, None
                self.process_updated = datetime.datetime.now().strftime('%H:%M:%S')
                self.render_processes()
        except Exception as error:
            if self.selected == node:
                text = str(error) or type(error).__name__
                if self.process_error != text:
                    self.log_event(f"{node['hostname']}: can't read processes: {text}")
                self.process_error = text
                if self.process_data is not None and self.process_node == node:
                    self.render_processes(f"Refresh failed ({text}) · showing the read from {self.process_updated} · retrying")
                else:
                    self.process_data, self.process_node = None, None
                    self.render_processes(f"Can't read processes on {node['hostname']}: {text} · retrying every {PROCESS_INTERVAL} s")
        finally:
            if generation == self.process_generation:
                self.process_busy = False

    def selected_process(self):
        table = self.query_one('#proc-table', DataTable)
        if not self.process_data or not table.row_count:
            return None
        key = table.coordinate_to_cell_key(table.cursor_coordinate).row_key.value
        return next((p for p in self.process_data['processes'] if str(p['pid']) == key), None)

    def render_processes(self, message=None):
        table = self.query_one('#proc-table', DataTable)
        data = self.process_data
        windows = bool(data and data.get('platform') == 'windows')
        sort, descending = self.process_sort
        for key, label, _, _ in PROCESS_COLUMNS:
            if key == 'disk' and windows:
                label = 'I/O'
            elif key == 'net' and windows:
                label = 'Net'
            arrow = (' ▼' if descending else ' ▲') if key == sort else ''
            table.columns[key].label = Text(label + arrow, style='bold #79e2c2' if key == sort else '')
        previous = None
        if table.row_count:
            previous = table.coordinate_to_cell_key(table.cursor_coordinate).row_key.value
        scroll = table.scroll_y
        table.clear()
        if data is None:
            for key in ('cpu', 'gpu', 'memory', 'disk', 'net'):
                self.query_one('#proc-' + key, Static).update('—\n')
            if message is not None:
                self.query_one('#proc-status', Static).update(message)
            self.query_one('#proc-detail', Static).update('')
            self.query_one('#proc-note', Static).update('')
            self.query_one('#proc-end', Button).disabled = True
            return
        totals = data['totals']
        def box(key, first, second=''):
            text = Text.from_markup(first + '\n' + second, overflow='ellipsis')
            text.no_wrap = True
            self.query_one('#proc-' + key, Static).update(text)
        short = lambda value: f'{value / 1024**3:.1f}G' if value < 10 * 1024**3 else f'{value / 1024**3:.0f}G'
        box('cpu', f"{totals['cpu']:.0f}%", f"[#79e2c2]{meter(totals['cpu'], 8)}[/]")
        if totals.get('gpu') is None:
            box('gpu', 'Not available')
        else:
            box('gpu', f"{totals['gpu']:.0f}% busy", f"VRAM {short(totals['vram_used'])}/{short(totals['vram_total'])}" if totals.get('vram_total') else '')
        used = 100 * totals['memory_used'] / max(1, totals['memory_total'])
        box('memory', f"{short(totals['memory_used'])}/{short(totals['memory_total'])}", f"[#79e2c2]{meter(used, 8)}[/]")
        box('disk', f"R {speed(totals['disk_read'])}", f"W {speed(totals['disk_write'])}")
        box('net', f"↓ {speed(totals['net_rx'])}", f"↑ {speed(totals['net_tx'])}")

        text = self.query_one('#proc-filter', Input).value
        rows = process_rows(data['processes'], text, self.query_one('#proc-mine', Checkbox).value, sort, descending)
        dim = '#596a85'
        for p in rows[:PROCESS_LIMIT]:
            cells = [Text(str(p['pid']), justify='right', style=dim), Text(p.get('name') or '?'), Text(p.get('user') or '—', style='' if p.get('user') else dim)]
            for key, formatter in (('cpu', percent_text), ('gpu', percent_text), ('memory', size), ('disk', speed), ('net', speed)):
                value = process_value(p, key)
                cells.append(Text(formatter(value), justify='right', style=dim if not value else ''))
            table.add_row(*cells, key=str(p['pid']))
        names = {'pid': 'PID', 'name': 'name', 'user': 'user', 'cpu': 'CPU', 'gpu': 'GPU', 'memory': 'memory', 'disk': 'disk', 'net': 'network'}
        order = ('highest first' if descending else 'lowest first') if next(c[3] for c in PROCESS_COLUMNS if c[0] == sort) else ('Z–A' if descending else 'A–Z')
        shown = f'Showing {min(len(rows), PROCESS_LIMIT)} of {len(rows)}' + (f" matching (of {len(data['processes'])})" if len(rows) != len(data['processes']) else '')
        status = message or f"{shown} · sorted by {names[sort]}, {order} · updated {self.process_updated} · sort: click a title or press O"
        if not rows:
            status = 'No processes match this filter.' if text.strip() or self.query_one('#proc-mine', Checkbox).value else 'No processes reported.'
        self.query_one('#proc-status', Static).update(status)
        if previous is not None and previous in table.rows:
            table.move_cursor(row=table.get_row_index(previous), scroll=False)
            table.call_after_refresh(table.scroll_to, y=scroll, animate=False)
        self.show_process_detail()

    def show_process_detail(self):
        process = self.selected_process()
        data = self.process_data or {}
        windows = data.get('platform') == 'windows'
        if windows:
            note = 'I/O = all reads and writes, network included (Windows has no per-process network counter) · GPU = busiest engine · CPU = share of the whole machine'
        else:
            note = 'Net TCP = TCP only, no UDP/QUIC (NETWORK above counts all traffic) · CPU = share of the whole machine · — = needs root'
        end = self.query_one('#proc-end', Button)
        end.disabled = not (process and process.get('own') and not windows)
        self.query_one('#proc-note', Static).update(note)
        if not process:
            self.query_one('#proc-detail', Static).update('')
            return
        threads = process.get('threads', 0)
        lines = [f"{process.get('name')} · PID {process['pid']} · {process.get('user') or 'user unknown'} · {threads} thread{'' if threads == 1 else 's'}"]
        if process.get('command'):
            lines.append(process['command'][:240])
        io = 'I/O' if windows else 'Disk'
        lines.append(f"{io} read {speed(process.get('disk_read'))} · write {speed(process.get('disk_write'))}"
                     + ('' if windows else f"   ·   TCP ↓ {speed(process.get('net_rx'))} ↑ {speed(process.get('net_tx'))}")
                     + f"   ·   VRAM {size(process.get('vram'))}")
        self.query_one('#proc-detail', Static).update(Text('\n'.join(lines), no_wrap=True, overflow='ellipsis'))

    @on(DataTable.RowHighlighted, '#proc-table')
    def process_highlighted(self, event):
        self.show_process_detail()

    @on(DataTable.HeaderSelected, '#proc-table')
    def process_header(self, event):
        key = event.column_key.value
        numeric = next(column[3] for column in PROCESS_COLUMNS if column[0] == key)
        self.process_sort = (key, not self.process_sort[1]) if self.process_sort[0] == key else (key, numeric)
        self.render_processes()

    @on(Input.Changed, '#proc-filter')
    @on(Checkbox.Changed, '#proc-mine')
    def process_filter(self, event):
        if self.process_data is not None:
            self.render_processes()

    @work(group='commands')
    async def end_process(self, node, process):
        try:
            result = await self.backend.call(node, 'process-term', {'pid': process['pid'], 'start': process['start']})
            message = result.get('text', 'Done.')
            self.log_event(f"{node['hostname']}: {message}")
            self.notify(message)
        except Exception as error:
            self.log_event(f"{node['hostname']}: could not end process: {error}")
            self.notify(str(error), severity='error')
        if self.selected == node:
            self.refresh_processes()

    def action_clipboards(self):
        tabs = self.query_one('#tabs', TabbedContent)
        if tabs.active == 'clipboards':
            self.refresh_clipboards()
        else:
            tabs.active = 'clipboards'

    @on(TabbedContent.TabActivated)
    def tab_changed(self, event):
        if event.pane.id == 'hyprland' and self.selected['os'] == 'linux':
            self.refresh_hypr()
        if event.pane.id == 'processes':
            self.refresh_processes()
        if event.pane.id == 'clipboards':
            self.refresh_clipboards()
        else:
            self.clipboard_values.clear()
            for node in NODES:
                alias = node['alias']
                self.clipboard_generations[alias] += 1
                self.query_one('#clip-text-' + alias, TextArea).load_text('')
                self.query_one('#clip-copy-' + alias, Button).disabled = True
                self.query_one('#clip-view-' + alias, Button).disabled = True

    def show_clipboard(self, node, value):
        alias = node['alias']
        self.clipboard_values[alias] = value
        kind = value.get('kind')
        if kind == 'text':
            text = value['text'][:20000]
            description = f"{len(value['text']):,} characters"
            if len(value['text']) > 20000:
                description += ' · Preview shows the first 20,000; Copy here copies the full text.'
        elif kind == 'image':
            text = f"PNG image · {value.get('width', '?')} × {value.get('height', '?')} pixels\n\nUse Copy here to paste this image into another app, or View image to open it."
            description = f"PNG · {value.get('bytes', 0):,} bytes"
        elif kind == 'empty':
            text, description = '', 'Clipboard is empty.'
        else:
            text = 'This clipboard offers: ' + ', '.join(value.get('types', []))
            description = 'Copying supports plain text and PNG.'
        self.query_one('#clip-text-' + alias, TextArea).load_text(text)
        timestamp = datetime.datetime.now().strftime('%H:%M:%S')
        self.query_one('#clip-state-' + alias, Static).update(description + ' · ' + timestamp)
        self.query_one('#clip-copy-' + alias, Button).disabled = kind not in ('text', 'image') or self.clipboard_busy
        self.query_one('#clip-view-' + alias, Button).disabled = kind != 'image'

    @work(group='clipboard-reads')
    async def refresh_clipboards(self, alias=None):
        async def read(node):
            key = node['alias']
            self.clipboard_generations[key] += 1
            generation = self.clipboard_generations[key]
            self.clipboard_values.pop(key, None)
            self.query_one('#clip-copy-' + key, Button).disabled = True
            self.query_one('#clip-view-' + key, Button).disabled = True
            self.query_one('#clip-text-' + key, TextArea).load_text('')
            self.query_one('#clip-state-' + key, Static).update('Reading…')
            try:
                value = await self.backend.clipboard(node)
                if generation == self.clipboard_generations[key] and self.query_one('#tabs', TabbedContent).active == 'clipboards':
                    self.show_clipboard(node, value)
            except Exception as error:
                if generation == self.clipboard_generations[key]:
                    self.query_one('#clip-state-' + key, Static).update('Unavailable: ' + str(error))
                self.log_event(node['hostname'] + ': clipboard read unavailable')
        await asyncio.gather(*(read(node) for node in NODES if alias is None or node['alias'] == alias))

    @work(group='clipboard-writes')
    async def copy_clipboard(self, alias, send_local=False):
        if self.clipboard_busy:
            return
        source = self.local_node if send_local else next(n for n in NODES if n['alias'] == alias)
        destination = next(n for n in NODES if n['alias'] == alias) if send_local else self.local_node
        self.clipboard_busy = True
        for button in self.query('.clipboard-card Button'):
            if button.id.startswith(('clip-copy-', 'clip-send-')):
                button.disabled = True
        try:
            value = await self.backend.clipboard(source) if send_local else self.clipboard_values.get(alias)
            if not value or value.get('kind') not in ('text', 'image'):
                raise ValueError('The source clipboard is empty or has an unsupported format. Refresh it first.')
            await self.backend.set_clipboard(destination, value)
            message = f"Copied {source['hostname']} clipboard to {destination['hostname']}."
            self.query_one('#clipboard-result', Static).update(message)
            self.log_event(message)
            self.notify('Clipboard copied')
            if self.query_one('#tabs', TabbedContent).active == 'clipboards':
                self.show_clipboard(destination, value)
        except Exception as error:
            self.query_one('#clipboard-result', Static).update(str(error))
            self.notify(str(error), severity='error')
            self.log_event('Clipboard copy failed')
        finally:
            self.clipboard_busy = False
            for node in NODES:
                key = node['alias']
                self.query_one('#clip-copy-' + key, Button).disabled = self.clipboard_values.get(key, {}).get('kind') not in ('text', 'image')
                self.query_one('#clip-send-' + key, Button).disabled = self.backend.is_local(node)

    def view_clipboard_image(self, alias):
        value = self.clipboard_values.get(alias, {})
        if value.get('kind') != 'image':
            return
        try:
            _, image_data = payload_bytes(value)
            self.open_local_image(image_data, '.png')
        except Exception as error:
            self.notify(str(error), severity='error')

    def on_unmount(self):
        self.clipboard_values.clear()
        for path in self.clipboard_previews:
            path.unlink(missing_ok=True)

    @work(exclusive=True, group='hypr')
    async def refresh_hypr(self):
        node = self.selected
        try:
            data = await self.backend.call(node, 'hypr')
            if self.selected != node:
                return
            self.hypr_data = data
            summary = f"Hyprland {data['version'].get('version', '')}   ·   {len(data['monitors'])} monitor(s)   ·   {len(data['clients'])} windows"
            summary += '\n' + '   '.join(f"{m['name']} {m['width']}×{m['height']} @ {m.get('refreshRate', 0):.0f} Hz" for m in data['monitors'])
            self.query_one('#hypr-summary', Static).update(summary)
            table = self.query_one('#workspaces', DataTable)
            table.clear()
            for ws in sorted(data['workspaces'], key=lambda w: (w.get('id', 100000), w.get('name', ''))):
                table.add_row(str(ws['name']), ws.get('monitor', ''), str(ws.get('windows', 0)), key=str(ws.get('address', ws['name'])))
            table = self.query_one('#windows', DataTable)
            table.clear()
            for window in data['clients']:
                table.add_row(Text(window.get('class', '')), Text(window.get('title', '')), str(window.get('workspace', {}).get('name', '')), key=window['address'])
        except Exception as error:
            if self.selected == node:
                self.query_one('#hypr-summary', Static).update(str(error))
            self.log_event(f"{node['hostname']}: {error}")

    def open_local_image(self, data, suffix):
        """Show image bytes in the desktop's image viewer; the temporary file is removed after 5 minutes."""
        runtime = Path(os.environ.get('XDG_RUNTIME_DIR', f'/run/user/{os.getuid()}'))
        with tempfile.NamedTemporaryFile(dir=runtime, prefix='archfleet-', suffix=suffix, delete=False) as image_file:
            image_file.write(data)
            path = Path(image_file.name)
        self.clipboard_previews.append(path)
        from helper import hypr_environment
        env = hypr_environment()
        env.setdefault('DBUS_SESSION_BUS_ADDRESS', f'unix:path={runtime}/bus')
        subprocess.Popen(['xdg-open', str(path)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        self.set_timer(300, lambda: path.unlink(missing_ok=True))

    @work(group='peek')
    async def peek_screen(self, node):
        self.notify(f"Capturing {node['hostname']}'s screen…")
        try:
            shot = await self.backend.call(node, 'screenshot')
            import base64
            self.open_local_image(base64.b64decode(shot['data']), '.jpg')
            self.log_event(f"{node['hostname']}: screen captured ({shot['bytes'] / 1024**2:.1f} MB), opened here.")
        except Exception as error:
            self.log_event(f"{node['hostname']}: could not capture the screen: {error}")
            self.notify(str(error), severity='error')

    def ask(self, node, key, title, placeholder, action, callback):
        """Open a Prompt that remembers what was typed last time, per machine and kind."""
        def done(value):
            if value:
                self.prompt_values[(node['alias'], key)] = value
                callback(value)
        self.push_screen(Prompt(title, placeholder, action, self.prompt_values.get((node['alias'], key), '')), done)

    def report_heading(self, title, subtitle=''):
        report = self.query_one('#report', RichLog)
        report.auto_scroll = False  # long results are read from the top
        report.clear()
        report.write(Text.assemble((title, 'bold #c7b4f8'), (('   ' + subtitle) if subtitle else '', '#91a3bf')))
        return report

    @work(exclusive=True, group='news')
    async def show_news(self, node):
        report = self.report_heading('Arch Linux news', 'reading archlinux.org…')
        try:
            items = await self.backend.arch_news()
        except Exception as error:
            report.write(Text(f"Couldn't read archlinux.org news: {error}", style='#ed8796'))
            return
        since_text = self.statuses.get(node['alias'], {}).get('last_upgrade') if node['os'] == 'linux' else None
        since = datetime.datetime.fromisoformat(since_text) if since_text else None
        report = self.report_heading('Arch Linux news', 'checked ' + datetime.datetime.now().strftime('%H:%M'))
        if node['os'] == 'linux':
            report.write(Text(f"{node['hostname']}: last full upgrade {since:%Y-%m-%d} ({ago(since_text)})." if since else f"{node['hostname']}: last upgrade date unknown, so review the recent items.", style='#a6b7d0'))
        report.write('')
        fresh = 0
        for item in items:
            when = datetime.datetime.fromisoformat(item['date'])
            new = since is not None and when > since
            fresh += new
            line = Text.assemble((f'{when:%Y-%m-%d}  ', '#91a3bf'), ('NEW  ' if new else '', 'bold #f5c26b'), (item['title'], 'bold' if new else ''))
            line.stylize(f"link {item['link']}")
            report.write(line)
            if new:
                report.write(Text('            ' + item['link'], style=f"#91a3bf link {item['link']}"))
        if fresh:
            report.write('')
            report.write(Text(f"{fresh} item{'s' if fresh > 1 else ''} since {node['hostname']}'s last upgrade. News posts often mean a manual step before or after -Syu; read them first.", style='#f5c26b'))
        elif since:
            report.write('')
            report.write(Text(f"Nothing new since {node['hostname']}'s last upgrade.", style='#79e2c2'))

    @work(exclusive=True, group='packages')
    async def compare_packages(self):
        machines = [n for n in NODES if n['os'] == 'linux'][:2]
        first, second = (n['hostname'] for n in machines)
        report = self.report_heading(f'Explicit packages  ·  {first} ↔ {second}', 'reading pacman…')
        results = await asyncio.gather(*(self.backend.packages(n) for n in machines), return_exceptions=True)
        failed = [(n, r) for n, r in zip(machines, results) if isinstance(r, BaseException)]
        report = self.report_heading(f'Explicit packages  ·  {first} ↔ {second}', 'checked ' + datetime.datetime.now().strftime('%H:%M'))
        if failed:
            for node, error in failed:
                report.write(Text(f"Can't compare: {node['hostname']} didn't answer ({error}). Wake it and press Compare packages again.", style='#ed8796'))
            return
        a, b = (set(r['packages']) for r in results)
        only_a, only_b = sorted(a - b), sorted(b - a)
        report.write(Text(f'{len(a)} on {first}  ·  {len(b)} on {second}  ·  {len(a & b)} on both', style='#a6b7d0'))
        report.write('')
        if not only_a and not only_b:
            report.write(Text('Both machines have the same explicitly installed packages.', style='#79e2c2'))
            return
        for name, only in ((first, only_a), (second, only_b)):
            report.write(Text(f'Only on {name} ({len(only)})', style='bold #79e2c2'))
            report.write(Text('   '.join(only) if only else 'nothing', style='' if only else '#596a85'))
            report.write('')

    # Run on fleet ---------------------------------------------------------------------------

    def run_targets(self):
        return [n for n in NODES if self.query_one('#run-target-' + n['alias'], Checkbox).value]

    def render_run_button(self):
        count = len(self.run_targets())
        button = self.query_one('#run-go', Button)
        button.label = 'Running…' if self.run_running else f'Run on {count}'
        button.disabled = self.run_running or not count

    @on(Checkbox.Changed, '#run-targets Checkbox')
    def run_target_changed(self, event):
        self.render_run_button()

    @on(Input.Submitted, '#run-command')
    def run_submitted(self, event):
        self.start_run()

    def start_run(self):
        field = self.query_one('#run-command', HistoryInput)
        command, targets = field.value.strip(), self.run_targets()
        if self.run_running:
            return
        if not command:
            self.notify('Type a command first.', severity='warning')
            return
        if not targets:
            self.notify('Tick at least one machine.', severity='warning')
            return
        field.remember(command)
        self.run_fleet(command, targets)

    @work(exclusive=True, group='run')
    async def run_fleet(self, command, targets):
        self.run_running = True
        self.render_run_button()
        output = self.query_one('#run-output', RichLog)
        output.clear()
        output.write(Text.assemble(('$ ', '#91a3bf'), (command, 'bold')))
        states = {n['alias']: Text('… running', style='#c7b4f8') for n in targets}

        def show_states():
            parts = [Text(n['hostname'] + ' ', style='bold') + states[n['alias']] for n in targets]
            self.query_one('#run-status', Static).update(Text('    ').join(parts))
        show_states()

        async def one(node):
            return node, await self.backend.run_command(node, command, timeout=RUN_TIMEOUT)

        failures = 0
        for finished in asyncio.as_completed([one(n) for n in targets]):
            node, result = await finished
            code, seconds = result['code'], result['seconds']
            if code == 0:
                label, color = f'✓ {seconds:.1f} s', '#79e2c2'
            elif code is None:
                label, color = f'✗ timed out after {RUN_TIMEOUT} s', '#ed8796'
            elif code == 255 and node['alias'] != self.local_node['alias']:
                label, color = '✗ connection failed', '#ed8796'
            else:
                label, color = f'✗ exit {code} · {seconds:.1f} s', '#ed8796'
            failures += code != 0
            states[node['alias']] = Text(label, style=color)
            show_states()
            output.write('')
            output.write(Text.assemble(('━━ ', color), (node['hostname'], f'bold {color}'), (f' · {label}', color)))
            output.write(Text(result['output'] or '(no output)', style='' if result['output'] else '#596a85'))
        self.log_event(f"Ran `{command[:60]}` on {len(targets)} machine{'s' if len(targets) > 1 else ''}: " + ('all succeeded' if not failures else f'{failures} failed'))
        self.run_running = False
        self.render_run_button()

    @work(group='commands')
    async def execute(self, node, action, value=None):
        try:
            result = await self.backend.call(node, action, value)
            text = result.get('text', '')
            if self.selected == node and action in ('updates', 'services', 'journal', 'storage', 'sessions'):
                report = self.query_one('#report', RichLog)
                report.clear()
                report.write(Text(f"{node['hostname']} / {action}\n\n{text}"))
            self.log_event(f"{node['hostname']} / {action}: {'done' if not result.get('code') else 'failed'}")
            if result.get('code'):
                self.notify(text[:250], severity='error')
            elif action in ('lock', 'notify', 'open-url'):
                self.notify(f"{node['hostname']}: {text}")
            if action in ('workspace', 'focus', 'reload') and self.selected == node:
                self.refresh_hypr()
        except Exception as error:
            self.log_event(f"{node['hostname']} / {action}: {error}")
            self.notify(str(error), severity='error')
            if self.selected == node:
                self.query_one('#report', RichLog).write(Text(str(error)))

    def terminal(self, argv):
        try:
            with self.suspend():
                subprocess.run(argv, check=False)
        except Exception as error:
            self.notify(str(error), severity='error')
        self.refresh_status()

    def action_shell(self):
        self.terminal(['ssh', self.selected['alias']])

    def action_sftp(self):
        self.terminal(['sftp', self.selected['alias']])

    def upgrade(self, confirmed, node):
        if confirmed:
            self.terminal(['ssh', '-t', node['alias'], 'sudo pacman -Syu'])

    @work(group='transfer')
    async def do_transfer(self, node, local, remote, download):
        self.transfer_running = True
        self.query_one('#upload', Button).disabled = True
        self.query_one('#download', Button).disabled = True
        self.query_one('#transfer-status', Static).update('Transfer in progress…')
        try:
            message = await self.backend.transfer(node, local, remote, download)
            self.query_one('#transfer-status', Static).update(message)
            self.log_event(message)
            self.notify('Transfer complete')
        except Exception as error:
            self.query_one('#transfer-status', Static).update(str(error))
            self.log_event(f"Transfer: {error}")
            self.notify(str(error), severity='error')
        finally:
            self.transfer_running = False
            self.query_one('#upload', Button).disabled = False
            self.query_one('#download', Button).disabled = False

    @on(Button.Pressed)
    def pressed(self, event):
        ident = event.button.id
        node = self.selected
        if ident.startswith('select-'):
            self.action_select_node(ident.removeprefix('select-'))
            if self.query_one('#tabs', TabbedContent).active == 'clipboards':
                self.query_one('#clip-card-' + self.selected['alias']).scroll_visible(animate=False)
        elif ident == 'proc-end':
            process = self.selected_process()
            if process and process.get('own'):
                self.push_screen(Confirm(f"End {process.get('name')} (PID {process['pid']}) on {node['hostname']}?\n\nThis sends SIGTERM, asking it to close. Unsaved work in it may be lost."),
                                 lambda yes: self.end_process(node, process) if yes else None)
        elif ident == 'clipboard-refresh-all':
            self.refresh_clipboards()
        elif ident.startswith('clip-refresh-'):
            self.refresh_clipboards(ident.removeprefix('clip-refresh-'))
        elif ident.startswith('clip-copy-'):
            self.copy_clipboard(ident.removeprefix('clip-copy-'))
        elif ident.startswith('clip-send-'):
            self.copy_clipboard(ident.removeprefix('clip-send-'), send_local=True)
        elif ident.startswith('clip-view-'):
            self.view_clipboard_image(ident.removeprefix('clip-view-'))
        elif ident == 'shell':
            self.action_shell()
        elif ident in ('sftp', 'file-sftp'):
            self.action_sftp()
        elif ident == 'tmux':
            self.terminal(['ssh', '-t', node['alias'], 'if command -v tmux >/dev/null; then tmux new-session -A -s archfleet; else printf "tmux is not installed on this machine.\\n"; fi'])
        elif ident == 'hypr-refresh':
            self.refresh_hypr()
        elif ident == 'hypr-reload':
            self.push_screen(Confirm(f"Reload the Hyprland configuration on {node['hostname']}?", 'Reload config'), lambda yes: self.execute(node, 'reload') if yes else None)
        elif ident == 'wake':
            self.action_wake()
        elif ident == 'speed':
            self.run_speed_test(node)
        elif ident == 'peek':
            self.peek_screen(node)
        elif ident == 'lock':
            self.push_screen(Confirm(f"Lock {node['hostname']}'s screen?\n\nhyprlock will ask for your password there to unlock it.", 'Lock screen'), lambda yes: self.execute(node, 'lock') if yes else None)
        elif ident == 'notify':
            self.ask(node, 'notify', f"Show a notification on {node['hostname']}", 'Message, up to 500 characters', 'Send',
                     lambda text: self.execute(node, 'notify', {'text': text, 'from': self.local_node['hostname']}))
        elif ident == 'open-url':
            self.ask(node, 'url', f"Open a link in {node['hostname']}'s browser", 'https://…', 'Open', lambda url: self.execute(node, 'open-url', url))
        elif ident in ('vpn-connect', 'vpn-reconnect', 'vpn-test'):
            self.vpn_do(node, ident)
        elif ident == 'vpn-disconnect':
            lock = ((self.statuses.get(node['alias'], {}).get('vpn') or {}).get('lockdown'))
            self.push_screen(Confirm(f"Disconnect {node['hostname']}'s VPN?\n\n" + ('Lockdown is on there: it loses internet until you reconnect (the LAN keeps working).' if lock else 'Its traffic goes out directly until you reconnect.'), 'Disconnect'),
                             lambda yes: self.vpn_do(node, 'vpn-disconnect') if yes else None)
        elif ident == 'vpn-relay':
            self.ask(node, 'vpn-relay', f"Switch {node['hostname']}'s VPN relay", 'country [city] or hostname, e.g. se sto · de fra · se-sto-wg-001', 'Switch',
                     lambda where: self.vpn_do(node, 'vpn-relay', where))
        elif ident == 'vpn-restart':
            self.push_screen(Confirm(f"Restart the VPN tunnel on {node['hostname']}?\n\nWhat uses it pauses for a few seconds; the kill switch keeps it from leaking meanwhile.", 'Restart tunnel'),
                             lambda yes: self.vpn_do(node, 'vpn-restart') if yes else None)
        elif ident == 'run-go':
            self.start_run()
        elif ident == 'news':
            self.show_news(node)
        elif ident == 'packages':
            self.compare_packages()
        elif ident == 'workspace-go':
            self.execute(node, 'workspace', self.query_one('#workspace', Input).value)
        elif ident == 'window-focus':
            table = self.query_one('#windows', DataTable)
            if table.row_count:
                key = table.coordinate_to_cell_key(table.cursor_coordinate).row_key.value
                self.execute(node, 'focus', key)
        elif ident in ('updates', 'services', 'journal', 'storage', 'sessions'):
            self.query_one('#report', RichLog).clear()
            self.query_one('#report', RichLog).write(Text(f"{node['hostname']}: running {ident}…"))
            self.execute(node, ident)
        elif ident == 'upgrade':
            self.push_screen(Confirm(f"Run a full system upgrade on {node['hostname']}?\n\nThis opens sudo pacman -Syu in SSH. Review its package list before accepting. Enter any password in the terminal.\n\nTip: Arch news shows posts that may need a manual step first.", 'Upgrade'), lambda yes: self.upgrade(yes, node))
        elif ident in ('upload', 'download') and not self.transfer_running:
            local = self.query_one('#local-path', Input).value
            remote = self.query_one('#remote-path', Input).value
            download = ident == 'download'
            if not local.strip() or not remote.strip():
                self.notify('Enter both paths first.', severity='warning')
                return
            message = f"{'Download from' if download else 'Upload to'} {node['hostname']}?\n\nLocal: {local}\nRemote: {remote}\n\nAn existing destination file will be overwritten."
            self.push_screen(Confirm(message), lambda yes: self.do_transfer(node, local, remote, download) if yes else None)

    @on(DataTable.RowSelected, '#fleet')
    def fleet_selected(self, event):
        self.action_select_node(event.row_key.value)


if __name__ == '__main__':
    import sys
    if not NODES:
        from backend import CONFIG
        raise SystemExit(f"No machines configured yet. Copy hosts.example.json to {CONFIG} and edit it "
                         "(one entry per SSH alias), or run install.sh again.")
    if '--wake' in sys.argv:
        # archfleet --wake arch-desktop: send the magic packet and exit (no dashboard).
        alias = sys.argv[sys.argv.index('--wake') + 1] if len(sys.argv) > sys.argv.index('--wake') + 1 else ''
        node = next((n for n in NODES if alias in (n['alias'], n['hostname'], n['label'].lower())), None)
        if node is None:
            raise SystemExit('Usage: archfleet --wake <' + '|'.join(n['alias'] for n in NODES) + '>')
        if not node.get('mac'):
            raise SystemExit(f"{node['hostname']} has no wired MAC (\"mac\") in hosts.json, so it can't be woken over the network.")
        print(Backend().wake(node))
        raise SystemExit(0)
    Archfleet(initial_tab='clipboards' if '--clipboard' in sys.argv else 'overview').run()
