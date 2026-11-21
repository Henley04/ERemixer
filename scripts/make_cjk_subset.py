#!/usr/bin/env python3
"""从完整 Noto Sans CJK 字体生成 GUI 用的中文子集（大幅缩小体积）。

界面里所有文案都写在 Rust 源码中（含 mixer-core 的阶段名/错误信息），
因此直接把 `crates/**/*.rs` 里出现过的非 ASCII 字符收集起来做子集，
既不会漏字，也不会把整个 20 MB 的字体塞进二进制。

用法：
    python scripts/make_cjk_subset.py
    python scripts/make_cjk_subset.py --font /usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_FONT = Path("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc")
DEFAULT_OUT = ROOT / "crates" / "mixer-app" / "assets" / "NotoSansCJK-subset.ttf"

# 界面里用到的符号/标点（部分不在源码里直接出现，或需要固定保留）。
EXTRA = (
    "　！＂＃％＆（）＊＋，－．／：；＜＝＞？＠［］｛｝｜～"
    "、。〈〉《》「」『』【】〔〕・…‥—–‘’“”※"
    "①②③④⑤⑥⑦⑧⑨⓪✓✗⚠●○◉◆■□▶►→←↑↓±×÷≈≠≤≥°′″µΩ"
    "⚡🎛🎚🎵🔊📁💾⏱⏸⏹"
)


def collect_chars() -> set[str]:
    chars: set[str] = {chr(c) for c in range(0x20, 0x7F)}
    chars.update(EXTRA)
    for path in (ROOT / "crates").rglob("*.rs"):
        for ch in path.read_text(encoding="utf-8", errors="ignore"):
            if ord(ch) > 0x7F:
                chars.add(ch)
    # 去掉控制字符
    return {c for c in chars if c.isprintable() or c == " "}


def pick_face(path: Path, prefer: str = "SC"):
    from fontTools.ttLib import TTCollection, TTFont

    if path.suffix.lower() not in (".ttc", ".otc"):
        return TTFont(str(path))
    coll = TTCollection(str(path))
    for font in coll.fonts:
        names = {
            rec.toUnicode()
            for rec in font["name"].names
            if rec.nameID in (1, 4, 6, 16, 17)
        }
        if any(prefer in n for n in names):
            return font
    print(f"! 未找到含 '{prefer}' 的字面，退回第 0 个", file=sys.stderr)
    return coll.fonts[0]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--font", default=str(DEFAULT_FONT))
    ap.add_argument("--out", default=str(DEFAULT_OUT))
    ap.add_argument("--prefer", default="SC", help="TTC 中优先选择的字面（如 SC/TC/JP）")
    args = ap.parse_args()

    from fontTools import subset

    font_path, out_path = Path(args.font), Path(args.out)
    if not font_path.exists():
        raise SystemExit(
            f"缺少字体 {font_path}\n"
            "  Debian/Ubuntu: sudo apt-get install fonts-noto-cjk\n"
        )

    chars = collect_chars()
    print(f"字符数：{len(chars)}")

    opts = subset.Options()
    opts.layout_features = ["*"]
    opts.name_IDs = ["*"]
    opts.notdef_outline = True
    opts.recalc_bounds = True
    opts.drop_tables += ["DSIG"]

    font = pick_face(font_path, args.prefer)
    sub = subset.Subsetter(options=opts)
    sub.populate(text="".join(sorted(chars)))
    sub.subset(font)

    out_path.parent.mkdir(parents=True, exist_ok=True)
    font.save(str(out_path))
    print(f"✓ {out_path}  ({out_path.stat().st_size / 1048576:.2f} MB)")

    # 缺失字形自检：CJK 缺字会导致界面豆腐块，必须为 0；
    # emoji（CLI 终端提示用的 ⚠✗ 等）Noto CJK 本身没有，仅告警。
    cmap = font.getBestCmap()
    missing = sorted(c for c in chars if ord(c) not in cmap and c != " ")
    cjk_missing = [
        c for c in missing
        if 0x3000 <= ord(c) <= 0x303F or 0x4E00 <= ord(c) <= 0x9FFF
        or 0xFF00 <= ord(c) <= 0xFFEF or 0x3400 <= ord(c) <= 0x4DBF
    ]
    if missing:
        print(f"! 字体无字形（非 CJK，可忽略）：{''.join(missing)}", file=sys.stderr)
    if cjk_missing:
        print(f"✗ 缺少 CJK 字形：{''.join(cjk_missing)}", file=sys.stderr)
        return 1
    print("✓ CJK 字符全部有字形")
    return 0


if __name__ == "__main__":
    sys.exit(main())
