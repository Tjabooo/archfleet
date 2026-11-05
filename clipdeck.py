"""clipdeck: the SUPER+V clipboard picker, for every machine on the home network.

  clipdeck show        open the overlay on the focused monitor and fill it in
  clipdeck pick KEY    copy that entry (on the page shown) to this machine's clipboard, then close
  clipdeck page DIR    flip every column a page: next/prev (or down/up, eww's scroll), first, last
  clipdeck hide        close it
  clipdeck stream      JSON for the eww overlay (eww runs this itself)
  clipdeck forget      delete the cached copies of the other machines' history

One column per machine: this one's cliphist history first, then the other Linux machines' history
and Windows clipboard history (with its current clipboard as a fallback). Other machines are reached
over their SSH aliases through Archfleet's helper. Every entry has a key chip like the
SUPER+/ sheet; the "clip" submap in hyprland.lua binds those keys to `clipdeck pick`. Each column
holds up to CLIPDECK_HISTORY entries (default 100; Windows keeps 25), PER_COLUMN to a page: arrows,
Page Up/Down or the scroll wheel flip all columns together and the same keys pick from the new page.
The overlay is an eww window called `clipdeck`: either in your own eww config (any
~/.config/*/eww or ~/.config/eww that defines it, so it wears your palette) or the one bundled in
clipdeck/eww. Set CLIPDECK_EWW to pick a config folder yourself. What's on screen is kept in $XDG_RUNTIME_DIR (RAM, this user only)
and deleted on close; clipboard contents never go on a command line.
Each other machine's last answer is cached in ~/.cache/clipdeck/<alias>.json (this user only; text
entries up to 64 KiB keep their full text). SUPER+V shows that copy at once while the machine is asked
again, and keeps it, marked offline, when the machine doesn't answer: text still copies, images
need the machine on. CLIPDECK_CACHE=0 turns the cache off; `clipdeck forget` empties it.
"""
import base64
import fcntl
import json
import os
import re
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))
from backend import Backend, NODES  # noqa: E402
from clipboards import PNG_SIGNATURE, cliphist_history, payload_bytes  # noqa: E402

CONF = Path(os.environ.get('XDG_CONFIG_HOME', Path.home() / '.config'))
def find_eww_config():
    if os.environ.get('CLIPDECK_EWW'):
        return Path(os.environ['CLIPDECK_EWW']).expanduser()
    for folder in [CONF / 'eww', *sorted(CONF.glob('*/eww'))]:
        try:
            if 'defwindow clipdeck' in (folder / 'eww.yuck').read_text(errors='replace'):
                return folder
        except OSError:
            pass
    return ROOT / 'clipdeck' / 'eww'


EWW_CFG = find_eww_config()
RUNTIME = Path(os.environ.get('XDG_RUNTIME_DIR', f'/run/user/{os.getuid()}'))
STATE = RUNTIME / 'clipdeck.json'
KEY_ROWS = ('1234567890', 'QWERTYUIOP', 'ASDFGHJKL')  # one row of keys per column
PER_COLUMN = 10
try:
    HISTORY = max(PER_COLUMN, min(int(os.environ.get('CLIPDECK_HISTORY', 100)), 1000))
except ValueError:
    HISTORY = 100
CLOSED = {'open': False, 'page': 0, 'cols': []}
CACHE = Path(os.environ.get('XDG_CACHE_HOME', Path.home() / '.cache')) / 'clipdeck'
CACHING = os.environ.get('CLIPDECK_CACHE', '1') != '0'
TEXT_CAP = 64 * 1024
IMAGE = re.compile(r'^\[\[ binary data (.+?) (\w+) (\d+)x(\d+) \]\]$')


# ---------- what the overlay shows ----------
def one_line(text):
    # eww reads "\n" in a label as a line break, so backslashes are shown as a look-alike (labels only)
    return ' '.join(text.split()).replace('\\', '⧵')


def describe(preview):
    """cliphist's one-line preview -> (label, is_image)."""
    m = IMAGE.match(preview)
    if m:
        size, fmt, w, h = m.groups()
        return f'▣ {fmt.upper()} {w}×{h} · {size}', True
    if preview.startswith('[[ binary data'):
        return '▣ ' + preview.strip('[] '), True
    return one_line(preview) or '(blank space)', False


def node_title(node, local):
    kind = 'THIS MACHINE' if local else ('WINDOWS · HISTORY' if node['os'] != 'linux' else 'ARCH')
    return node['hostname'].upper(), kind


def column(node, index, local=False):
    name, kind = node_title(node, local)
    return {'alias': node['alias'], 'name': name, 'kind': kind, 'base_kind': kind, 'state': 'loading',
            'note': 'reaching ' + node['hostname'] + '…', 'items': [], 'keys': KEY_ROWS[index]}


def fill_history(col, entries):
    """All fetched entries go in `items`; the key chips are handed out per page (see page_of)."""
    col['items'] = []
    for entry in entries[:HISTORY]:
        label, image = ('▣ Windows image', True) if entry.get('kind') == 'image' else describe(entry['preview'])
        item = {'id': entry['id'], 'label': label, 'img': image}
        if isinstance(entry.get('text'), str):
            item['text'] = entry['text']  # full text: copies without asking the machine again
        col['items'].append(item)
    col['state'], col['note'] = ('ok', '') if col['items'] else ('empty', 'no clipboard history')


def fill_current(col, value):
    col['items'] = []
    kind = value.get('kind')
    if kind == 'text' and value.get('text'):
        label, image = one_line(value['text'][:600])[:300] or '(blank space)', False
    elif kind == 'image':
        label, image = f"▣ PNG {value.get('width', '?')}×{value.get('height', '?')} · {value.get('bytes', 0) // 1024} KiB", True
    else:
        col['state'], col['note'] = 'empty', ('clipboard is empty' if kind in ('empty', 'text') else 'only formats it can’t copy')
        return
    col['items'] = [{'label': label, 'img': image}]
    if not image and len(value['text'].encode()) <= TEXT_CAP:
        col['items'][0]['text'] = value['text']
    col['state'], col['note'] = 'ok', ''


def fetch_column(col, node, backend):
    if node['os'] == 'linux':
        fill_history(col, time_boxed(backend.call(node, 'clipboard-history', {'limit': HISTORY, 'text': True})))
        return
    try:
        history = time_boxed(backend.call(node, 'clipboard-history', HISTORY))
    except Exception:
        history = {'status': 'unavailable', 'entries': []}
    if history.get('entries'):
        fill_history(col, history['entries'])
        return
    fill_current(col, time_boxed(backend.clipboard(node)))
    col['kind'] = 'WINDOWS · CURRENT'
    reason = {'ok': 'history is empty', 'disabled': 'history is disabled'}.get(history.get('status'), 'history is unavailable')
    col['note'] = reason + (' · showing current clipboard' if col['items'] else ' · ' + col['note'])


def offline(col, error):
    text = str(error)
    asleep = 'timed out' in text.lower() or 'no route' in text.lower() or type(error).__name__ == 'TimeoutError'
    col['state'], col['note'] = 'off', ('offline · asleep?' if asleep else text[:80] or 'unreachable')
    if col.get('cached') and col['items']:
        col['note'] = ('offline' if asleep else 'unreachable') + ' · showing its history from ' + ago(col['saved'])


# ---------- the other machines' last answer, for when they're off ----------
def ago(saved, now=None):
    seconds = max(0, (now or time.time()) - saved)
    if seconds < 90:
        return 'just now'
    if seconds < 90 * 60:
        return f'{round(seconds / 60)} min ago'
    if seconds < 36 * 3600:
        return f'{round(seconds / 3600)} h ago'
    return f'{round(seconds / 86400)} d ago'


def cache_file(alias):
    return CACHE / (re.sub(r'[^A-Za-z0-9._-]', '_', alias) + '.json')


def store_cache(col):
    if not CACHING or col['state'] not in ('ok', 'empty'):
        return
    try:
        CACHE.mkdir(mode=0o700, parents=True, exist_ok=True)
        path = cache_file(col['alias'])
        tmp = path.with_suffix('.tmp')
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, 'w') as f:
            json.dump({'saved': time.time(), 'col': {k: col[k] for k in ('kind', 'state', 'note', 'items')}}, f, ensure_ascii=False)
        os.replace(tmp, path)
    except OSError:
        pass  # a cache that can't be written only means no offline copy


def prefill(col):
    """Show the machine's cached history while it's asked again."""
    if not CACHING:
        return
    try:
        cached = json.loads(cache_file(col['alias']).read_text())
        saved, data = float(cached['saved']), cached['col']
    except (OSError, ValueError, KeyError, TypeError):
        return
    if not data.get('items'):
        return
    col.update(kind=data.get('kind', col['kind']), items=data['items'], state='cached', cached=True, saved=saved,
               note='updating · showing its history from ' + ago(saved))


def cmd_forget():
    for path in CACHE.glob('*.json'):
        path.unlink(missing_ok=True)


# ---------- state shared with `pick` and the eww stream ----------
def save(state):
    tmp = STATE.with_suffix('.tmp')
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, 'w') as f:
        json.dump(state, f, ensure_ascii=False)
    os.replace(tmp, STATE)


def load():
    try:
        return json.loads(STATE.read_text())
    except (OSError, ValueError):
        return dict(CLOSED)


class changing:
    """Read-modify-write of the state file, one process at a time (show's fetches, page, pick)."""
    def __enter__(self):
        self.fd = os.open(RUNTIME / 'clipdeck.lock', os.O_WRONLY | os.O_CREAT, 0o600)
        fcntl.flock(self.fd, fcntl.LOCK_EX)
        return load()

    def __exit__(self, *exc):
        os.close(self.fd)  # closing drops the lock


def pages(state):
    return max([1] + [-(-len(c['items']) // PER_COLUMN) for c in state.get('cols', [])])


def page_of(col, page):
    """The column's entries on that page, each with its key chip."""
    shown = col['items'][page * PER_COLUMN:(page + 1) * PER_COLUMN]
    return [dict(it, key=key) for key, it in zip(col['keys'], shown)]


def for_eww(state):
    """Only what the overlay draws (no ids)."""
    total = pages(state)
    page = min(state.get('page', 0), total - 1)
    cols = []
    for c in state.get('cols', []):
        shown, count = page_of(c, page), len(c['items'])
        note = c['note']
        if not note and not shown:
            note = f'end of history · {count} entries'
        span = f'{page * PER_COLUMN + 1}–{page * PER_COLUMN + len(shown)} / {count}' if shown and total > 1 else ''
        cols.append({k: c[k] for k in ('name', 'kind', 'state')} | {
            'note': note, 'span': span, 'items': [{k: it[k] for k in ('key', 'label', 'img')} for it in shown]})
    return {'open': state.get('open', False), 'page': page + 1, 'pages': total, 'cols': cols}


def cmd_stream():
    last = None
    while True:
        try:
            mtime = STATE.stat().st_mtime_ns
        except OSError:
            mtime = None
        if mtime != last:
            last = mtime
            try:
                print(json.dumps(for_eww(load() if mtime else CLOSED), ensure_ascii=False), flush=True)
            except BrokenPipeError:
                return
        time.sleep(0.1 if mtime else 0.5)


# ---------- opening and closing ----------
def sh(*cmd, timeout=10):
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return r.returncode, (r.stdout + r.stderr).strip()
    except (FileNotFoundError, subprocess.TimeoutExpired) as e:
        return 1, str(e)


def eww(*args):
    return sh('eww', '-c', str(EWW_CFG), *args)


def submap(name):
    # Hyprland's Lua config takes a Lua expression; the classic hyprlang config a plain dispatcher.
    rc, out = sh('hyprctl', 'dispatch', f'hl.dsp.submap("{name}")')
    if rc or out.strip().lower() not in ('', 'ok'):
        sh('hyprctl', 'dispatch', 'submap', name)


def notify(title, body=''):
    sh('notify-send', '-a', 'clipdeck', '-t', '2500', title, body)


def focused_monitor():
    rc, out = sh('hyprctl', 'monitors', '-j')
    try:
        m = next((m for m in json.loads(out) if m.get('focused')), None)
    except json.JSONDecodeError:
        return []
    return [x for x in (m.get('model'), m.get('name')) if x] if m else []


def cmd_show():
    backend = Backend()
    local = next((n for n in NODES if backend.is_local(n)), None)
    if local is None:
        notify('clipdeck', 'This machine isn’t in ~/.config/archfleet/hosts.json (match "hostname" to its hostname).')
        return
    nodes = [local] + [n for n in NODES if n is not local]
    state = {'open': True, 'page': 0, 'cols': [column(n, i, n is local) for i, n in enumerate(nodes[:len(KEY_ROWS)])]}
    for col in state['cols'][1:]:
        prefill(col)
    try:
        fill_history(state['cols'][0], cliphist_history(HISTORY))
    except Exception as error:
        offline(state['cols'][0], error)
    save(state)

    if eww('ping')[1] != 'pong':
        subprocess.Popen(['eww', '-c', str(EWW_CFG), 'daemon'], stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL, start_new_session=True)
        for _ in range(40):
            time.sleep(0.1)
            if eww('ping')[1] == 'pong':
                break
    for mon in [*focused_monitor(), None]:
        if eww('open', 'clipdeck', *(['--screen', mon] if mon else []))[0] == 0:
            # only now enter the "clip" submap: every other bind is off inside it
            submap('clip')
            break
    else:
        STATE.unlink(missing_ok=True)
        notify('Clipboard overlay didn’t open', f'eww couldn’t open it; try: eww -c {EWW_CFG} logs')
        return

    lock = threading.Lock()

    def fetch(col, node, index):
        fetched = dict(col)
        try:
            fresh = dict(col, kind=col['base_kind'], cached=False)
            fetch_column(fresh, node, backend)
            fetched = fresh
            store_cache(fetched)
        except Exception as error:
            offline(fetched, error)  # keeps the cached copy, if there is one
        with lock, changing() as current:
            # merge into what's on disk now: the page may have been flipped meanwhile
            if current.get('open') and len(current.get('cols', [])) > index:  # closed: don't bring it back
                current['cols'][index] = fetched
                save(current)

    threads = [threading.Thread(target=fetch, args=(col, node, i))
               for i, (col, node) in enumerate(zip(state['cols'][1:], nodes[1:]), 1)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()


def time_boxed(coro, seconds=8):
    import asyncio
    return asyncio.run(asyncio.wait_for(coro, seconds))


def cmd_hide():
    STATE.unlink(missing_ok=True)
    eww('close', 'clipdeck')
    submap('reset')


def fetch_bytes(alias, item):
    """The entry's content and, when known, its type."""
    node = next(n for n in NODES if n['alias'] == alias)
    backend = Backend()
    if node['os'] != 'linux':
        value = (backend.call(node, 'clipboard-entry', item['id']) if 'id' in item else backend.clipboard(node))
        normalized, raw = payload_bytes(time_boxed(value, 15))
        return raw, normalized.get('mime') or None
    if backend.is_local(node):
        r = subprocess.run(['cliphist', 'decode', str(item['id'])], capture_output=True, timeout=6)
        if r.returncode:
            raise RuntimeError(r.stderr.decode(errors='replace').strip() or 'That entry is gone.')
        raw = r.stdout
    else:
        raw = base64.b64decode(time_boxed(backend.call(node, 'clipboard-entry', item['id']), 15)['data'])
    return raw, ('image/png' if raw.startswith(PNG_SIGNATURE) else None)  # else wl-copy works it out


def cmd_page(direction):
    with changing() as state:
        if not state.get('open'):
            return
        last, page = pages(state) - 1, min(state.get('page', 0), pages(state) - 1)
        page = {'next': page + 1, 'down': page + 1, 'prev': page - 1, 'up': page - 1,
                'first': 0, 'last': last}.get(direction, page)
        state['page'] = max(0, min(page, last))
        save(state)


def cmd_pick(key):
    state = load()
    page = min(state.get('page', 0), pages(state) - 1)
    found = next(((c, it) for c in state.get('cols', []) for it in page_of(c, page) if it['key'] == key.upper()), None)
    if not found:
        return  # nothing (yet) under that key: stay open
    col, item = found
    cmd_hide()
    try:
        if isinstance(item.get('text'), str):
            data, mime = item['text'].encode('utf-8'), None  # already here: works with that machine off
        elif col.get('cached'):
            try:
                data, mime = fetch_bytes(col['alias'], item)
            except Exception:
                raise RuntimeError(f"{col['name'].lower()} is off: only text entries copy from its saved history.")
        else:
            data, mime = fetch_bytes(col['alias'], item)
        # stderr to DEVNULL: wl-copy's background child would hold a pipe open forever
        subprocess.run(['wl-copy', *(['--type', mime] if mime else [])], input=data,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=6, check=True)
        if col is not state['cols'][0]:  # from another machine: say so, it can take a moment
            notify('Copied from ' + col['name'].lower(), 'Ready to paste here.')
    except Exception as error:
        notify('Couldn’t copy from ' + col['name'].lower(), str(error)[:200])


def main():
    args = sys.argv[1:]
    if not args or args[0] in ('-h', '--help', 'help'):
        print(__doc__.strip())
    elif args[0] == 'show':
        cmd_show()
    elif args[0] == 'hide':
        cmd_hide()
    elif args[0] == 'pick' and len(args) == 2:
        cmd_pick(args[1])
    elif args[0] == 'page' and len(args) == 2:
        cmd_page(args[1].lower())
    elif args[0] == 'stream':
        cmd_stream()
    elif args[0] == 'forget':
        cmd_forget()
    else:
        sys.exit('clipdeck: bad command (try `clipdeck help`)')


if __name__ == '__main__':
    main()
