"""Read-only diagnostics; --roundtrip briefly tests text/PNG and restores content."""
import asyncio
import io
import json
import sys
import uuid

from backend import Backend, NODES
from clipboards import from_bytes, payload_bytes


async def main():
    backend = Backend()
    roundtrip = '--roundtrip' in sys.argv
    if roundtrip:
        from PIL import Image
    for node in NODES:
        original = await backend.clipboard(node)
        result = {'host': node['alias'], 'read': 'ok', 'kind': original.get('kind'), 'bytes': original.get('bytes', 0)}
        if roundtrip and original.get('kind') in ('text', 'image', 'empty'):
            marker = from_bytes(('Archfleet test ' + uuid.uuid4().hex + '\nåäö 🔧\n\n').encode(), 'text/plain')
            image = Image.frombytes('RGBA', (4, 1), uuid.uuid4().bytes)
            stream = io.BytesIO()
            image.save(stream, format='PNG')
            png = from_bytes(stream.getvalue(), 'image/png')
            last = None
            def equivalent(a, b):
                if a.get('kind') != b.get('kind'):
                    return False
                if a['kind'] == 'text':
                    return a.get('text') == b.get('text')
                if a['kind'] == 'image':
                    first = Image.open(io.BytesIO(payload_bytes(a)[1])).convert('RGBA')
                    second = Image.open(io.BytesIO(payload_bytes(b)[1])).convert('RGBA')
                    return first.size == second.size and first.tobytes() == second.tobytes()
                return a['kind'] == 'empty'
            try:
                for payload in (marker, png):
                    last = payload
                    await backend.set_clipboard(node, payload)
                    actual = await backend.clipboard(node)
                    last = actual  # what our write left there, so a failed comparison is still restored
                    if not equivalent(actual, payload):
                        raise AssertionError('Clipboard round-trip differs for ' + node['alias'] + ' / ' + payload['kind'])
                    result[payload['kind'] + '_roundtrip'] = 'ok'
            finally:
                current = await backend.clipboard(node)
                if last is not None and equivalent(current, last):
                    await backend.set_clipboard(node, original)
                    result['restored'] = True
                else:
                    result['restored'] = False
                    result['reason'] = 'Clipboard changed during the test; the new user content was preserved.'
        print(json.dumps(result), flush=True)


if __name__ == '__main__':
    asyncio.run(main())
