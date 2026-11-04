"""Write the launchers, desktop entry and first config. install.sh runs this from the app folder."""
import json
import shutil
from pathlib import Path

root = Path(__file__).resolve().parent
home = Path.home()
config = home / '.config' / 'archfleet'
config.mkdir(parents=True, exist_ok=True)
hosts = config / 'hosts.json'
created = not hosts.exists()
if created:
    hosts.write_text((root / 'hosts.example.json').read_text())
bin_dir = home / '.local' / 'bin'
bin_dir.mkdir(parents=True, exist_ok=True)


def shell_path(path):
    """$HOME/... when under the home folder, so the launchers survive being copied into dotfiles."""
    path = Path(path)
    return '$HOME/' + path.relative_to(home).as_posix() if path.is_relative_to(home) else str(path)


def wrapper(name, python, script):
    path = bin_dir / name
    path.write_text(f'#!/bin/sh\nexec "{shell_path(python)}" "{shell_path(root / script)}" "$@"\n')
    path.chmod(0o755)
    return path


launcher = wrapper('archfleet', root / 'venv' / 'bin' / 'python', 'app.py')
wrapper('archfleet-clipboard', root / 'venv' / 'bin' / 'python', 'clipboard_cli.py')
# SUPER+V picker: plain system Python (standard library only) so it opens fast.
wrapper('clipdeck', '/usr/bin/python3', 'clipdeck.py')

# The window class "archfleet" lets compositor rules float or size it; kitty and foot can set it.
terminal = next((t for t in ('kitty', 'foot', 'alacritty') if shutil.which(t)), None)
exec_line = {'kitty': f'kitty --class archfleet --title Archfleet -e {launcher}',
             'foot': f'foot --app-id archfleet --title Archfleet {launcher}',
             'alacritty': f'alacritty --class archfleet --title Archfleet -e {launcher}'}.get(terminal, str(launcher))
applications = home / '.local' / 'share' / 'applications'
applications.mkdir(parents=True, exist_ok=True)
desktop = applications / 'archfleet.desktop'
desktop.write_text(f'''[Desktop Entry]
Type=Application
Name=Archfleet
Comment=Home network, SSH, files, and Hyprland controls
Exec={exec_line}
Icon={root / 'icon.svg'}
Terminal={'false' if terminal else 'true'}
Categories=Network;RemoteAccess;
Keywords=SSH;SFTP;Hyprland;Arch;Network;
StartupWMClass=archfleet
''')
print(json.dumps({'launcher': str(launcher), 'desktop_entry': str(desktop), 'configuration': str(hosts), 'new_configuration': created}))
