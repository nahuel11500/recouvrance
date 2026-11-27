#!/usr/bin/env bash
# Remove what install.sh added, and the app menu entry and icons the app adds on its first start
# (or `recouvrance --install-desktop-entry`) at the same paths. Your settings in ~/.config/recouvrance and the
# agent activity log in ~/.local/share/recouvrance are kept.
set -euo pipefail
bin_dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
rm -f "$bin_dir/recouvrance" "$data_dir/applications/recouvrance.desktop" "$data_dir/icons/hicolor/scalable/apps/recouvrance.svg" \
  "$data_dir/icons/hicolor/16x16/apps/recouvrance.svg" "$data_dir/icons/hicolor/24x24/apps/recouvrance.svg" \
  "$data_dir/recouvrance/desktop-entry"
update-desktop-database "$data_dir/applications" 2>/dev/null || true
echo "recouvrance uninstalled"
