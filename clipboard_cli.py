"""SSH-backed clipboard commands for humans and coding agents."""
import argparse
import asyncio
import json
import sys

from backend import Backend, NODES
from clipboards import MAX_BYTES, from_bytes, payload_bytes


def node(alias):
    return next(item for item in NODES if item['alias'] == alias)


async def main():
    aliases = [n['alias'] for n in NODES]
    parser = argparse.ArgumentParser(description='Read or copy home-network clipboards over verified SSH. Plain text and PNG; 8 MiB limit.')
    commands = parser.add_subparsers(dest='command', required=True)
    read = commands.add_parser('get', help='Print text exactly; --json includes image data and format metadata.')
    read.add_argument('host', choices=aliases)
    read.add_argument('--json', action='store_true')
    write = commands.add_parser('set', help='Read clipboard content from stdin. Never put clipboard data on the command line.')
    write.add_argument('host', choices=aliases)
    write.add_argument('--type', choices=['text/plain', 'image/png'], default='text/plain')
    copy = commands.add_parser('copy', help='Copy the source clipboard to the destination, preserving text or PNG.')
    copy.add_argument('source', choices=aliases)
    copy.add_argument('destination', choices=aliases)
    args = parser.parse_args()
    backend = Backend()
    if args.command == 'get':
        value = await backend.clipboard(node(args.host))
        if args.json:
            print(json.dumps(value, ensure_ascii=False))
        elif value['kind'] == 'text':
            sys.stdout.write(value['text'])
        elif value['kind'] != 'empty':
            raise ValueError('This clipboard contains non-text data. Use get --json or copy to another host.')
    elif args.command == 'set':
        data = sys.stdin.buffer.read(MAX_BYTES + 1)
        await backend.set_clipboard(node(args.host), from_bytes(data, args.type))
        print('Clipboard set on ' + args.host + '.', file=sys.stderr)
    else:
        value = await backend.clipboard(node(args.source))
        if value['kind'] not in ('text', 'image'):
            raise ValueError('The source clipboard is empty or has an unsupported format.')
        await backend.set_clipboard(node(args.destination), value)
        print('Copied clipboard from ' + args.source + ' to ' + args.destination + '.', file=sys.stderr)


if __name__ == '__main__':
    if hasattr(sys.stdout, 'reconfigure'):
        sys.stdout.reconfigure(encoding='utf-8')
    try:
        asyncio.run(main())
    except Exception as error:
        print('Clipboard: ' + str(error), file=sys.stderr)
        raise SystemExit(1)
