#!/bin/sh
# Install (or update) Archfleet for this user: ./install.sh
# Copies the app to ~/.local/share/archfleet (or $ARCHFLEET_DIR), builds its own Python venv with
# Textual, writes ~/.local/bin/{archfleet,archfleet-clipboard,clipdeck} and a desktop entry, and
# creates ~/.config/archfleet/hosts.json from hosts.example.json the first time.
# Run it on every Linux machine in the fleet: the others' helper is called over SSH from that path.
set -eu
src=$(cd "$(dirname "$0")" && pwd)
dest=${ARCHFLEET_DIR:-$HOME/.local/share/archfleet}

command -v python3 >/dev/null || { echo "archfleet needs python3" >&2; exit 1; }
for tool in ssh scp; do command -v $tool >/dev/null || echo "note: $tool is missing (install openssh)" >&2; done

mkdir -p "$dest"
if [ "$src" != "$dest" ]; then
    # app files only; the venv, logs and your config stay put
    cp "$src"/*.py "$src"/style.tcss "$src"/icon.svg "$src"/requirements.txt "$src"/hosts.example.json "$dest"/
    rm -rf "$dest/clipdeck" && cp -r "$src/clipdeck" "$dest/clipdeck"
fi
if [ ! -x "$dest/venv/bin/python" ]; then
    python3 -m venv "$dest/venv"
fi
"$dest/venv/bin/python" -m pip install --quiet --upgrade pip
"$dest/venv/bin/python" -m pip install --quiet -r "$dest/requirements.txt"
"$dest/venv/bin/python" "$dest/install.py"

case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo "note: add ~/.local/bin to your PATH" >&2 ;; esac
missing=""
for tool in wl-copy wl-paste cliphist eww hyprctl notify-send grim; do
    command -v $tool >/dev/null || missing="$missing $tool"
done
[ -z "$missing" ] || echo "optional tools not found:$missing (see README: clipboard, clipdeck, Session tab)"
cat <<MSG

Archfleet is installed in $dest.
Next:
  1. List your machines in ~/.config/archfleet/hosts.json (one entry per SSH alias).
     No SSH aliases yet? python3 $dest/setup_ssh.py sets them up from that file.
  2. Run ./install.sh on your other Linux machines too, and windows/install.ps1 on Windows ones.
  3. Start it: archfleet (or "Archfleet" in your app launcher).
MSG
