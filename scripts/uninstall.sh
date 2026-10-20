#!/usr/bin/env bash
# Remove what install.sh added. Your settings in ~/.config/recouvrance and the
# agent activity log in ~/.local/share/recouvrance are kept.
set -euo pipefail
bin_dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
rm -f "$bin_dir/recouvrance" "$data_dir/applications/recouvrance.desktop" "$data_dir/icons/hicolor/scalable/apps/recouvrance.svg"
update-desktop-database "$data_dir/applications" 2>/dev/null || true
echo "recouvrance uninstalled"
