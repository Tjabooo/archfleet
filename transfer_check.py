"""Exercise the app's actual SCP adapter with unique, temporary test files."""
import asyncio
import os
import tempfile
import uuid
from pathlib import Path
from backend import Backend, NODES, run


async def main():
    backend = Backend()
    for node in NODES:
        paths = []
        remote = 'archfleet transfer test ' + uuid.uuid4().hex + '.tmp'
        try:
            for _ in range(2):
                fd, name = tempfile.mkstemp(prefix='archfleet transfer ', suffix='.tmp')
                os.close(fd)
                paths.append(Path(name))
            payload = b'Archfleet binary transfer check\x00\x01\xff\n'
            paths[0].write_bytes(payload)
            await asyncio.wait_for(backend.transfer(node, str(paths[0]), remote), 20)
            await asyncio.wait_for(backend.transfer(node, str(paths[1]), remote, download=True), 20)
            assert paths[1].read_bytes() == payload, 'SCP round-trip contents differ'
            print(node['alias'] + ': app upload/download passed, including binary data and spaces in paths')
        finally:
            process = await asyncio.create_subprocess_exec('sftp', '-o', 'BatchMode=yes', '-b', '-', node['alias'], stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
            await asyncio.wait_for(process.communicate(('-rm "' + remote + '"\n').encode()), 10)
            for path in paths:
                path.unlink(missing_ok=True)


if __name__ == '__main__':
    asyncio.run(main())
