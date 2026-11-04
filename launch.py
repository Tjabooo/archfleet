"""Open the installed dashboard in the current user's graphical Hyprland session."""
import json
import subprocess
import time
from pathlib import Path
from helper import hypr_environment

env = hypr_environment()
result = subprocess.run(['hyprctl', '-j', 'clients'], env=env, capture_output=True, text=True, check=True)
windows = json.loads(result.stdout)
existing = [w for w in windows if w.get('class') == 'archfleet']
if not existing:
    log_path = Path(__file__).resolve().parent / 'launcher.log'
    with log_path.open('ab') as log:
        proc = subprocess.Popen(['kitty', '--class', 'archfleet', '--title', 'Archfleet', '-e', str(Path.home() / '.local/bin/archfleet')], env=env, stdout=log, stderr=log, stdin=subprocess.DEVNULL, start_new_session=True)
    time.sleep(3)
    if proc.poll() is not None:
        raise RuntimeError('Kitty exited. Inspect launcher.log.')
    windows = json.loads(subprocess.run(['hyprctl', '-j', 'clients'], env=env, capture_output=True, text=True, check=True).stdout)
    existing = [w for w in windows if w.get('class') == 'archfleet']
print(json.dumps({'running': bool(existing), 'windows': [{'address': w['address'], 'size': w['size']} for w in existing]}))
if not existing:
    raise SystemExit(1)
