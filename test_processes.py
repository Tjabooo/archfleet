import json
import os
os.environ['ARCHFLEET_HOSTS'] = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'test_hosts.json')
import unittest
from unittest.mock import AsyncMock, patch

from textual.widgets import Button, Checkbox, DataTable, Input, TabbedContent

import processes
from app import Archfleet, Confirm, process_rows, speed, size
from backend import Backend, NODES
from test_app import FakeBackend


def proc(pid, name, cpu, own=True, **extra):
    base = {'pid': pid, 'name': name, 'user': 'tjabo' if own else 'root', 'own': own, 'start': 1000 + pid, 'cpu': cpu,
            'memory': 300 * 1024**2, 'gpu': 0.0 if own else None, 'vram': 0 if own else None,
            'disk_read': 0 if own else None, 'disk_write': 0 if own else None,
            'net_rx': 0 if own else None, 'net_tx': 0 if own else None, 'threads': 4, 'command': '/usr/bin/' + name}
    base.update(extra)
    return base


SAMPLE = {
    'platform': 'linux', 'interval': 1.0, 'cpus': 16, 'tcp_available': True,
    'totals': {'cpu': 23.0, 'memory_used': 8 * 1024**3, 'memory_total': 32 * 1024**3, 'gpu': 41.0,
               'vram_used': 5 * 1024**3, 'vram_total': 16 * 1024**3, 'disk_read': 12_000_000, 'disk_write': 900,
               'net_rx': 2_100_000, 'net_tx': 120_000},
    'processes': [
        proc(10, 'firefox', 4.5, gpu=12.0, net_rx=1_000_000, net_tx=20_000, vram=600 * 1024**2),
        proc(20, 'kitty', 0.4, disk_write=5000),
        proc(1, 'systemd', 0.1, own=False),
        proc(30, 'cs2', 12.0, gpu=55.0, command='/home/tjabo/.steam/cs2 -novid'),
    ],
}


class ProcessBackend(FakeBackend):
    def __init__(self):
        super().__init__()
        self.process_calls = 0

    async def call(self, node, action='status', value=None):
        if action == 'processes':
            self.calls.append((node['alias'], action, value))
            self.process_calls += 1
            if node['os'] != 'linux':
                return dict(SAMPLE, platform='windows', processes=[proc(4, 'System', 0.2, own=False, user=None, disk_read=10, disk_write=0, gpu=0)])
            return json.loads(json.dumps(SAMPLE))
        if action == 'process-term':
            self.calls.append((node['alias'], action, value))
            return {'code': 0, 'text': f"Sent SIGTERM to x ({value['pid']})."}
        return await super().call(node, action, value)


class ProcessViewTests(unittest.IsolatedAsyncioTestCase):
    async def open(self, pilot, app):
        await pilot.press('p')
        await pilot.pause()
        await app.workers.wait_for_complete()
        await pilot.pause()

    async def test_tab_lists_sorted_processes_and_summary(self):
        backend = ProcessBackend()
        app = Archfleet(backend)
        async with app.run_test(size=(120, 42)) as pilot:
            await app.workers.wait_for_complete()
            self.assertEqual(backend.process_calls, 0, 'no polling before the tab opens')
            await self.open(pilot, app)
            self.assertEqual(app.query_one('#tabs', TabbedContent).active, 'processes')
            table = app.query_one('#proc-table', DataTable)
            self.assertEqual([table.get_row_at(i)[1].plain for i in range(table.row_count)], ['cs2', 'firefox', 'kitty', 'systemd'])
            self.assertEqual(table.get_row_at(3)[6].plain, '—')
            self.assertIn('41% busy', str(app.query_one('#proc-gpu').render()))
            self.assertIn('CPU ▼', str(table.columns['cpu'].label))
            # Click-equivalent: sort by name, then flip it.
            app.process_header(DataTable.HeaderSelected(table, table.columns['name'].key, 1, table.columns['name'].label))
            self.assertEqual(table.get_row_at(0)[1].plain, 'cs2')
            app.process_header(DataTable.HeaderSelected(table, table.columns['name'].key, 1, table.columns['name'].label))
            self.assertEqual(table.get_row_at(0)[1].plain, 'systemd')

    async def test_filter_mine_and_end_process_confirm(self):
        backend = ProcessBackend()
        app = Archfleet(backend)
        async with app.run_test(size=(120, 42)) as pilot:
            await self.open(pilot, app)
            app.query_one('#proc-filter', Input).value = 'steam'
            await pilot.pause()
            table = app.query_one('#proc-table', DataTable)
            self.assertEqual(table.row_count, 1)
            app.query_one('#proc-filter', Input).value = ''
            app.query_one('#proc-mine', Checkbox).value = True
            await pilot.pause()
            self.assertEqual(table.row_count, 3)
            table.move_cursor(row=0)
            await pilot.pause()
            self.assertFalse(app.query_one('#proc-end', Button).disabled)
            app.query_one('#proc-end', Button).press()
            await pilot.pause()
            self.assertIsInstance(app.screen, Confirm)
            await pilot.press('escape')
            await pilot.pause()
            self.assertFalse(any(call[1] == 'process-term' for call in backend.calls))
            app.query_one('#proc-end', Button).press()
            await pilot.pause()
            app.screen.query_one('#continue', Button).press()
            await pilot.pause()
            await app.workers.wait_for_complete()
            self.assertIn(('arch-desktop', 'process-term', {'pid': 30, 'start': 1030}), backend.calls)

    async def test_root_process_cannot_be_ended_and_windows_labels(self):
        backend = ProcessBackend()
        app = Archfleet(backend)
        async with app.run_test(size=(120, 42)) as pilot:
            await self.open(pilot, app)
            table = app.query_one('#proc-table', DataTable)
            table.move_cursor(row=3)
            await pilot.pause()
            self.assertTrue(app.query_one('#proc-end', Button).disabled)
            await pilot.press('3')
            await pilot.pause()
            await app.workers.wait_for_complete()
            await pilot.pause()
            self.assertEqual(table.columns['disk'].label.plain, 'I/O')
            self.assertTrue(app.query_one('#proc-end', Button).disabled)
            self.assertIn('no per-process network', str(app.query_one('#proc-note').render()))

    async def test_polling_stops_after_leaving_tab(self):
        backend = ProcessBackend()
        app = Archfleet(backend)
        async with app.run_test(size=(120, 42)) as pilot:
            await self.open(pilot, app)
            calls = backend.process_calls
            app.query_one('#tabs', TabbedContent).active = 'overview'
            await pilot.pause()
            app.process_tick()
            await app.workers.wait_for_complete()
            self.assertEqual(backend.process_calls, calls)

    async def test_failed_refresh_keeps_last_read(self):
        backend = ProcessBackend()
        app = Archfleet(backend)
        async with app.run_test(size=(120, 42)) as pilot:
            await self.open(pilot, app)
            original = backend.call
            async def failing(node, action='status', value=None):
                if action == 'processes':
                    raise RuntimeError('Could not read Windows processes.')
                return await original(node, action, value)
            backend.call = failing
            app.refresh_processes()
            await app.workers.wait_for_complete()
            await pilot.pause()
            self.assertEqual(app.query_one('#proc-table', DataTable).row_count, 4)
            self.assertIn('Refresh failed', str(app.query_one('#proc-status').render()))
            await pilot.press('3')
            await pilot.pause()
            await app.workers.wait_for_complete()
            await pilot.pause()
            self.assertEqual(app.query_one('#proc-table', DataTable).row_count, 0)
            self.assertIn("Can't read processes on homeserverino", str(app.query_one('#proc-status').render()))

    async def test_windows_script_travels_on_stdin(self):
        payload = json.dumps(dict(SAMPLE, platform='windows'))
        with patch('backend.run', new=AsyncMock(return_value=(0, payload, ''))) as execute:
            data = await Backend().call(NODES[2], 'processes')
        self.assertEqual(data['platform'], 'windows')
        self.assertNotIn('Get-Counter', ' '.join(execute.call_args.args[0]))
        self.assertIn(b'Get-Counter', execute.call_args.kwargs['input_data'])


class ProcessHelperTests(unittest.TestCase):
    def test_formatting(self):
        self.assertEqual(speed(0), '0')
        self.assertEqual(speed(None), '—')
        self.assertEqual(speed(1_400_000), '1.4 MB/s')
        self.assertEqual(speed(56_000), '56 kB/s')
        self.assertEqual(size(512 * 1024**2), '512 MiB')
        self.assertEqual(size(3 * 1024**3), '3.0 GiB')

    def test_unreadable_values_sort_last(self):
        rows = process_rows(SAMPLE['processes'], sort='disk')
        self.assertEqual(rows[-1]['name'], 'systemd')
        self.assertEqual(rows[0]['name'], 'kitty')

    def test_collect_reads_this_machine(self):
        data = processes.collect(sample=0.2)
        me = next(p for p in data['processes'] if p['pid'] == os.getpid())
        self.assertTrue(me['own'])
        self.assertIsNotNone(me['disk_read'])
        self.assertGreater(me['memory'], 0)
        self.assertGreaterEqual(data['totals']['cpu'], 0)
        json.dumps(data)

    def test_ss_parser_attributes_bytes(self):
        output = ('ESTAB 0 0 10.0.0.2:5000 1.1.1.1:443 users:(("firefox",pid=10,fd=3))\n'
                  '\t cubic bytes_sent:200 bytes_acked:200 bytes_received:5000 segs_out:3\n'
                  'ESTAB 0 0 10.0.0.2:22 10.0.0.9:5000\n\t cubic bytes_sent:1 bytes_received:1\n')
        with patch('processes.subprocess.run') as run:
            run.return_value.stdout = output
            self.assertEqual(processes.read_tcp(), {('10.0.0.2:5000', '1.1.1.1:443'): (10, 5000, 200)})

    def test_terminate_refuses_reused_pid_and_other_users(self):
        with self.assertRaises(ValueError):
            processes.terminate({'pid': os.getpid(), 'start': -1})
        with self.assertRaises(ValueError):
            processes.terminate({'pid': 1, 'start': 0})
        other = next((int(e.name) for e in os.scandir('/proc') if e.name.isdigit() and int(e.name) > 1 and e.stat().st_uid != os.getuid()), None)
        if other is not None:
            with self.assertRaises(ValueError) as error:
                processes.terminate({'pid': other, 'start': processes.read_stat(other)[4]})
            self.assertIn('own processes', str(error.exception))


if __name__ == '__main__':
    unittest.main()
