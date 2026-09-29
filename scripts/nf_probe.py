#!/usr/bin/env python3
"""Nerd Font 终端覆盖探测：打印 openslate-tui nerd 档实际使用的码点 + 各区样本。

用法：python3 scripts/nf_probe.py
在你「看 TUI 的那个终端」里跑，逐行看输出：
  - 看到 图标形状        -> 该码点正常（你的字体覆盖）
  - 看到 生僻汉字        -> 回退到了 CJK 字体（PUA 被中文映射吃掉）
  - 看到 空白/豆腐框 □   -> 无任何字体覆盖该码点
结论对照：
  - 第一区「openslate nerd 档」全正常 -> --icons nerd 可用
  - 仅 cod 区异常 -> 告诉我，可把 brand/branch/thinking 换回 fa 区码点
  - 全部变汉字/豆腐 -> 终端未配 Nerd Font，用 unicode 档或装字体
"""
import sys

# (hex, 槽位名, 应该看到什么) —— 码点以 hex 字符串承载，运行时 chr() 还原，
# 源码保持纯 ASCII（PUA 字面量过编辑通道会损坏）。
TUI_NERD = [
    ("EC59", "thinking", "思考云：云朵+左下两颗拖尾小圆点"),
    ("EC4F", "brand", "对话气泡+右上角星芒"),
    ("EC6F", "branch", "git 分支：竖干线+上下两枚节点圆"),
    ("F105", "prompt", "细单箭头 ›"),
    ("F111", "anchor", "实心圆 ●"),
    ("F0C1", "delegate", "锁链环（链环图形）"),
    ("F013", "bullet", "齿轮"),
    ("F10C", "pending", "空心圆 ○"),
    ("F00C", "check", "对勾 ✓"),
    ("F00D", "cross", "叉 ×"),
    ("F071", "warn", "三角内感叹号"),
    ("F29F", "diamond", "实心菱形 ◆"),
    ("F4BF", "phase", "空心菱形 ◇"),
    ("F1CE", "starting", "带缺口的圆环"),
    ("F140", "waiting", "同心圆靶心 ◉"),
    ("F04B", "agents_running", "实心三角播放键 ▶"),
    ("F04D", "stopped", "实心方块 ■"),
    ("F149", "steer", "折角下箭头 ⤵"),
    ("F0E7", "zap", "闪电 ⚡"),
    ("F062", "up", "向上实心箭头"),
    ("F063", "down", "向下实心箭头"),
    ("F061", "right", "向右实心箭头"),
    ("F060", "left", "向左实心箭头"),
    ("F05E", "blocked", "禁止圈（圆圈内斜杠）"),
]

EXTRAS = [
    ("E28C", "fae-brain (候选1)", "大脑（FAE 老区）——思维链行图标候选"),
    ("EE9C", "fa-brain (候选2)", "大脑（FA6 新区，大概率缺）"),
    ("F0C2", "fa-cloud (兜底)", "云朵=思考气泡兜底（FA4 老区必覆盖）"),
    ("EA61", "cod-lightbulb", "灯泡"),
    ("EA77", "cod-sync", "双箭头圆环"),
    ("EB19", "cod-loading", "开口圆弧"),
    ("EC10", "cod-sparkle", "双星描边"),
    ("EC21", "cod-sparkle_filled", "实心四角星"),
    ("EC20", "cod-robot", "机器人头"),
    ("F418", "oct-git_branch", "旧版 git 分支"),
    ("E0A0", "pl-branch", "powerline 细线分支钩"),
    ("E0B0", "pl-right", "powerline 右三角分隔符"),
    ("F0D0", "fa-magic", "魔法棒+星芒"),
]

NON_BMP = [
    ("F0004", "md-account", "增补 PUA（非 BMP）人形"),
    ("F024B", "md-folder", "增补 PUA 文件夹"),
]


def show(title, rows):
    print(f"\n=== {title} ===")
    for cp, name, expect in rows:
        g = chr(int(cp, 16))
        print(f"  {g}  U+{cp:<5s} {name:<22s} 应为: {expect}")


def main():
    print("Nerd Font 终端覆盖探测 —— 逐行核对「字形列」与你看到的形状是否一致")
    show("① openslate-tui nerd 档实际使用的 24 个码点（重点看这区）", TUI_NERD)
    show("② 各区补充样本（fa/cod/oct/powerline）", EXTRAS)
    show("③ 增补 PUA 探针（非 BMP，openslate 未用，仅测终端宽度/覆盖行为）", NON_BMP)
    print("\n判读：图标=正常 / 生僻汉字=CJK 回退吃掉 PUA / □ 或空白=无覆盖")
    print("①全正常 -> --icons nerd 直接可用；仅部分区异常 -> 反馈异常码点，我调整选码")


if __name__ == "__main__":
    try:  # 非 UTF-8 locale 下强制 UTF-8 输出；老版本无此方法则跳过
        sys.stdout.reconfigure(encoding="utf-8")  # type: ignore[attr-defined]
    except AttributeError:
        pass
    main()
