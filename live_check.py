"""Read-only integration check against the real home network."""
import asyncio
import json
from backend import Backend, NODES


async def main():
    backend = Backend()
    statuses = await asyncio.gather(*(backend.status(n) for n in NODES))
    print(json.dumps({'status': dict(zip([n['alias'] for n in NODES], statuses))}, indent=2))
    if not all(item.get('online') for item in statuses):
        raise RuntimeError('A node did not respond to its status probe.')
    for node in NODES:
        if node['os'] == 'linux':
            data = await backend.call(node, 'hypr')
            print(json.dumps({'node': node['alias'], 'hyprland': data['version'].get('version'), 'monitors': len(data['monitors']), 'windows': len(data['clients'])}))
            result = await backend.call(node, 'storage')
            if result['code']:
                raise RuntimeError(result['text'])
    print('Live status, Hyprland discovery, and maintenance read checks passed.')


if __name__ == '__main__':
    asyncio.run(main())
