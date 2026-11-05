import os
from pathlib import Path as _Path
os.environ['ARCHFLEET_HOSTS'] = str(_Path(__file__).resolve().parent / 'test_hosts.json')  # before backend loads it

import asyncio
import unittest
from unittest.mock import patch, AsyncMock
import json
import struct
import tempfile
import zlib

from rich.text import Text

from textual.widgets import Button, Checkbox, DataTable, Input, RichLog, Static, TabbedContent, TextArea

import helper
import app as app_module
from app import Archfleet, Confirm, Prompt, ago, screentime_text, sharing_text, since, vpn_lines, vpn_short
from backend import Backend, NODES, lan_label, load_nodes, magic_packet, marquee_config, parse_news, windows_vpn
from clipboards import from_bytes, payload_bytes


def test_png():
    def chunk(name, data):
        return struct.pack('>I', len(data)) + name + data + struct.pack('>I', zlib.crc32(name + data))
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 2, 1, 8, 6, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(b'\x00\xff\x00\x00\xff\x00\xff\x00\xff')) + chunk(b'IEND', b'')


def screentime_sample(host='magi'):
    hourly = [{'hour': h, 'seconds': 1800 if h in (9, 10) else 0, 'categories': {'Development': 1200, 'Web': 600} if h in (9, 10) else {}} for h in range(24)]
    return {'device': host, 'date': '2026-11-04', 'total': 3600, 'hourly': hourly, 'pickups': 4, 'first_pickup': '08:55',
            'apps': [{'app': 'kitty', 'name': 'Kitty', 'category': 'Development', 'seconds': 2400},
                     {'app': 'firefox', 'name': 'Firefox', 'category': 'Web', 'seconds': 1200}],
            'categories': [{'name': 'Development', 'seconds': 2400}, {'name': 'Web', 'seconds': 1200}],
            'longest_session': {'seconds': 2700, 'start': '09:00', 'end': '09:45', 'app': 'kitty', 'name': 'Kitty'},
            'daemon': {'running': True, 'state': 'active', 'app': 'kitty'}}


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

    async def screentime(self, node):
        self.calls.append((node['alias'], 'screentime', None))
        return screentime_sample(node['hostname'])

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

    async def test_screen_time_panel_reads_each_arch_machine(self):
        backend = FakeBackend()
        app = Archfleet(backend)
        async with app.run_test(size=(120, 60)) as pilot:
            await app.workers.wait_for_complete()
            await pilot.pause()
            box = app.query_one('#screentime-box')
            text = str(app.query_one('#screentime', Static).render())
            self.assertTrue(box.display)
            self.assertIn('1h 00m today', text)
            self.assertIn('first at 08:55', text)
            self.assertIn('now active in Kitty', text)
            self.assertIn('today on ' + app.selected['hostname'], str(app.query_one('#screentime-label', Static).render()))
            await pilot.press('3')  # the Windows server: no Screen Time
            await pilot.pause()
            self.assertFalse(box.display)
            await pilot.press('2')
            await app.workers.wait_for_complete()
            await pilot.pause()
            asked = [alias for alias, action, _ in backend.calls if action == 'screentime']
            self.assertEqual(len(set(asked)), 2)
            self.assertNotIn('home-server', asked)
            self.assertTrue(box.display)

    async def test_screen_time_errors_show_in_the_panel(self):
        backend = FakeBackend()
        async def missing(node):
            raise RuntimeError(f"screentime isn’t installed on {node['hostname']}")
        backend.screentime = missing
        app = Archfleet(backend)
        async with app.run_test(size=(120, 60)) as pilot:
            await app.workers.wait_for_complete()
            await pilot.pause()
            self.assertIn('isn’t installed', str(app.query_one('#screentime', Static).render()))

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


    async def test_vpn_tab_shows_every_machine_and_runs_actions(self):
        backend = FakeBackend()
        base = backend.status
        async def status(node):
            data = await base(node)
            data['vpn'] = {
                'arch-desktop': {'kind': 'mullvad', 'state': 'connected', 'relay': 'se-sto-wg-001', 'city': 'Stockholm', 'exit_ip': '203.0.113.7', 'lockdown': False, 'last_drop': None, 'problems': [], 'ok': True},
                'arch-laptop': {'kind': 'mullvad', 'state': 'disconnected', 'relay': None, 'city': None, 'exit_ip': None, 'lockdown': True, 'last_drop': 1, 'problems': [], 'ok': True},
                'home-server': {'kind': 'tunnel', 'state': 'down', 'relay': 'se-sto-wg-002', 'city': None, 'exit_ip': None, 'lockdown': None, 'last_drop': 2, 'problems': ['tunnel has no recent handshake'], 'ok': False},
            }[node['alias']]
            return data
        backend.status = status
        app = Archfleet(backend)
        async with app.run_test(size=(130, 46)) as pilot:
            await app.workers.wait_for_complete()
            fleet = app.query_one('#fleet', DataTable)
            self.assertEqual(str(fleet.get_row('arch-desktop')[-1]), 'on · Stockholm')
            self.assertEqual(str(fleet.get_row('home-server')[-1]), 'DOWN !')
            app.query_one('#tabs', TabbedContent).active = 'vpn'
            await pilot.pause()
            table = app.query_one('#vpn-table', DataTable)
            self.assertEqual(table.row_count, 3)
            self.assertEqual(str(table.get_row('arch-laptop')[2]), 'off · lockdown')
            self.assertIn('203.0.113.7', str(app.query_one('#vpn-detail', Static).render()))
            self.assertFalse(app.query_one('#vpn-connect', Button).disabled)
            self.assertTrue(app.query_one('#vpn-restart', Button).disabled)
            await pilot.click('#vpn-relay')
            self.assertIsInstance(app.screen, Prompt)
            await pilot.press(*'se got')
            await pilot.press('enter')
            await app.workers.wait_for_complete()
            self.assertIn(('arch-desktop', 'vpn-relay', 'se got'), backend.calls)
            await pilot.click('#vpn-disconnect')
            self.assertIsInstance(app.screen, Confirm)
            await pilot.press('escape')
            self.assertFalse(any(action == 'vpn-disconnect' for _, action, _ in backend.calls))
            await pilot.press('3')
            await pilot.pause()
            self.assertTrue(app.query_one('#vpn-connect', Button).disabled)   # Windows: no Mullvad app controls
            self.assertFalse(app.query_one('#vpn-test', Button).disabled)    # hosts.json names a test command
            self.assertIn('tunnel has no recent handshake', str(app.query_one('#vpn-detail', Static).render()))
            await pilot.click('#vpn-test')
            await app.workers.wait_for_complete()
            self.assertIn(('home-server', 'vpn-test', None), backend.calls)
            await pilot.click('#vpn-restart')
            self.assertIsInstance(app.screen, Confirm)

    async def test_status_passes_the_vpn_state_file_from_hosts_json(self):
        backend = Backend()
        with patch.object(Backend, 'call', AsyncMock(return_value={'cpu': 1})) as call:
            await backend.status(NODES[0])
            call.assert_awaited_with(NODES[0], 'status', {'vpn_state': '~/.local/state/example/vpn.json'})


class VpnTests(unittest.TestCase):
    def test_short_text_and_detail(self):
        self.assertEqual(vpn_short(None), '—')
        self.assertEqual(vpn_short({'state': 'error', 'problems': ['x']}), 'BLOCKED !')
        self.assertEqual(vpn_short({'kind': 'mullvad', 'state': 'disconnected', 'lockdown': False}), 'off')
        self.assertEqual(since(1000, now=1040), '40 s ago')
        self.assertEqual(since(0), '—')
        self.assertEqual(since(1000, now=1000 + 3 * 3600), '3 h ago')
        text = vpn_lines('geofront', {'kind': 'tunnel', 'state': 'connected', 'relay': 'r1', 'exit_ip': '203.0.113.9', 'city': 'Stockholm',
                                      'lockdown': None, 'last_drop': 100, 'last_leak': 50, 'problems': []}, now=160)
        self.assertIn('torrent split tunnel', text)
        self.assertIn('last drop 60 s ago', text)
        self.assertIn('last leak alarm', text)
        self.assertNotIn('lockdown', text)

    def test_windows_status_file_is_normalised(self):
        import datetime
        now = datetime.datetime.now(datetime.timezone.utc)
        raw = {'at': now.isoformat(), 'tunnel': 'up', 'server': 'se-sto-wg-001', 'ok': True, 'problems': [],
               'exit': {'ok': True, 'ip': '203.0.113.9', 'city': 'Stockholm', 'provider': True},
               'lastLeakAt': '2026-11-04T23:57:55.2915319+01:00', 'lastDropAt': None}
        vpn = windows_vpn(raw)
        self.assertEqual((vpn['state'], vpn['relay'], vpn['exit_ip'], vpn['ok']), ('connected', 'se-sto-wg-001', '203.0.113.9', True))
        self.assertAlmostEqual(vpn['last_leak'], datetime.datetime(2026, 11, 4, 22, 57, 55, 291531, tzinfo=datetime.timezone.utc).timestamp(), places=0)
        stale = windows_vpn(dict(raw, at='2020-01-01T00:00:00+00:00', exit={'ok': True, 'ip': '198.51.100.1', 'provider': False}))
        self.assertFalse(stale['ok'])
        self.assertEqual(len(stale['problems']), 2)
        self.assertEqual(windows_vpn(dict(raw, tunnel='down'))['state'], 'down')
        self.assertIsNone(windows_vpn(None))

    def test_helper_reads_npukit_status_and_formats_it(self):
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'status.json'
            self.assertIsNone(helper.npu_status(str(path)))
            path.write_text('not json')
            self.assertIsNone(helper.npu_status(str(path)))
            path.write_text(json.dumps({'state': 'ready', 'components': ['chat', 'asr'], 'chat_model': 'qwen3.5:4b', 'extra': 1}))
            npu = helper.npu_status(str(path))
        self.assertNotIn('extra', npu)
        line = app_module.npu_line(npu)
        self.assertIn('#79e2c2', line)
        self.assertIn('qwen3.5 4b+STT', line)
        self.assertLessEqual(len(Text.from_markup(line).plain), 22)
        self.assertIn('REC', app_module.npu_line({'state': 'ready', 'activity': 'rec', 'components': ['asr']}))
        self.assertIn('idle', app_module.npu_line({'state': 'off', 'components': []}))
        self.assertEqual(app_module.npu_line(None), '')

    def test_helper_reads_mullvad_and_validates_relays(self):
        import subprocess, tempfile
        from pathlib import Path
        status = {'state': 'connected', 'details': {'location': {'hostname': 'se-sto-wg-003', 'city': 'Stockholm', 'ipv4': '203.0.113.5', 'mullvad_exit_ip': False}}}
        def fake_run(argv, **kwargs):
            out = json.dumps(status) if argv[1] == 'status' else 'Block traffic when the VPN is disconnected: on\n'
            return subprocess.CompletedProcess(argv, 0, out, '')
        with tempfile.TemporaryDirectory() as folder, patch('helper.shutil.which', return_value='/usr/bin/mullvad'), patch('helper.subprocess.run', side_effect=fake_run):
            state = Path(folder) / 'vpn.json'
            state.write_text(json.dumps({'last_drop': 1700000000}))
            vpn = helper.vpn_status(str(state))
        self.assertEqual((vpn['relay'], vpn['exit_ip'], vpn['lockdown'], vpn['last_drop']), ('se-sto-wg-003', '203.0.113.5', True, 1700000000))
        self.assertFalse(vpn['ok'])
        self.assertIn('not a Mullvad address', vpn['problems'][0])
        with patch('helper.shutil.which', return_value=None):
            self.assertIsNone(helper.vpn_status())
        with patch('helper.shutil.which', return_value='/usr/bin/mullvad'), patch('helper.command') as command:
            for bad in ('', 'se; rm -rf ~', 'a b c d', '../x'):
                with self.assertRaises(ValueError):
                    helper.dispatch({'action': 'vpn-relay', 'value': bad})
            command.assert_not_called()
            command.return_value = {'code': 0, 'text': ''}
            helper.dispatch({'action': 'vpn-disconnect'})
            command.assert_called_with(['mullvad', 'disconnect'], timeout=20)


class HelperTests(unittest.TestCase):
    def test_screen_time_text(self):
        data = screentime_sample()
        text = screentime_text(data)
        self.assertIn('[b]1h 00m[/b] today', text)
        self.assertIn('4 pickups, first at 08:55', text)
        self.assertIn('longest 45m (09:00–09:45)', text)
        self.assertIn('Kitty', text)
        self.assertIn('Development 40m', text)
        self.assertIn('█', text)  # the busiest hour gets a full bar
        data['daemon'] = {'running': False}
        self.assertIn('isn’t running', screentime_text(data))
        empty = screentime_text({'total': 0, 'hourly': [], 'apps': [], 'categories': []})
        self.assertIn('Nothing used yet today', empty)

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
        self.assertEqual(len(col['items']), 14)
        self.assertEqual([it['key'] for it in clipdeck.page_of(col, 0)], list('QWERTYUIOP'))
        self.assertEqual([(it['key'], it['id']) for it in clipdeck.page_of(col, 1)], list(zip('QWER', range(10, 14))))
        server = clipdeck.column(NODES[2], 2)
        clipdeck.fill_current(server, {'kind': 'empty'})
        self.assertEqual((server['state'], server['items']), ('empty', []))
        clipdeck.fill_current(server, {'kind': 'text', 'text': 'from\nwindows'})
        self.assertEqual(clipdeck.page_of(server, 0), [{'key': 'A', 'label': 'from windows', 'img': False, 'text': 'from\nwindows'}])
        self.assertNotIn('text', clipdeck.for_eww({'cols': [server]})['cols'][0]['items'][0])
        view = clipdeck.for_eww({'cols': [col, server]})
        self.assertNotIn('id', view['cols'][0]['items'][0])
        self.assertEqual((view['page'], view['pages'], view['cols'][0]['span']), (1, 2, '1–10 / 14'))
        view = clipdeck.for_eww({'page': 1, 'cols': [col, server]})
        self.assertEqual([it['label'] for it in view['cols'][0]['items']], [f'entry {i}' for i in range(10, 14)])
        self.assertEqual((view['cols'][1]['items'], view['cols'][1]['note']), ([], 'end of history · 1 entries'))

    def test_clipdeck_paging_and_pick(self):
        import clipdeck
        with tempfile.TemporaryDirectory() as tmp, \
                patch.object(clipdeck, 'RUNTIME', _Path(tmp)), patch.object(clipdeck, 'STATE', _Path(tmp) / 'deck.json'):
            col = clipdeck.column(NODES[0], 0, local=True)
            clipdeck.fill_history(col, [{'id': i, 'preview': f'entry {i}'} for i in range(25)])
            clipdeck.save({'open': True, 'page': 0, 'cols': [col]})
            for direction, page in (('prev', 0), ('next', 1), ('down', 2), ('next', 2), ('up', 1), ('first', 0), ('last', 2)):
                clipdeck.cmd_page(direction)
                self.assertEqual(clipdeck.load()['page'], page, direction)
            clipdeck.cmd_page('prev')
            with patch.object(clipdeck, 'cmd_hide'), patch.object(clipdeck, 'fetch_bytes', return_value=(b'x', None)) as fetch, \
                    patch.object(clipdeck.subprocess, 'run'):
                clipdeck.cmd_pick('2')
            self.assertEqual(fetch.call_args.args[1]['id'], 11)

    def test_clipdeck_cache_when_machine_is_off(self):
        import clipdeck
        with tempfile.TemporaryDirectory() as tmp, patch.object(clipdeck, 'CACHE', _Path(tmp) / 'cache'), \
                patch.object(clipdeck, 'CACHING', True):
            # a fresh answer is cached (with full text), and is shown at once next time
            col = clipdeck.column(NODES[1], 1)
            clipdeck.fill_history(col, [{'id': 7, 'preview': 'hello', 'text': 'hello\nworld'},
                                        {'id': 6, 'preview': '[[ binary data 1 MiB png 20x20 ]]'}])
            clipdeck.store_cache(col)
            self.assertEqual(oct(clipdeck.cache_file(col['alias']).stat().st_mode & 0o777), '0o600')
            again = clipdeck.column(NODES[1], 1)
            clipdeck.prefill(again)
            self.assertEqual((again['state'], again['cached'], len(again['items'])), ('cached', True, 2))
            self.assertTrue(again['note'].startswith('updating'))
            # the machine doesn't answer: the cached copy stays, marked offline
            clipdeck.offline(again, TimeoutError())
            self.assertEqual((again['state'], len(again['items'])), ('off', 2))
            self.assertIn('offline · showing its history from just now', again['note'])
            # text copies from the cache without the machine; an image says why it can't
            with patch.object(clipdeck, 'load', return_value={'open': True, 'page': 0, 'cols': [again]}), \
                    patch.object(clipdeck, 'cmd_hide'), patch.object(clipdeck, 'notify') as notify, \
                    patch.object(clipdeck, 'fetch_bytes', side_effect=TimeoutError()), \
                    patch.object(clipdeck.subprocess, 'run') as run:
                clipdeck.cmd_pick('Q')
                self.assertEqual(run.call_args.kwargs['input'], 'hello\nworld'.encode())
                clipdeck.cmd_pick('W')
                self.assertIn('only text entries', notify.call_args.args[1])
            # failed answers never overwrite the cache; forget empties it
            clipdeck.store_cache(again)
            self.assertEqual(json.loads(clipdeck.cache_file(col['alias']).read_text())['col']['state'], 'ok')
            clipdeck.cmd_forget()
            self.assertFalse(clipdeck.cache_file(col['alias']).exists())
        self.assertEqual(clipdeck.ago(0, 3 * 3600), '3 h ago')

    def test_cliphist_history_with_text(self):
        import subprocess
        import clipboards
        def run(cmd, **kw):
            if cmd[1] == 'list':
                return subprocess.CompletedProcess(cmd, 0, b'3\tshort\n2\t[[ binary data 1 KiB png 2x2 ]]\n1\tbad\n', b'')
            return subprocess.CompletedProcess(cmd, 0, {'3': 'short text'.encode(), '1': b'\xff\xfe'}[cmd[2]], b'')
        with patch.object(clipboards.subprocess, 'run', side_effect=run):
            entries = clipboards.cliphist_history(10, with_text=True)
        self.assertEqual([e.get('text') for e in entries], ['short text', None, None])

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
