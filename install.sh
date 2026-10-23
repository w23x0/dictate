#!/usr/bin/env bash
# dictate 一键安装：装二进制、应用入口、开机自启，并后台起托盘。
#
# 从 GitHub Release 装（推荐的“一行命令”）：
#   curl -fsSL https://raw.githubusercontent.com/<repo>/main/install.sh | bash
#
# 从本地源码装（开发者）：
#   ./install.sh --local
#
# 卸载：./uninstall.sh
set -euo pipefail

REPO="${DICTATE_REPO:-w23x0/dictate}"                       # 形如 owner/dictate
# 下载根：默认 GitHub；换成镜像或本地目录也能装（便于测试/国内加速）
BASE_URL="${DICTATE_BASE_URL:-https://github.com/$REPO/releases}"
VERSION="${DICTATE_VERSION:-latest}"
PREFIX="${DICTATE_PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
MODE="release"

for arg in "$@"; do
  case "$arg" in
    --local) MODE="local" ;;
    --user) PREFIX="$HOME/.local"; BIN_DIR="$PREFIX/bin" ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "未知参数：$arg" >&2; exit 2 ;;
  esac
done

say() { printf '\033[36m▸\033[0m %s\n' "$*"; }
die() { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || die "缺少依赖：$1"; }

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
  *) die "暂不支持的系统：$(uname -s) $(uname -m)（目前只出 Linux 包）" ;;
esac

mkdir -p "$BIN_DIR"

# 升级时先停掉正在跑的实例，否则 install 会因为 Text file busy 失败
pkill -f "$BIN_DIR/dictate" 2>/dev/null || true
sleep 0.5
rm -f "$HOME/.local/share/dictate/daemon.pid" 2>/dev/null || true

if [[ "$MODE" == "local" ]]; then
  need cargo
  say "从本地源码编译（release）…"
  ( cd "$(dirname "$0")" && cargo build --release -q )
  SRC="$(dirname "$0")/target/release/dictate"
  [[ -x "$SRC" ]] || die "没找到编译产物：$SRC"
  install -m 755 "$SRC" "$BIN_DIR/dictate"
else
  [[ -n "$REPO" ]] || die "没设置仓库：DICTATE_REPO=owner/dictate ./install.sh"
  need curl
  need tar
  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT
  if [[ "$VERSION" == "latest" ]]; then
    URL="$BASE_URL/latest/download/dictate-$TARGET.tar.gz"
  else
    URL="$BASE_URL/download/$VERSION/dictate-$TARGET.tar.gz"
  fi
  say "下载 $URL"
  curl -fsSL "$URL" -o "$TMP/dictate.tar.gz" || die "下载失败（这个平台还没有发布包？）"
  tar -xzf "$TMP/dictate.tar.gz" -C "$TMP"
  install -m 755 "$TMP/dictate" "$BIN_DIR/dictate"
fi

say "已安装：$BIN_DIR/dictate"
"$BIN_DIR/dictate" version

# 只有装到标准位置时才写桌面入口与自启；测试用的临时前缀不碰用户环境
if [[ "$BIN_DIR" != "$HOME/.local/bin" ]]; then
  say "前缀不是 ~/.local/bin，跳过桌面入口与自启（只放了二进制）"
  echo "  $BIN_DIR/dictate 可直接运行"
  exit 0
fi

# 应用入口 + 图标（能被应用搜索找到）
"$BIN_DIR/dictate" install-desktop >/dev/null 2>&1 || true
say "已写入应用入口（~/.local/share/applications/dictate.desktop）"

# 开机自启
AUTOSTART="$HOME/.config/autostart"
mkdir -p "$AUTOSTART"
cat > "$AUTOSTART/dictate.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Dictate
Comment=按热键说话，文字进历史（通义千问 ASR）
Exec=$BIN_DIR/dictate
Icon=dictate
Terminal=false
X-GNOME-Autostart-enabled=true
EOF
say "已设置开机自启（$AUTOSTART/dictate.desktop）"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) say "提醒：$BIN_DIR 不在 PATH 里，把这一行加进 ~/.profile：
    export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
esac

# 起托盘（后台，不占终端）
if ! pgrep -f "$BIN_DIR/dictate" >/dev/null 2>&1; then
  setsid nohup "$BIN_DIR/dictate" >/dev/null 2>&1 &
  sleep 1
fi

say "完成"
echo "  用法：按住热键说话（默认 ctrl+space），托盘菜单里能看历史"
echo "  首次使用：dictate settings 填 API key，然后 dictate doctor 验证"
