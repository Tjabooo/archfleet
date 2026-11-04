"""Render the dashboard against your real machines into docs/ (SVG, plus PNG if rsvg-convert exists)."""
import asyncio
import shutil
import subprocess
from pathlib import Path
from textual.widgets import TabbedContent
from app import Archfleet

DOCS = Path(__file__).resolve().parent / 'docs'


async def main():
    app = Archfleet()
    async with app.run_test(size=(140, 44)) as pilot:
        await app.workers.wait_for_complete()
        await pilot.pause(1)
        await app.workers.wait_for_complete()
        app.save_screenshot('overview.svg', path=str(DOCS))
        app.query_one('#tabs', TabbedContent).active = 'maintenance'
        await pilot.pause(1)
        app.save_screenshot('maintenance.svg', path=str(DOCS))
    for svg in DOCS.glob('*.svg'):
        if shutil.which('rsvg-convert'):
            subprocess.run(['rsvg-convert', '-z', '1.5', '-o', str(svg.with_suffix('.png')), str(svg)], check=True)
            svg.unlink()
    print('Saved screenshots in', DOCS)


if __name__ == '__main__':
    asyncio.run(main())
