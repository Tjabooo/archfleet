#!/bin/sh
# Install (or update) Archfleet for this user: ./install.sh
# Builds the archfleet binary (or downloads the latest release when Rust isn't installed, or takes
# ARCHFLEET_BINARY=<path>), puts it in
# ~/.local/share/archfleet (or $ARCHFLEET_DIR), links archfleet, archfleet-clipboard and clipdeck into
# ~/.local/bin, writes a desktop entry, and creates ~/.config/archfleet/hosts.json the first time.
# Run it on every Linux machine in the fleet: the dashboard calls `archfleet helper` there over SSH.
set -eu
src=$(cd "$(dirname "$0")" && pwd)
dest=${ARCHFLEET_DIR:-$HOME/.local/share/archfleet}
bin_dir=$HOME/.local/bin
repo=Tjabooo/archfleet

for tool in ssh scp; do command -v $tool >/dev/null || echo "note: $tool is missing (install openssh)" >&2; done
mkdir -p "$dest" "$bin_dir"

if [ -n "${ARCHFLEET_BINARY:-}" ]; then
    built=$ARCHFLEET_BINARY  # a binary built elsewhere (same architecture)
elif command -v cargo >/dev/null; then
    echo "Building archfleet (release)…"
    cargo build --release --locked --quiet --manifest-path "$src/Cargo.toml"
    built=$src/target/release/archfleet
else
    case $(uname -m) in
        x86_64) asset=archfleet-linux-x86_64 ;;
        aarch64) asset=archfleet-linux-aarch64 ;;
        *) echo "No prebuilt archfleet for $(uname -m): install Rust (https://rustup.rs) and run this again." >&2; exit 1 ;;
    esac
    echo "Rust isn't installed: downloading the latest $asset release…"
    built=$(mktemp)
    curl -fsSL -o "$built" "https://github.com/$repo/releases/latest/download/$asset"
fi
# replace atomically, so a running dashboard or helper keeps its old copy until it exits
install -m 755 "$built" "$dest/archfleet.new"
mv -f "$dest/archfleet.new" "$dest/archfleet"
case $built in "$src/target/release/archfleet" | "${ARCHFLEET_BINARY:-}") ;; *) rm -f "$built" ;; esac

if [ "$src" != "$dest" ]; then
    cp "$src/icon.svg" "$src/hosts.example.json" "$dest/"
    rm -rf "$dest/clipdeck" && cp -r "$src/clipdeck" "$dest/clipdeck"
fi
# what the Python version left behind
rm -rf "$dest/venv" "$dest/__pycache__"
rm -f "$dest"/*.py "$dest/style.tcss" "$dest/requirements.txt" "$dest/test_hosts.json" "$dest/launcher.log"

for name in archfleet archfleet-clipboard clipdeck; do
    ln -sfn "$dest/archfleet" "$bin_dir/$name"
done

config=$HOME/.config/archfleet
mkdir -p "$config"
new_config=no
if [ ! -e "$config/hosts.json" ]; then
    cp "$src/hosts.example.json" "$config/hosts.json"
    new_config=yes
fi

# The window class "archfleet" lets compositor rules float or size it; kitty, foot and alacritty set it.
if command -v kitty >/dev/null; then
    exec_line="kitty --class archfleet --title Archfleet -e $bin_dir/archfleet"; terminal=false
elif command -v foot >/dev/null; then
    exec_line="foot --app-id archfleet --title Archfleet $bin_dir/archfleet"; terminal=false
elif command -v alacritty >/dev/null; then
    exec_line="alacritty --class archfleet --title Archfleet -e $bin_dir/archfleet"; terminal=false
else
    exec_line="$bin_dir/archfleet"; terminal=true
fi
mkdir -p "$HOME/.local/share/applications"
cat > "$HOME/.local/share/applications/archfleet.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Archfleet
Comment=Home network, SSH, files, and Hyprland controls
Exec=$exec_line
Icon=$dest/icon.svg
Terminal=$terminal
Categories=Network;RemoteAccess;
Keywords=SSH;SFTP;Hyprland;Arch;Network;
StartupWMClass=archfleet
EOF

case ":$PATH:" in *":$bin_dir:"*) ;; *) echo "note: add ~/.local/bin to your PATH" >&2 ;; esac
missing=""
for tool in wl-copy wl-paste cliphist eww hyprctl notify-send grim; do
    command -v $tool >/dev/null || missing="$missing $tool"
done
[ -z "$missing" ] || echo "optional tools not found:$missing (see README: clipboard, clipdeck, Session tab)"
echo
echo "Archfleet $("$dest/archfleet" --version | cut -d' ' -f2) is installed in $dest."
if [ $new_config = yes ]; then
    cat <<MSG
Next:
  1. List your machines in $config/hosts.json (one entry per SSH alias).
     No SSH aliases yet? archfleet setup-ssh sets them up from that file.
  2. Run ./install.sh on your other Linux machines too, and windows/install.ps1 on Windows ones.
  3. Start it: archfleet (or "Archfleet" in your app launcher).
MSG
fi
