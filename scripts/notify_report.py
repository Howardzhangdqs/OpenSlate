#!/usr/bin/env python3
"""openslate-tui 任务进度通知：JSON 字段 -> HTML 邮件 -> 自动发送。

用法：
  scripts/notify_report.py <report.json>
标题：有进行中/排队中任务时 `[OPS] (进行中数/排队中数) <headline>`；两者皆空时 `[OPS] 任务全部完成`。
JSON 字段：
  headline    str            任务简述（进行中状态用于标题）
  subline     str?           题头下小字（默认当前时间）
  in_progress [{title,desc}] 进行中（title 必填，desc 一段话简述可空）
  queued      [str]          排队中（每项一行）
  recent      [str]          最近完成（建议 ≤5，每项一行；兼容传 {desc} 对象，id 不再展示）
  baseline    str            测试基线脚注（如 "281 passed · clippy 0 · fmt clean"）
SMTP 凭据运行时从 ~/.mailrc 的 mta= 解析；发送前强制自检并打印出站头。
"""
import html, json, re, ssl, smtplib, sys
from datetime import datetime
from email.message import EmailMessage
from pathlib import Path

RECIPIENT = '202483710036@nuist.edu.cn'
S = ('style="margin:0;padding:0;background:#f4f5f7;font-family:'
     "'Segoe UI','PingFang SC','Microsoft YaHei',sans-serif;\"")


def esc(x: str) -> str:
    return html.escape(str(x), quote=True)


def build_report(r: dict) -> str:
    sub = r.get('subline') or datetime.now().strftime('%Y-%m-%d · orchestrator 自动通知')
    recent = ''.join(
        f'<div>✔ {esc(i if isinstance(i, str) else i.get("desc", ""))}</div>'
        for i in r.get('recent', [])) or '<div>✔ 无</div>'
    cards = []
    for it in r.get('in_progress', []):
        desc = (f'<p style="color:#3c4043;font-size:13px;line-height:1.7;margin:8px 0 0 0;">'
                f'{esc(it.get("desc", ""))}</p>') if it.get('desc') else ''
        cards.append(
            '<div style="background:#f8f9fa;border:1px solid #e8eaed;border-radius:6px;'
            f'padding:14px 16px;margin-bottom:10px;"><div style="font-weight:600;'
            f'color:#202124;font-size:14px;">{esc(it["title"])}</div>{desc}</div>')
    queued = ''.join(f'<div>▫ {esc(q)}</div>'
                     for q in r.get('queued', [])) or '<div>▫ 无</div>'
    cards_html = ''.join(cards) or '<div style="font-size:13px;color:#3c4043;">▫ 无</div>'
    baseline = esc(r.get('baseline', ''))
    return f"""<!DOCTYPE html>
<html><body {S}>
<div style="max-width:640px;margin:24px auto;background:#ffffff;border-radius:8px;overflow:hidden;border:1px solid #e2e4e8;">
<div style="background:#1a2b3c;padding:18px 24px;">
  <div style="color:#8ab4f8;font-size:13px;letter-spacing:2px;">OPENSLATE-TUI</div>
  <div style="color:#ffffff;font-size:20px;font-weight:600;margin-top:4px;">任务进度通知</div>
  <div style="color:#9aa5b1;font-size:12px;margin-top:4px;">{esc(sub)}</div>
</div>
<div style="padding:20px 24px;">
  <div style="font-size:15px;font-weight:600;color:#1a73e8;border-left:4px solid #1a73e8;padding-left:10px;margin-bottom:10px;">🔄 进行中</div>
  <div style="margin-bottom:22px;">{cards_html}</div>
  <div style="font-size:15px;font-weight:600;color:#e8710a;border-left:4px solid #e8710a;padding-left:10px;margin-bottom:10px;">📋 排队中</div>
  <div style="margin-bottom:22px;font-size:13px;color:#3c4043;line-height:1.9;">{queued}</div>
  <div style="font-size:15px;font-weight:600;color:#188038;border-left:4px solid #188038;padding-left:10px;margin-bottom:10px;">✅ 最近完成</div>
  <div style="margin-bottom:22px;font-size:13px;color:#3c4043;line-height:1.9;">{recent}</div>
</div>
<div style="background:#f8f9fa;border-top:1px solid #e8eaed;padding:12px 24px;font-size:12px;color:#5f6368;">
  测试基线：{baseline} ｜ 全量任务记录见 .slim/deepwork/agent-tui.md
</div>
</div></body></html>"""


def build_subject(r: dict) -> str:
    n_ip, n_q = len(r.get('in_progress') or []), len(r.get('queued') or [])
    if n_ip or n_q:
        return f"[OPS] ({n_ip}/{n_q}) {r['headline']}"
    return '[OPS] 任务全部完成'


def main() -> None:
    r = json.loads(Path(sys.argv[1]).read_text(encoding='utf-8'))
    subject = build_subject(r)
    body = build_report(r)

    msg = EmailMessage()
    msg['Subject'] = subject
    cfg = Path.home().joinpath('.mailrc').read_text()
    m = re.search(r'set\s+mta="smtps://([^:"]+):([^@" ]+)@([^:" ]+):(\d+)"', cfg)
    user_enc, passwd, host, port = m.groups()
    msg['From'], msg['To'] = user_enc.replace('%40', '@'), RECIPIENT
    msg.set_content('（HTML 邮件，请用支持 HTML 的客户端查看）', charset='utf-8')
    msg.add_alternative(body, subtype='html', charset='utf-8')

    raw = msg.as_string()
    assert 'text/html' in raw and str(msg['Subject']) == subject
    print('--- outbound head ---')
    print('\n'.join(raw.splitlines()[:6]))

    with smtplib.SMTP_SSL(host, int(port), context=ssl.create_default_context()) as s:
        s.login(msg['From'], passwd)
        s.send_message(msg)
    print('SEND_OK')


if __name__ == '__main__':
    main()
