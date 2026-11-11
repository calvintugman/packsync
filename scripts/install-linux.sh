#!/usr/bin/env bash
# Install PackSync for the current user (no root): the app goes to ~/.local/opt/packsync, an entry
# appears in the application menu, and `packsync` is linked into ~/.local/bin.
# Run it from the unpacked Linux download. `./install.sh --uninstall` removes all of it again
# (your settings, local snapshots and cloud data are not touched).
set -euo pipefail

src="$(cd "$(dirname "$0")" && pwd)"
dest="$HOME/.local/opt/packsync"
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
icons="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"
bin="$HOME/.local/bin"

if [ "${1:-}" = "--uninstall" ]; then
  rm -f "$apps/packsync.desktop" "$icons/128x128/apps/packsync.png" "$icons/256x256/apps/packsync.png"
  [ -L "$bin/packsync" ] && rm -f "$bin/packsync"
  rm -rf "$dest"
  update-desktop-database "$apps" >/dev/null 2>&1 || true
  echo "PackSync removed. Your settings and data are still there (packsync's config and state folders)."
  exit 0
fi

for f in packsync packsync-desktop; do
  [ -x "$src/$f" ] || { echo "error: $f is missing next to this script" >&2; exit 1; }
done

mkdir -p "$dest" "$apps" "$icons/128x128/apps" "$icons/256x256/apps" "$bin"
# replace the programs without disturbing a running copy
for f in packsync packsync-desktop; do
  install -m 755 "$src/$f" "$dest/$f.new" && mv -f "$dest/$f.new" "$dest/$f"
done
[ -f "$src/icon-128.png" ] && install -m 644 "$src/icon-128.png" "$icons/128x128/apps/packsync.png"
[ -f "$src/icon-256.png" ] && install -m 644 "$src/icon-256.png" "$icons/256x256/apps/packsync.png"
ln -sfn "$dest/packsync" "$bin/packsync"

cat > "$apps/packsync.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=PackSync
GenericName=Modpack sync
Comment=Continue your Minecraft modpack on any computer
Exec="$dest/packsync-desktop"
Icon=packsync
Terminal=false
Categories=Game;
Keywords=minecraft;prism;modpack;sync;
EOF
update-desktop-database "$apps" >/dev/null 2>&1 || true
gtk-update-icon-cache -q "$icons" >/dev/null 2>&1 || true

echo "PackSync installed: open it from your application menu (or run $dest/packsync-desktop)."
case ":$PATH:" in
  *":$bin:"*) echo "The command-line tool is available as: packsync" ;;
  *) echo "The command-line tool is at $bin/packsync (add $bin to your PATH to type just 'packsync')." ;;
esac
