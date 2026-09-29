#!/usr/bin/env python3
"""OpenSlate 开屏字体画廊：每页 5 款轮流播放，挑中记下名字告诉 orchestrator。

用法:  python3 scripts/font_gallery.py
依赖:  pyfiglet（已装入设备共用 venv：uv pip install pyfiglet）
过滤:  纯 ASCII 字形、宽 ≤72、高 ≤10、相同渲染去重（别名只留首名）
键位:  Enter=下一页 · b=上一页 · q=退出
"""
import os
import string

import pyfiglet

ASCII = set(string.printable[:95])
MAXW, MAXH, PER_PAGE = 72, 10, 5


def render(font: str):
    """返回 (lines, w, h) 或 None（超尺寸/非 ASCII/渲染失败）。"""
    try:
        art = pyfiglet.figlet_format("OpenSlate", font=font)
    except Exception:
        return None
    lines = art.rstrip("\n").split("\n")
    while lines and not lines[0].strip():
        lines.pop(0)
    while lines and not lines[-1].strip():
        lines.pop()
    lines = [l.rstrip() for l in lines]
    if not lines:
        return None
    w = max(len(l) for l in lines)
    h = len(lines)
    if w > MAXW or h > MAXH:
        return None
    if any(not set(l) <= ASCII for l in lines):
        return None
    return lines, w, h


def main() -> None:
    seen, cands = set(), []
    for f in sorted(pyfiglet.FigletFont.getFonts()):
        r = render(f)
        if r is None:
            continue
        lines, w, h = r
        key = "\n".join(lines)
        if key in seen:
            continue
        seen.add(key)
        cands.append((f, w, h, lines))

    total_pages = (len(cands) + PER_PAGE - 1) // PER_PAGE
    page = 0
    while True:
        os.system("clear")
        chunk = cands[page * PER_PAGE:(page + 1) * PER_PAGE]
        print(
            f"OpenSlate 字体画廊 · 第 {page + 1}/{total_pages} 页 · "
            f"共 {len(cands)} 款（纯 ASCII ≤{MAXW}x{MAXH}，已去重）\n"
        )
        for i, (name, w, h, lines) in enumerate(chunk):
            idx = page * PER_PAGE + i + 1
            print(f"### {idx}/{len(cands)} · {name} ({w}x{h}) ###")
            for l in lines:
                print(l)
            print()
        try:
            cmd = input(
                f"[Enter]=下一页  b=上一页  q=退出 · "
                f"挑中就记下名字（第 {page + 1}/{total_pages} 页）> "
            ).strip().lower()
        except (EOFError, KeyboardInterrupt):
            break
        if cmd == "q":
            break
        elif cmd == "b":
            page = max(0, page - 1)
        else:
            page = min(total_pages - 1, page + 1)


if __name__ == "__main__":
    main()
