import os
from pathlib import Path as _Path
os.environ['ARCHFLEET_HOSTS'] = str(_Path(__file__).resolve().parent / 'test_hosts.json')  # before backend loads it

import asyncio
import unittest
from unittest.mock import patch, AsyncMock
import json
import struct
import zlib

from rich.text import Text

from textual.widgets import Button, Checkbox, DataTable, Input, RichLog, Static, TabbedContent, TextArea

import helper
import app as app_module
from app import Archfleet, Confirm, Prompt, ago, sharing_text
from backend import Backend, NODES, lan_label, load_nodes, magic_packet, marquee_config, parse_news
from clipboards import from_bytes, payload_bytes


def test_png():
    def chunk(name, data):
        return struct.pack('>I', len(data)) + name + data + struct.pack('>I', zlib.crc32(name + data))
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 2, 1, 8, 6, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(b'\x00\xff\x00\x00\xff\x00\xff\x00\xff')) + chunk(b'IEND', b'')


class FakeBackend:
    def __init__(self):
        self.transfers = []
        self.calls = []
        self.clipboard_writes = []
        self.clipboard_failures = set()
        self.clipboards = {n['alias']: from_bytes(('Clipboard from ' + n['hostname'] + '\nåäö 🔧\n').encode(), 'text/plain') for n in NODES}

    def is_local(self, node):
        return node['alias'] == 'arch-desktop'

    async def status(self, node):
        return {'online': True, 'hostname': node['hostname'], 'cpu': 12.5, 'memory_used': 4 * 1024**3, 'memory_total': 16 * 1024**3, 'disk_used': 50 * 1024**3, 'disk_total': 250 * 1024**3, 'uptime': 7400, 'kernel': 'test-kernel', 'elapsed_ms': 5, 'battery': []}

    async def call(self, node, action='status', value=None):
        self.calls.append((node['alias'], action, value))
        if action == 'hypr':
            return {'version': {'version': '0.57.0'}, 'monitors': [{'name': 'DP-1', 'width': 1920, 'height': 1080, 'refreshRate': 144}], 'workspaces': [{'id': 1, 'name': '1', 'monitor': 'DP-1', 'windows': 1}, {'address': 'special:music', 'type': 'special', 'name': 'special:music', 'monitor': 'DP-1', 'windows': 0}], 'clients': [{'class': 'kitty', 'title': 'Terminal', 'workspace': {'name': '1'}, 'address': '0xabc'}], 'activeworkspace': {'id': 1}}
        return {'code': 0, 'text': 'test result'}

    def wake(self, node):
        self.calls.append((node['alias'], 'wake', None))
        return 'Wake-on-LAN packet sent.'

    async def speed_test(self, node, progress=None):
        progress('latency', {'bytes': 1})
        progress('upload', {'bytes': 1, 'latency_ms': 1.5})
        return {'bytes': 1, 'latency_ms': 1.5, 'handshake_ms': 90, 'upload': 50e6, 'download': 60e6}

    async def run_command(self, node, command, timeout=60):
        self.calls.append((node['alias'], 'run', command))
        return {'code': 0, 'output': f"ran {command} on {node['hostname']}", 'seconds': 0.2}

    async def packages(self, node):
        common = ['base', 'linux']
        return {'packages': common + (['steam'] if node['alias'] == 'arch-desktop' else ['tlp']), 'last_upgrade': None}

    async def arch_news(self):
        return [{'title': 'Manual step needed', 'link': 'https://archlinux.org/news/x/', 'date': '2030-01-01T00:00:00+00:00'},
                {'title': 'Old news', 'link': 'https://archlinux.org/news/y/', 'date': '2020-01-01T00:00:00+00:00'}]

    async def transfer(self, node, local, remote, download=False):
        self.transfers.append((node['alias'], local, remote, download))
        return 'Transfer complete'

    async def clipboard(self, node):
        await asyncio.sleep(0)
        if node['alias'] in self.clipboard_failures:
            raise RuntimeError('Host is offline.')
        return dict(self.clipboards[node['alias']])

    async def set_clipboard(self, node, value):
        self.clipboard_writes.append((node['alias'], dict(value)))
        self.clipboards[node['alias']] = dict(value)
        return {'written': True}


class AppTests(unittest.IsolatedAsyncioTestCase):
    async def test_offline_machine_can_be_woken_and_comes_back(self):
        backend = FakeBackend()
        online = backend.status
        async def partly_offline(node):
            return await online(node) if node['alias'] == 'arch-desktop' else {'online': False, 'error': 'No route to host'}
        backend.status = partly_offline
        app = Archfleet(backend)
        with patch.object(app_module, 'WAKE_POLL', 0):
            async with app.run_test(size=(120, 42)) as pilot:
                await app.workers.wait_for_complete()
                await pilot.press('2')
                self.assertFalse(app.query_one('#wake', Button).display)
                self.assertIn('wake it by hand', str(app.query_one('#wake-state', Static).render()))
                await pilot.press('3')
                self.assertTrue(app.query_one('#wake', Button).display)
                backend.status = online
                await pilot.press('w')
                await app.workers.wait_for_complete()
                await pilot.pause()
                self.assertIn(('home-server', 'wake', None), backend.calls)
                self.assertTrue(app.statuses['home-server']['online'])
                self.assertFalse(app.query_one('#wake', Button).display)

    async def test_overview_shows_sensors_drives_and_speed_test(self):
        backend = FakeBackend()
        base = backend.status
        async def status(node):
            data = await base(node)
            data.update(cpu_temp=61.0, gpu_temp=48.0, gpu_busy=17, net_rx=1.4e6, net_tx=1.2e5, last_upgrade='2020-01-01T10:00:00+0100')
            if node['os'] != 'linux':
                data['disks'] = [{'name': 'C:', 'used': 100 * 1024**3, 'total': 200 * 1024**3}, {'name': 'D:', 'used': 3 * 1024**4, 'total': 4 * 1024**4}]
            return data
        backend.status = status
        app = Archfleet(backend)
        async with app.run_test(size=(120, 42)) as pilot:
            await app.workers.wait_for_complete()
            self.assertIn('61°C', str(app.query_one('#cpu', Static).render()))
            self.assertIn('17% busy', str(app.query_one('#gpu', Static).render()))
            self.assertIn('Last full upgrade', str(app.query_one('#details', Static).render()))
            self.assertTrue(app.query_one('#speed', Button).disabled)  # this machine
            await pilot.press('3')
            self.assertIn('D:', str(app.query_one('#disk', Static).render()))
            self.assertFalse(app.query_one('#speed', Button).disabled)
            await pilot.click('#speed')
            await app.workers.wait_for_complete()
            await pilot.pause()
            self.assertIn('480 Mbit/s', str(app.query_one('#speed-result', Static).render()))

    async def test_sharing_panel_shows_marquee_load_on_the_server_only(self):
        backend = FakeBackend()
        now = 1_800_000_000_000
        load = {
            'at': now, 'sampled': True, 'limits': {'uploadMbps': 350, 'linkMbps': 1000, 'nvencSessions': 8},
            'cpu': 88, 'marqueeCpu': 0.4, 'net': {'up': 6.5e6, 'down': 1e5},
            'gpu': {'busy': 5, 'enc': 38, 'sessions': 1, 'temp': 40},
            'internet': {'streams': 2.5e6, 'torrents': 9e5}, 'lan': {'streams': 3e6},
            'disks': [{'drive': 'D', 'label': 'Clips', 'read': 0, 'write': 0, 'busy': 0}, {'drive': 'E', 'label': 'Media', 'read': 5.5e6, 'write': 0, 'busy': 23}],
            'streams': [{'who': 'Anna [guest]', 'device': 'iPhone · Safari', 'where': 'internet', 'fromHome': False, 'label': 'Dune: Part Two (2024)',
                         'since': now - 1800000, 'rate': 2.4e6, 'avg': 2.1e6, 'needs': 2.6e6, 'sent': 3.9e9, 'waiting': False, 'speed': 2.2e6, 'behind': True}],
            'jobs': [{'kind': 'Clip share copy', 'label': 'ace.mp4', 'encoder': 'x264', 'since': now - 75000, 'cpu': 84.5, 'mem': 1.2e9}],
            'top': [{'name': 'ffmpeg', 'cpu': 84.5}], 'warnings': ['CPU is at 88 %: clip share copies and conversions slow down.'],
            'recent': [{'type': 'job', 'kind': 'Clip share copy', 'label': 'old.mp4', 'encoder': 'x264', 'start': now - 9e5, 'end': now - 8e5, 'ok': False, 'cpu': 70}],
            'history': [{'t': now - 120000, 'inet': 1e6, 'tor': 0}, {'t': now - 60000, 'inet': 3e6, 'tor': 1e5}],
        }
        async def marquee_load():
            return load
        backend.marquee_load = marquee_load
        text = sharing_text(load, now_ms=now)
        for part in ('arrives at 18', '27 / 350 Mbit/s', 'torrents 7.2', '1 of 8 NVENC encodes', 'Media E:', 'Dune: Part Two', 'file needs 21',
                     'Clip share copy · ace.mp4', 'CPU 84.5%', 'FAILED', '⚠ CPU is at 88'):
            self.assertIn(part, Text.from_markup(text).plain)
        self.assertLess(text.index('Media E:'), text.index('Clips D:'))
        app = Archfleet(backend)
        async with app.run_test(size=(120, 50)) as pilot:
            await app.workers.wait_for_complete()
            self.assertFalse(app.query_one('#sharing-box').display)  # the desktop is selected
            await pilot.press('3')
            self.assertTrue(app.query_one('#sharing-box').display)
            self.assertIn('Anna [guest]', str(app.query_one('#sharing', Static).render()))
            self.assertIn('peak 25 Mbit/s', str(app.query_one('#upload-label', Static).render()))
            self.assertIn('5% busy', str(app.query_one('#gpu', Static).render()), 'the server GPU comes from Marquee')
            async def broken():
                raise RuntimeError('Marquee isn’t answering at http://x.')
            backend.marquee_load = broken
            app.refresh_status()
            await app.workers.wait_for_complete()
            shown = str(app.query_one('#sharing', Static).render())
            self.assertIn('Last read failed', shown)
            self.assertIn('Dune', shown, 'the last good reading stays up, marked as old')

    def test_marquee_config_prefers_the_house_agent_file(self):
        import tempfile, pathlib
        with tempfile.TemporaryDirectory() as tmp:
            agent, server = pathlib.Path(tmp, 'agent.env'), pathlib.Path(tmp, 'server.env')
            self.assertIsNone(marquee_config(agent, server))
            server.write_text('PORT=9000\nHOUSE_TOKEN="abc"\n')
            self.assertEqual(marquee_config(agent, server), ('http://127.0.0.1:9000', 'abc'))
            agent.write_text('# marquee-house\nMARQUEE_URL=http://192.168.50.67:8080/\nHOUSE_TOKEN=xyz\n')
            self.assertEqual(marquee_config(agent, server), ('http://192.168.50.67:8080', 'xyz'))

    async def test_run_on_fleet_defaults_to_arch_machines(self):
        backend = FakeBackend()
        app = Archfleet(backend)
        async with app.run_test(size=(120, 42)) as pilot:
            app.query_one('#tabs', TabbedContent).active = 'run'
            await pilot.pause()
            self.assertEqual(str(app.query_one('#run-go', Button).label), 'Run on 2')
            field = app.query_one('#run-command', Input)
            field.focus()
            await pilot.press(*'uname')
            self.assertEqual(app.selected['alias'], 'arch-desktop')
            await pilot.press('enter')
            await app.workers.wait_for_complete()
            ran = sorted(alias for alias, action, value in backend.calls if action == 'run')
            self.assertEqual(ran, ['arch-desktop', 'arch-laptop'])
            text = '\n'.join(line.text for line in app.query_one('#run-output', RichLog).lines)
            self.assertIn('ran uname on unit-01', text)
            field.value = ''
            await pilot.press('up')
            self.assertEqual(field.value, 'uname')

    async def test_maintenance_compares_packages_and_flags_news(self):
        app = Archfleet(FakeBackend())
        async with app.run_test(size=(120, 42)) as pilot:
            await app.workers.wait_for_complete()
            app.statuses['arch-desktop']['last_upgrade'] = '2025-06-01T12:00:00+0200'
            app.query_one('#tabs', TabbedContent).active = 'maintenance'
            await pilot.pause()
            await pilot.click('#packages')
            await app.workers.wait_for_complete()
            await pilot.pause()
            text = '\n'.join(line.text for line in app.query_one('#report', RichLog).lines)
            self.assertIn('Only on magi (1)', text)
            self.assertIn('tlp', text)
            await pilot.click('#news')
            await app.workers.wait_for_complete()
            await pilot.pause()
            text = '\n'.join(line.text for line in app.query_one('#report', RichLog).lines)
            self.assertIn('NEW  Manual step needed', text)
            self.assertNotIn('NEW  Old news', text)

    async def test_session_actions_confirm_and_prompt(self):
        backend = FakeBackend()
        app = Archfleet(backend)
        async with app.run_test(size=(120, 42)) as pilot:
            app.query_one('#tabs', TabbedContent).active = 'hyprland'
            await pilot.pause()
            await app.workers.wait_for_complete()
            await pilot.click('#lock')
            self.assertIsInstance(app.screen, Confirm)
            await pilot.press('escape')
            await pilot.click('#notify')
            self.assertIsInstance(app.screen, Prompt)
            await pilot.press(*'hej')
            await pilot.press('enter')
            await app.workers.wait_for_complete()
            self.assertIn(('arch-desktop', 'notify', {'text': 'hej', 'from': 'magi'}), backend.calls)
            self.assertFalse(any(action == 'lock' for _, action, _ in backend.calls))
            await pilot.press('3')
            self.assertTrue(app.query_one('#peek', Button).disabled)

    async def test_fleet_navigation_and_windows_controls(self):
        app = Archfleet(FakeBackend())
        async with app.run_test(size=(120, 42)) as pilot:
            await app.workers.wait_for_complete()
            await pilot.press('3')
            self.assertEqual(app.selected['alias'], 'home-server')
            self.assertTrue(app.query_one('#upgrade', Button).disabled)
            self.assertTrue(app.query_one('#hypr-refresh', Button).disabled)
            self.assertFalse(app.query_one('#shell', Button).disabled)
            self.assertEqual(app.query_one('#fleet', DataTable).row_count, 3)
            await pilot.press('2')
            self.assertFalse(app.query_one('#upgrade', Button).disabled)

    async def test_hypr_session_and_input_keeps_number_keys(self):
        backend = FakeBackend()
        app = Archfleet(backend)
        async with app.run_test(size=(120, 42)) as pilot:
            app.query_one('#tabs', TabbedContent).active = 'hyprland'
            await pilot.pause()
            await app.workers.wait_for_complete()
            self.assertEqual(app.query_one('#windows', DataTable).row_count, 1)
            self.assertEqual(app.query_one('#workspaces', DataTable).row_count, 2)
            field = app.query_one('#workspace', Input)
            field.focus()
            await pilot.press('2')
            self.assertEqual(field.value, '2')
            self.assertEqual(app.selected['alias'], 'arch-desktop')
            await pilot.click('#workspace-go')
            await app.workers.wait_for_complete()
            self.assertIn(('arch-desktop', 'workspace', '2'), backend.calls)

    async def test_transfer_cancel_and_confirm_capture_selected_host(self):
        backend = FakeBackend()
        app = Archfleet(backend)
        async with app.run_test(size=(120, 42)) as pilot:
            app.query_one('#tabs', TabbedContent).active = 'files'
            await pilot.pause()
            app.query_one('#local-path', Input).value = '/tmp/source.txt'
            app.query_one('#remote-path', Input).value = 'Downloads/source.txt'
            await pilot.click('#upload')
            self.assertIsInstance(app.screen, Confirm)
            await pilot.press('escape')
            self.assertEqual(backend.transfers, [])
            await pilot.click('#download')
            await pilot.click('#continue')
            await app.workers.wait_for_complete()
            self.assertEqual(backend.transfers, [('arch-desktop', '/tmp/source.txt', 'Downloads/source.txt', True)])

    async def test_upgrade_cancel_does_not_start_command(self):
        app = Archfleet(FakeBackend())
        async with app.run_test(size=(100, 35)) as pilot:
            app.query_one('#tabs', TabbedContent).active = 'maintenance'
            await pilot.pause()
            with patch.object(app, 'terminal') as terminal:
                await pilot.click('#upgrade')
                self.assertIsInstance(app.screen, Confirm)
                await pilot.press('escape')
                terminal.assert_not_called()

    async def test_compact_terminal_navigation(self):
        app = Archfleet(FakeBackend())
        async with app.run_test(size=(80, 24)) as pilot:
            await app.workers.wait_for_complete()
            await pilot.press('2')
            self.assertEqual(app.selected['alias'], 'arch-laptop')
            app.query_one('#tabs', TabbedContent).active = 'files'
            await pilot.pause()
            self.assertTrue(app.query_one('#local-path', Input).display)

    async def test_transfer_rejects_control_characters(self):
        with self.assertRaises(ValueError):
            await Backend().transfer(NODES[0], '/tmp/example\nfile', 'name')

    async def test_clipboards_show_all_hosts_and_copy_exact_snapshot(self):
        backend = FakeBackend()
        app = Archfleet(backend)
        async with app.run_test(size=(120, 42)) as pilot:
            await pilot.press('c')
            await app.workers.wait_for_complete()
            await pilot.pause()
            self.assertEqual(set(app.clipboard_values), {n['alias'] for n in NODES})
            expected = backend.clipboards['arch-desktop']['text']
            self.assertEqual(app.query_one('#clip-text-arch-desktop', TextArea).text, expected)
            backend.clipboards['arch-desktop'] = from_bytes(b'changed after preview', 'text/plain')
            with patch.object(app, 'log_event') as log:
                await pilot.click('#clip-copy-arch-desktop')
                await app.workers.wait_for_complete()
                self.assertEqual(backend.clipboard_writes[-1][1]['text'], expected)
                self.assertNotIn(expected, str(log.call_args_list))
            await pilot.click('#--content-tab-overview')
            await pilot.pause(0.2)
            self.assertEqual(app.clipboard_values, {})
            self.assertEqual(app.query_one('#clip-text-arch-desktop', TextArea).text, '')

    async def test_send_local_clipboard_and_offline_clears_stale_snapshot(self):
        backend = FakeBackend()
        app = Archfleet(backend)
        async with app.run_test(size=(120, 42)) as pilot:
            await pilot.press('c')
            await app.workers.wait_for_complete()
            app.copy_clipboard('home-server', send_local=True)
            await app.workers.wait_for_complete()
            self.assertEqual(backend.clipboard_writes[-1][0], 'home-server')
            self.assertEqual(backend.clipboard_writes[-1][1]['text'], backend.clipboards['arch-desktop']['text'])
            backend.clipboard_failures.add('arch-laptop')
            app.refresh_clipboards('arch-laptop')
            await app.workers.wait_for_complete()
            self.assertNotIn('arch-laptop', app.clipboard_values)
            self.assertEqual(app.query_one('#clip-text-arch-laptop', TextArea).text, '')
            self.assertTrue(app.query_one('#clip-copy-arch-laptop', Button).disabled)

    async def test_image_copy_and_unsupported_format(self):
        backend = FakeBackend()
        backend.clipboards['arch-desktop'] = from_bytes(test_png(), 'image/png')
        backend.clipboards['arch-laptop'] = {'kind': 'unsupported', 'types': ['application/x-custom']}
        app = Archfleet(backend)
        async with app.run_test(size=(120, 42)) as pilot:
            await pilot.press('c')
            await app.workers.wait_for_complete()
            self.assertFalse(app.query_one('#clip-view-arch-desktop', Button).disabled)
            self.assertTrue(app.query_one('#clip-copy-arch-laptop', Button).disabled)
            await pilot.click('#clip-copy-arch-desktop')
            await app.workers.wait_for_complete()
            self.assertEqual(payload_bytes(backend.clipboard_writes[-1][1])[1], test_png())

    async def test_clipboard_payload_is_sent_on_stdin(self):
        secret = 'private test contents\nsecond line\n'
        response = (0, json.dumps({'ok': True, 'data': {'written': True}}), '')
        for node in (NODES[0], NODES[2]):
            with patch('backend.run', new=AsyncMock(return_value=response)) as execute:
                await Backend().set_clipboard(node, from_bytes(secret.encode(), 'text/plain'))
                arguments = execute.call_args
                self.assertNotIn(secret, str(arguments.args[0]))
                self.assertEqual(json.loads(arguments.kwargs['input_data'])['value']['text'], secret)

    async def test_windows_history_actions_use_private_stdin_transport(self):
        for action, value, expected in (
            ('clipboard-history', None, {'status': 'ok', 'entries': []}),
            ('clipboard-entry', 'opaque-history-id', from_bytes(b'test history', 'text/plain')),
        ):
            response = (0, json.dumps({'ok': True, 'data': expected}), '')
            with patch('backend.run', new=AsyncMock(return_value=response)) as execute:
                self.assertEqual(await Backend().call(NODES[2], action, value), expected)
                arguments = execute.call_args
                self.assertNotIn('opaque-history-id', str(arguments.args[0]))
                self.assertEqual(json.loads(arguments.kwargs['input_data']), {'action': action, 'value': value})


class HelperTests(unittest.TestCase):
    def test_clipboard_png_and_unicode_roundtrip(self):
        image, data = payload_bytes(from_bytes(test_png(), 'image/png'))
        self.assertEqual((image['width'], image['height']), (2, 1))
        self.assertEqual(data, test_png())
        text = 'line 1\nåäö 🔧\n\n'
        self.assertEqual(payload_bytes(from_bytes(text.encode(), 'text/plain'))[1], text.encode())
        with self.assertRaises(ValueError):
            from_bytes(b'not an image', 'image/png')
        with patch('clipboards.MAX_BYTES', 4), self.assertRaises(ValueError):
            from_bytes(b'longer than the limit', 'text/plain')

    def test_empty_wayland_selection_reads_as_empty(self):
        import subprocess
        import clipboards
        listed = subprocess.CompletedProcess([], 0, b'text/plain;charset=utf-8\ntext/plain\n', b'')
        pasted = subprocess.CompletedProcess([], 0, b'', b'')
        with patch.object(clipboards.subprocess, 'run', side_effect=[listed, pasted]):
            self.assertEqual(clipboards.read_wayland({})['kind'], 'empty')

    def test_cliphist_history_and_entry_ids(self):
        import subprocess
        import clipboards
        listing = subprocess.CompletedProcess([], 0, '12\thello  world\n11\t[[ binary data 1 MiB png 2562x1440 ]]\nnoise\n'.encode(), b'')
        with patch.object(clipboards.subprocess, 'run', return_value=listing):
            self.assertEqual(clipboards.cliphist_history(1), [{'id': 12, 'preview': 'hello  world'}])
            self.assertEqual([e['id'] for e in clipboards.cliphist_history()], [12, 11])
        with patch.object(clipboards.subprocess, 'run') as run:
            for bad in ('12; rm -rf ~', -1, True, None):
                with self.assertRaises(ValueError):
                    clipboards.cliphist_entry(bad)
            run.assert_not_called()

    def test_clipdeck_columns(self):
        import clipdeck
        self.assertEqual(clipdeck.describe('[[ binary data 1 MiB png 2562x1440 ]]'), ('▣ PNG 2562×1440 · 1 MiB', True))
        self.assertEqual(clipdeck.describe('two\tlines  here '), ('two lines here', False))
        self.assertNotIn('\\', clipdeck.describe("printf '%s\\n' x")[0])
        col = clipdeck.column(NODES[1], 1)
        clipdeck.fill_history(col, [{'id': i, 'preview': f'entry {i}'} for i in range(14)])
        self.assertEqual([it['key'] for it in col['items']], list('QWERTYUIOP'))
        server = clipdeck.column(NODES[2], 2)
        clipdeck.fill_current(server, {'kind': 'empty'})
        self.assertEqual((server['state'], server['items']), ('empty', []))
        clipdeck.fill_current(server, {'kind': 'text', 'text': 'from\nwindows'})
        self.assertEqual(server['items'], [{'key': 'A', 'label': 'from windows', 'img': False}])
        self.assertNotIn('id', clipdeck.for_eww({'cols': [col]})['cols'][0]['items'][0])

    def test_windows_history_and_current_fallback(self):
        import clipdeck
        backend = FakeBackend()
        backend.call = AsyncMock(return_value={'status': 'ok', 'entries': [
            {'id': 'stable-text', 'preview': 'older\ntext', 'kind': 'text'},
            {'id': 'stable-image', 'preview': 'Windows image', 'kind': 'image'},
        ]})
        server = clipdeck.column(NODES[2], 2)
        clipdeck.fetch_column(server, NODES[2], backend)
        self.assertEqual(server['kind'], 'WINDOWS · HISTORY')
        self.assertEqual([(x['id'], x['img']) for x in server['items']], [('stable-text', False), ('stable-image', True)])
        for status in ('ok', 'disabled', 'unavailable'):
            backend.call = AsyncMock(return_value={'status': status, 'entries': []})
            server = clipdeck.column(NODES[2], 2)
            clipdeck.fetch_column(server, NODES[2], backend)
            self.assertEqual(server['kind'], 'WINDOWS · CURRENT')
            self.assertEqual(len(server['items']), 1)
            self.assertNotIn('id', server['items'][0])
            self.assertIn('showing current clipboard', server['note'])
        backend.call = AsyncMock(side_effect=RuntimeError('old helper'))
        clipdeck.fetch_column(server, NODES[2], backend)
        self.assertIn('unavailable', server['note'])
        clipdeck.fill_current(server, {'kind': 'empty'})
        self.assertEqual(server['items'], [])

    def test_windows_history_selection_reads_the_selected_id(self):
        import clipdeck
        for payload in (from_bytes('old fixture\nåäö'.encode(), 'text/plain'), from_bytes(test_png(), 'image/png')):
            backend = FakeBackend()
            backend.call = AsyncMock(return_value=payload)
            backend.clipboard = AsyncMock(side_effect=AssertionError('must not read current clipboard'))
            with patch.object(clipdeck, 'Backend', return_value=backend):
                raw, mime = clipdeck.fetch_bytes('home-server', {'id': 'selected-older-entry'})
            self.assertEqual((raw, mime), (payload_bytes(payload)[1], payload['mime']))
            backend.call.assert_awaited_once_with(NODES[2], 'clipboard-entry', 'selected-older-entry')

    def test_remote_copy_notification_does_not_expose_preview_in_process_arguments(self):
        import clipdeck
        col = clipdeck.column(NODES[2], 2)
        clipdeck.fill_history(col, [{'id': 'fixture-id', 'preview': 'private fixture preview'}])
        state = {'open': True, 'cols': [clipdeck.column(NODES[0], 0, True), col]}
        with patch.object(clipdeck, 'load', return_value=state), patch.object(clipdeck, 'cmd_hide'), \
                patch.object(clipdeck, 'fetch_bytes', return_value=(b'fixture bytes', 'text/plain')), \
                patch.object(clipdeck.subprocess, 'run'), patch.object(clipdeck, 'notify') as notify:
            clipdeck.cmd_pick('A')
        self.assertNotIn('private fixture preview', str(notify.call_args))

    def test_wake_news_upgrade_and_config_helpers(self):
        packet = magic_packet('30:c5:99:a8:69:23')
        self.assertEqual(len(packet), 102)
        self.assertEqual(packet[6:12], bytes.fromhex('30c599a86923'))
        with self.assertRaises(ValueError):
            magic_packet(None)
        news = parse_news('''<rss><channel>
            <item><title>Older</title><link>https://a/1</link><pubDate>Mon, 01 Jan 2024 10:00:00 +0000</pubDate></item>
            <item><title>Newer</title><link>https://a/2</link><pubDate>Tue, 02 Jan 2024 10:00:00 +0000</pubDate></item>
        </channel></rss>''')
        self.assertEqual([item['title'] for item in news], ['Newer', 'Older'])
        import datetime, tempfile
        from pathlib import Path
        now = datetime.datetime(2026, 11, 4, 12, tzinfo=datetime.timezone.utc)
        self.assertEqual(ago('2026-11-04T01:00:00+0000', now), 'today')
        self.assertEqual(ago('2026-11-01T01:00:00+0000', now), '3 days ago')
        self.assertEqual(ago(None), 'unknown')
        with tempfile.TemporaryDirectory() as folder:
            log = Path(folder) / 'pacman.log'
            log.write_text('[2026-10-01T10:00:00+0200] [PACMAN] starting full system upgrade\n[2026-10-20T09:00:00+0200] [PACMAN] starting full system upgrade\n[2026-10-21T09:00:00+0200] [PACMAN] Running \'pacman -S vim\'\n')
            self.assertEqual(helper.last_upgrade(log), '2026-10-20T09:00:00+0200')
            self.assertIsNone(helper.last_upgrade(Path(folder) / 'missing.log'))
            config = Path(folder) / 'hosts.json'
            self.assertEqual(load_nodes(config), [])
            config.write_text(json.dumps([{'alias': 'a', 'mac': 'aa:bb:cc:dd:ee:ff'}]))
            self.assertEqual(load_nodes(config), [{'alias': 'a', 'mac': 'aa:bb:cc:dd:ee:ff', 'label': 'a', 'hostname': 'a', 'os': 'linux', 'user': '', 'address': ''}])
            self.assertEqual(lan_label(NODES), '192.168.50.0/24')
            self.assertEqual(lan_label([{'address': '10.0.0.2'}, {'address': '10.0.1.2'}]), 'LAN')

    def test_session_inputs_are_validated_before_execution(self):
        with patch.object(helper, 'hypr_environment', return_value={'XDG_RUNTIME_DIR': '/run/user/1000'}), patch.object(helper, 'command') as execute, \
                patch.object(helper.subprocess, 'Popen') as popen:
            for action, value in [('open-url', 'file:///etc/passwd'), ('open-url', 'https://a b'), ('open-url', 'javascript:alert(1)'),
                                  ('notify', {'text': ''}), ('notify', {'text': 'x' * 501}), ('notify', {'text': 'bell\x07'})]:
                with self.assertRaises(ValueError):
                    helper.session_action({'action': action, 'value': value})
            execute.assert_not_called()
            popen.assert_not_called()
            helper.session_action({'action': 'open-url', 'value': 'https://archlinux.org/news/'})
            self.assertEqual(popen.call_args[0][0], ['xdg-open', 'https://archlinux.org/news/'])

    def test_control_inputs_are_validated_before_execution(self):
        with patch.object(helper, 'hypr_environment', return_value={}), patch.object(helper, 'command') as execute:
            for action, value in [('workspace', '0'), ('workspace', '2; os.execute("id")'), ('focus', '0xab;id')]:
                with self.assertRaises(ValueError):
                    helper.hypr_action({'action': action, 'value': value})
            execute.assert_not_called()


if __name__ == '__main__':
    unittest.main()
