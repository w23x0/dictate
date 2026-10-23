#!/usr/bin/env bash
# 卸载 dictate：停进程、删二进制、应用入口、自启；数据目录默认保留（--purge 才删）。
set -euo pipefail

PREFIX="${DICTATE_PREFIX:-$HOME/.local}"
PURGE=0
for arg in "$@"; do
  case "$arg" in
    --purge) PURGE=1 ;;
    -h|--help) sed -n '2,4p' "$0"; exit 0 ;;
  esac
done

say() { printf '\033[36m▸\033[0m %s\n' "$*"; }

pkill -f "$PREFIX/bin/dictate" 2>/dev/null || true
sleep 0.5
say "已停止进程"

rm -f "$PREFIX/bin/dictate"
rm -f "$HOME/.local/share/applications/dictate.desktop"
rm -f "$HOME/.local/share/icons/hicolor/64x64/apps/dictate.png"
rm -f "$HOME/.config/autostart/dictate.desktop"
say "已删除二进制与桌面入口"

if [[ "$PURGE" == "1" ]]; then
  rm -rf "$HOME/.local/share/dictate" "$HOME/.config/dictate"
  say "已删除配置与历史（--purge）"
else
  say "配置与历史保留在 ~/.config/dictate 与 ~/.local/share/dictate（加 --purge 删除）"
fi
