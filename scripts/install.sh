#!/usr/bin/env bash
# Build recouvrance and install it for the current user (no sudo):
#   ~/.local/bin/recouvrance, an app icon and a desktop entry.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --locked

bin_dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
mkdir -p "$bin_dir" "$data_dir/applications" "$data_dir/icons/hicolor/scalable/apps"

install -m 755 target/release/recouvrance "$bin_dir/recouvrance"
install -m 644 assets/recouvrance.svg "$data_dir/icons/hicolor/scalable/apps/recouvrance.svg"

# The repository's desktop entry, with Exec pointing at the installed binary (an absolute path,
# since ~/.local/bin is not always on the desktop session's PATH). The Desktop Entry spec wants it
# double-quoted with \ " ` $ backslash-escaped, then every backslash escaped again (string value
# rule), and % doubled.
exec_path="$bin_dir/recouvrance"
exec_path="${exec_path//\\/\\\\}"
exec_path="${exec_path//\"/\\\"}"
exec_path="${exec_path//\`/\\\`}"
exec_path="${exec_path//\$/\\\$}"
exec_path="${exec_path//\\/\\\\}"
exec_path="${exec_path//%/%%}"
# The file name matches the window's Wayland app id, so the dock shows the icon.
desktop="$data_dir/applications/recouvrance.desktop"
while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in
    Exec=*) printf 'Exec="%s"\n' "$exec_path" ;;
    *) printf '%s\n' "$line" ;;
  esac
done < assets/recouvrance.desktop > "$desktop"
chmod 644 "$desktop"

update-desktop-database "$data_dir/applications" 2>/dev/null || true
gtk-update-icon-cache -q "$data_dir/icons/hicolor" 2>/dev/null || true
echo "Installed: $bin_dir/recouvrance (launch it from your app menu as \"Recouvrance\")"
