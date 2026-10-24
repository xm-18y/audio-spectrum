#!/usr/bin/env python3
"""生成 README 界面截图(布局/配色/文案取自应用真实常量,音频内容为模拟)。

输出:docs/screenshots/spectrum.png · spectrogram.png(920×440 窗口的 2x 渲染)
依赖:Pillow(pip install pillow);Windows 缺字体时回退 simhei/arial。
"""

from PIL import Image, ImageDraw, ImageFont
import math
import random
import os

S = 2                                   # 2x 渲染
W, H = 920 * S, 440 * S
MENU_H = 28 * S
BANDS = 96
COLUMNS = 600                           # 语谱列数(与 SPECTRO_COLUMN_COUNT 一致)

# —— 调色板(Classic 主题,与 crates/app/src/lib.rs 一致)——
BAR_BOTTOM = (56, 92, 224)
BAR_TOP = (255, 72, 52)
BG = (12, 14, 19)
PEAK = (232, 238, 248)
MENU_BG = (18, 20, 28)                  # 0.07,0.08,0.11
AXIS_TEXT = (108, 114, 130)
BTN_FG = (217, 222, 235)
GRID = (36, 40, 50)
FREQ_GRID = (36, 40, 50)

FREQ_MIN, FREQ_MAX = 20.0, 20000.0
DB_CEILING, DB_FLOOR = 0.0, -80.0
FREQ_TICKS = [63, 125, 250, 500, 1000, 2000, 4000, 8000, 16000]
DB_TICKS = [0, -20, -40, -60]

OUT_DIR = os.path.join(os.path.dirname(__file__), "..", "docs", "screenshots")


def font(size: int, mono: bool = False) -> ImageFont.FreeTypeFont:
    cands = (
        ["consola.ttf", "arial.ttf"]
        if mono
        else ["msyh.ttc", "simhei.ttf", "arial.ttf"]
    )
    for name in cands:
        try:
            return ImageFont.truetype(f"C:/Windows/Fonts/{name}", size * S)
        except OSError:
            continue
    return ImageFont.load_default()


def mix(a, b, t: float):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def band_color(t: float):
    t = min(1.0, max(0.0, t))
    return mix(BAR_BOTTOM, BAR_TOP, t)


def freq_to_x(f: float, l: float, w: float) -> float:
    return l + w * math.log(f / FREQ_MIN) / math.log(FREQ_MAX / FREQ_MIN)


def db_to_y(db: float, top: float, h: float) -> float:
    return top + (DB_CEILING - db) / (DB_CEILING - DB_FLOOR) * h


# ---------------------------------------------------------------------------
# 模拟音频内容:96 频段 × 600 列强度(鼓点 + 旋律 + 人声段 + 底噪)
# ---------------------------------------------------------------------------

def synth_intensity() -> list[list[float]]:
    rng = random.Random(7)
    mat = []
    for col in range(COLUMNS):
        row = []
        kick_env = max(0.0, 1.0 - (col % 32) / 9.0)
        mel = 40 + 18 * math.sin(col / 60.0)
        for band in range(BANDS):
            v = math.exp(-band / 28.0) * 0.38                      # 低频基础能量
            v += math.exp(-((band - mel) ** 2) / 12.0) * 0.6       # 游走旋律
            if col < COLUMNS * 0.45 or col > COLUMNS * 0.62:       # 人声段
                v += (0.28 + 0.12 * math.sin(col / 23.0)) * math.exp(
                    -((band - 52) ** 2) / 90.0
                )
            v += kick_env * math.exp(-((band / 5.5) ** 2)) * 0.65  # 底鼓
            v += rng.uniform(0.0, 0.05)                            # 底噪
            row.append(min(1.0, v))
        mat.append(row)
    return [list(c) for c in zip(*mat)]  # 转置为 [band][col]


# ---------------------------------------------------------------------------
# 界面外壳(菜单栏 + 频率刻度),两视图共用
# ---------------------------------------------------------------------------

def shell(draw: ImageDraw.ImageDraw) -> tuple[float, float, float, float]:
    draw.rectangle([0, 0, W, H], fill=BG)
    draw.rectangle([0, 0, W, MENU_H], fill=MENU_BG)
    for i, label in enumerate(["视图", "声源"]):
        x = (6 + i * 56) * S
        draw.rounded_rectangle(
            [x, 3 * S, x + 56 * S, MENU_H - 3 * S],
            radius=4 * S,
            fill=(30, 34, 44),
        )
        f = font(12)
        tw = draw.textlength(label, font=f)
        draw.text((x + (56 * S - tw) / 2, (MENU_H - 24 * S) / 2), label,
                  font=f, fill=BTN_FG)
    # 设置 + 关于(末尾):右侧直达按钮。
    f = font(12)
    for i, label in enumerate(["关于", "设置"]):   # 关于在最末
        tw = draw.textlength(label, font=f)
        bw = max(tw + 16 * S, 40 * S)
        bx = W - (12 + i * 52) * S - bw
        draw.rounded_rectangle(
            [bx, 3 * S, bx + bw, MENU_H - 3 * S],
            radius=4 * S,
            fill=(30, 34, 44),
        )
        draw.text((bx + (bw - tw) / 2, (MENU_H - 24 * S) / 2), label,
                  font=f, fill=BTN_FG)
    return 0.0, MENU_H, W, H  # plot 区(菜单栏以下全出血)


def freq_axis_lines(draw: ImageDraw.ImageDraw, plot) -> None:
    """频率刻度线(柱条之下)。"""
    l, t, r, b = plot
    for f in FREQ_TICKS:
        x = freq_to_x(f, l, r - l)
        draw.line([(x, t), (x, b)], fill=FREQ_GRID, width=1)


def freq_axis_labels(draw: ImageDraw.ImageDraw, plot) -> None:
    """频率刻度文字(柱条之上,保证可读)。"""
    l, t, r, b = plot
    for f in FREQ_TICKS:
        x = freq_to_x(f, l, r - l)
        label = f"{f // 1000}k" if f >= 1000 else str(f)
        fnt = font(10, mono=True)
        tw = draw.textlength(label, font=fnt)
        draw.text((x - tw / 2, b - 14 * S), label, font=fnt, fill=AXIS_TEXT)


def tooltip(draw: ImageDraw.ImageDraw, x: float, y: float, content: str) -> None:
    f = font(11, mono=True)
    tw = int(draw.textlength(content, font=f))
    th = 16 * S
    box = [x + 12 * S, y - 16 * S - th, x + 12 * S + tw + 12 * S, y - 16 * S]
    draw.rounded_rectangle(box, radius=3 * S, fill=(28, 31, 40))
    draw.text((box[0] + 6 * S, box[1] + 2 * S), content, font=f, fill=(255, 255, 255))


def hud(draw: ImageDraw.ImageDraw, text: str) -> None:
    f = font(10)
    tw = int(draw.textlength(text, font=f))
    box = [W - 12 * S - tw - 20 * S, H - 12 * S - 22 * S, W - 12 * S, H - 12 * S]
    draw.rounded_rectangle(box, radius=4 * S, fill=(12, 14, 19))
    draw.text((box[0] + 10 * S, box[1] + 3 * S), text, font=f, fill=AXIS_TEXT)


# ---------------------------------------------------------------------------
# 频谱视图:柱条 + 峰值帽 + dB 网格
# ---------------------------------------------------------------------------

def render_spectrum(mat) -> Image.Image:
    img = Image.new("RGB", (W, H), BG)
    draw = ImageDraw.Draw(img, "RGBA")
    plot = shell(draw)
    l, t, r, b = plot
    h = b - t

    for db in DB_TICKS:
        y = db_to_y(db, t, h)
        draw.line([(l, y), (r, y)], fill=GRID, width=1)
    freq_axis_lines(draw, plot)

    # 柱条:近期加权值(平滑显示态)→ dB;峰值帽取长窗最大(拖尾感)。
    for i in range(BANDS):
        f_lo = FREQ_MIN * (FREQ_MAX / FREQ_MIN) ** (i / BANDS)
        f_hi = FREQ_MIN * (FREQ_MAX / FREQ_MIN) ** ((i + 1) / BANDS)
        x0 = freq_to_x(f_lo, l, r - l) + 0.5 * S
        x1 = freq_to_x(f_hi, l, r - l) - 0.5 * S
        raw = x1 - x0
        if raw < 0.5:
            continue
        inset = raw * 0.15 * 0.5
        x0, x1 = x0 + inset, x1 - inset
        w = mat[i]
        cur = 0.6 * w[-1] + 0.3 * w[-4] + 0.1 * w[-10]
        bar_top = db_to_y(-80.0 + cur * 80.0, t, h)
        if b - bar_top > 0.5:
            SEG = 12
            for s in range(SEG):
                t0, t1 = s / SEG, (s + 1) / SEG
                y0 = b + (bar_top - b) * t0
                y1 = b + (bar_top - b) * t1
                draw.rectangle([x0, min(y0, y1), x1, max(y0, y1)],
                               fill=band_color((t0 + t1) / 2))
        peak = min(max(max(w[-300:]), cur + 0.02), cur + 0.08)
        py = db_to_y(-80.0 + peak * 80.0, t, h)
        draw.rectangle([x0, py - 4 * S, x1, py], fill=(*PEAK, 225))

    # 刻度文字叠在柱条上(真实应用中标签在柱后,此处以可读为先)。
    for db in DB_TICKS:
        y = db_to_y(db, t, h)
        fnt = font(10, mono=True)
        draw.text((l + 4 * S, y - 16 * S), f"{db}dB", font=fnt, fill=AXIS_TEXT)
    freq_axis_labels(draw, plot)

    tooltip(draw, W * 0.52, H * 0.42, "1.2kHz · -18dB")
    hud(draw, "3.12ms/帧 · 60fps · ≈1.8% CPU")
    return img


# ---------------------------------------------------------------------------
# 语谱视图:强度纹理(行 = 频段,顶 = 高频;列 = 时间,右 = 最新)
# ---------------------------------------------------------------------------

def render_spectrogram(mat) -> Image.Image:
    img = Image.new("RGB", (W, H), BG)
    draw = ImageDraw.Draw(img, "RGBA")
    plot = shell(draw)
    l, t, r, b = plot

    # 纹理:600×96 → 放大到 plot 区(NEAREST 保持块状)。
    tex = Image.new("RGB", (COLUMNS, BANDS))
    px = tex.load()
    for x in range(COLUMNS):
        for band in range(BANDS):
            v = mat[band][x]
            hot = band_color(v)
            px[x, BANDS - 1 - band] = mix(BG, hot, v)   # 顶行 = 高频
    img.paste(tex.resize((int(r - l), int(b - t)), Image.NEAREST), (int(l), int(t)))
    draw = ImageDraw.Draw(img, "RGBA")

    freq_axis_lines(draw, plot)
    freq_axis_labels(draw, plot)
    tooltip(draw, W * 0.52, H * 0.42, "2.4kHz · -21dB")
    hud(draw, "3.12ms/帧 · 60fps · ≈1.8% CPU")
    return img


def main() -> None:
    os.makedirs(OUT_DIR, exist_ok=True)
    mat = synth_intensity()
    render_spectrum(mat).save(os.path.join(OUT_DIR, "spectrum.png"))
    render_spectrogram(mat).save(os.path.join(OUT_DIR, "spectrogram.png"))
    print("OK:", os.path.abspath(OUT_DIR))


if __name__ == "__main__":
    main()
