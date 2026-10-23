#!/bin/sh
# audio-spectrum Linux 安装脚本:二进制 + 桌面入口 + hicolor 图标,
# 装入用户目录(~/.local),无需 root。卸载命令见脚本输出。
set -eu

PREFIX="${HOME}/.local"
BIN_DIR="${PREFIX}/bin"
DATA_DIR="${PREFIX}/share"

cd "$(dirname "$0")"

mkdir -p "${BIN_DIR}" "${DATA_DIR}/applications"
install -m 755 audio-spectrum "${BIN_DIR}/audio-spectrum"
install -m 644 audio-spectrum.desktop "${DATA_DIR}/applications/audio-spectrum.desktop"

for size in 16x16 32x32 64x64 128x128 256x256 512x512; do
    dir="${DATA_DIR}/icons/hicolor/${size}/apps"
    mkdir -p "${dir}"
    install -m 644 "icons/hicolor/${size}/apps/audio-spectrum.png" "${dir}/audio-spectrum.png"
done

# 缓存刷新:缺工具时静默跳过(多数桌面环境即时生效)。
command -v update-desktop-database >/dev/null 2>&1 \
    && update-desktop-database "${DATA_DIR}/applications" || true
command -v gtk-update-icon-cache >/dev/null 2>&1 \
    && gtk-update-icon-cache -f -t "${DATA_DIR}/icons/hicolor" || true

echo "已安装到 ${PREFIX}(请确保 ${BIN_DIR} 在 PATH 中)。"
echo ""
echo "卸载:"
echo "  rm ${BIN_DIR}/audio-spectrum"
echo "  rm ${DATA_DIR}/applications/audio-spectrum.desktop"
echo "  rm -r ${DATA_DIR}/icons/hicolor/*/apps/audio-spectrum.png"
