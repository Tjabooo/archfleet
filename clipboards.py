"""Clipboard payloads and the Wayland adapter. Content stays out of command lines."""
import base64
import struct
import subprocess
import tempfile

MAX_BYTES = 8 * 1024 * 1024
PNG_SIGNATURE = b'\x89PNG\r\n\x1a\n'


def from_bytes(data, mime):
    if len(data) > MAX_BYTES:
        raise ValueError('Clipboard exceeds the 8 MiB transfer limit.')
    if mime == 'image/png':
        if len(data) < 24 or not data.startswith(PNG_SIGNATURE) or data[12:16] != b'IHDR':
            raise ValueError('The clipboard image is not a valid PNG.')
        width, height = struct.unpack('>II', data[16:24])
        if not width or not height or width * height > 64_000_000:
            raise ValueError('Clipboard image dimensions exceed the supported limit.')
        return {'kind': 'image', 'mime': mime, 'data': base64.b64encode(data).decode(), 'bytes': len(data), 'width': width, 'height': height}
    if not mime.startswith('text/plain'):
        raise ValueError('Supported clipboard formats are plain text and PNG.')
    try:
        text = data.decode('utf-8')
    except UnicodeDecodeError:
        raise ValueError('Clipboard text is not valid UTF-8.') from None
    return {'kind': 'text', 'mime': 'text/plain;charset=utf-8', 'text': text, 'bytes': len(data)}


def payload_bytes(payload):
    kind = payload.get('kind')
    if kind == 'text':
        if not isinstance(payload.get('text'), str):
            raise ValueError('Clipboard text must be a string.')
        raw = payload['text'].encode('utf-8')
        return from_bytes(raw, 'text/plain'), raw
    if kind == 'image':
        if not isinstance(payload.get('data'), str) or len(payload['data']) > (MAX_BYTES + 2) // 3 * 4:
            raise ValueError('Clipboard image exceeds the 8 MiB transfer limit.')
        raw = base64.b64decode(payload['data'], validate=True)
        return from_bytes(raw, 'image/png'), raw
    if kind == 'empty':
        return {'kind': 'empty', 'mime': '', 'bytes': 0}, b''
    raise ValueError('This clipboard format cannot be copied. Use plain text or PNG.')


def read_wayland(env):
    listed = subprocess.run(['wl-paste', '--list-types'], capture_output=True, env=env, timeout=6)
    if listed.returncode:
        error = listed.stderr.decode(errors='replace').strip()
        if 'Nothing is copied' in error or 'No selection' in error:
            return {'kind': 'empty', 'mime': '', 'bytes': 0}
        raise RuntimeError(error or 'Could not read the Wayland clipboard.')
    types = listed.stdout.decode(errors='replace').splitlines()
    if not types:
        return {'kind': 'empty', 'mime': '', 'bytes': 0}
    mime = 'image/png' if 'image/png' in types else next((t for t in ('text/plain;charset=utf-8', 'text/plain', 'UTF8_STRING', 'TEXT', 'STRING') if t in types), None)
    if mime is None:
        return {'kind': 'unsupported', 'types': types, 'bytes': 0}
    result = subprocess.run(['wl-paste', '--no-newline', '--type', mime], capture_output=True, env=env, timeout=6)
    if result.returncode:
        raise RuntimeError(result.stderr.decode(errors='replace').strip() or 'The clipboard owner stopped responding.')
    if not result.stdout:  # an empty selection can still offer text types
        return {'kind': 'empty', 'mime': '', 'bytes': 0}
    return from_bytes(result.stdout, 'image/png' if mime == 'image/png' else 'text/plain')


TEXT_CAP = 64 * 1024        # full text sent along with a history entry (clipdeck caches it)
TEXT_BUDGET = 2 * 1024 * 1024  # ... for the whole list


def cliphist_history(limit=10, with_text=False):
    """The newest cliphist entries, [{'id', 'preview'}]. cliphist keeps history; Wayland only the current one.
    with_text: text entries (valid UTF-8, up to TEXT_CAP, TEXT_BUDGET in all) also carry their full 'text'."""
    result = subprocess.run(['cliphist', 'list'], capture_output=True, timeout=6)
    if result.returncode:
        raise RuntimeError(result.stderr.decode(errors='replace').strip() or 'Could not read the clipboard history.')
    entries = []
    for line in result.stdout.decode('utf-8', errors='replace').splitlines():
        entry_id, _, preview = line.partition('\t')
        if entry_id.isdigit():
            entries.append({'id': int(entry_id), 'preview': preview})
            if len(entries) >= limit:
                break
    if with_text:
        budget = TEXT_BUDGET
        for entry in entries:
            if entry['preview'].startswith('[[ binary data') or budget <= 0:
                continue
            r = subprocess.run(['cliphist', 'decode', str(entry['id'])], capture_output=True, timeout=6)
            if r.returncode or len(r.stdout) > min(TEXT_CAP, budget) or r.stdout.startswith(PNG_SIGNATURE):
                continue
            try:
                entry['text'] = r.stdout.decode('utf-8')
            except UnicodeDecodeError:
                continue
            budget -= len(r.stdout)
    return entries


def cliphist_entry(entry_id):
    """One history entry's raw bytes (base64), exactly as `cliphist decode` gives them to wl-copy."""
    if type(entry_id) is not int or entry_id < 0:
        raise ValueError('Invalid clipboard history entry.')
    result = subprocess.run(['cliphist', 'decode', str(entry_id)], capture_output=True, timeout=6)
    if result.returncode:
        raise RuntimeError(result.stderr.decode(errors='replace').strip() or 'That history entry is gone.')
    if len(result.stdout) > MAX_BYTES:
        raise ValueError('Clipboard exceeds the 8 MiB transfer limit.')
    return {'data': base64.b64encode(result.stdout).decode(), 'bytes': len(result.stdout)}


def write_wayland(payload, env):
    normalized, data = payload_bytes(payload)
    argv = ['wl-copy', '--clear'] if normalized['kind'] == 'empty' else ['wl-copy', '--type', normalized['mime']]
    # wl-copy leaves a child behind to serve the selection, and it inherits stderr: a pipe there would
    # never reach EOF, so errors go to a file the parent's exit doesn't wait on.
    with tempfile.TemporaryFile() as stderr:
        result = subprocess.run(argv, input=data, stdout=subprocess.DEVNULL, stderr=stderr, env=env, timeout=6)
        if result.returncode:
            stderr.seek(0)
            raise RuntimeError(stderr.read().decode(errors='replace').strip() or 'Could not set the Wayland clipboard.')
    return {'written': True, 'kind': normalized['kind'], 'bytes': len(data)}
