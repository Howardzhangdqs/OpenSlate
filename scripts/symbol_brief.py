#!/usr/bin/env python3
"""symbol_brief.py —— 文件符号大纲（Agent 代码定位辅助）

用法:
    python3 scripts/symbol_brief.py <文件> [文件...]

    *.rs  → rust-analyzer (LSP documentSymbol)：层级符号树，精确起止行
    *.kt  → ast-grep：函数/类/对象/接口声明清单，起止行

输出行格式: L<起>-<止>  <类型>  <名字>（Rust 带层级缩进）

依赖:
    .rs: rust-analyzer（rustup component add rust-analyzer）
    .kt: ast-grep >= 0.51（https://ast-grep.github.io，单二进制）
"""
import subprocess, json, os, sys, re, shutil, time

# ---------- Rust: rust-analyzer headless LSP ----------

RA_KINDS = {
    2: "module", 5: "class", 6: "method", 8: "field", 9: "ctor",
    10: "enum", 11: "iface", 12: "fn", 13: "var", 14: "const",
    22: "enummember", 23: "struct", 26: "typeparam",
}

def rust_brief(path):
    ra = shutil.which("rust-analyzer") or os.path.expanduser("~/.cargo/bin/rust-analyzer")
    if not os.path.exists(ra):
        sys.exit("未找到 rust-analyzer，先执行: rustup component add rust-analyzer")
    root = subprocess.run(["git", "rev-parse", "--show-toplevel"],
                          cwd=os.path.dirname(os.path.abspath(path)),
                          capture_output=True, text=True).stdout.strip() or os.getcwd()
    proc = subprocess.Popen([ra], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL)
    _id = 0
    def send(obj):
        data = json.dumps(obj).encode()
        proc.stdin.write(f"Content-Length: {len(data)}\r\n\r\n".encode() + data)
        proc.stdin.flush()
    def req(method, params):
        nonlocal _id
        _id += 1
        send({"jsonrpc": "2.0", "id": _id, "method": method, "params": params})
        return _id
    def notify(method, params):
        send({"jsonrpc": "2.0", "method": method, "params": params})
    def read_one(timeout=120):
        deadline = time.time() + timeout
        headers = b""
        while b"\r\n\r\n" not in headers:
            if time.time() > deadline:
                raise TimeoutError("rust-analyzer 响应超时")
            ch = proc.stdout.read(1)
            if not ch:
                raise EOFError("rust-analyzer 退出（检查 rustup 组件是否完整）")
            headers += ch
        n = int(next(l for l in headers.decode().split("\r\n")
                     if l.startswith("Content-Length")).split(":", 1)[1])
        body = b""
        while len(body) < n:
            body += proc.stdout.read(n - len(body))
        return json.loads(body)
    def wait_for(rid, timeout=180):
        deadline = time.time() + timeout
        while True:
            m = read_one(max(1, deadline - time.time()))
            if m.get("id") == rid and "result" in m:
                return m["result"]
    try:
        req("initialize", {
            "processId": os.getpid(), "rootUri": "file://" + root,
            "workspaceFolders": [{"uri": "file://" + root, "name": os.path.basename(root)}],
            "capabilities": {"textDocument": {"documentSymbol": {
                "hierarchicalDocumentSymbolSupport": True}}},
        })
        while True:
            m = read_one()
            if "id" in m and m["id"] == 1:
                break
        notify("initialized", {})
        uri = "file://" + os.path.abspath(path)
        notify("textDocument/didOpen", {"textDocument": {
            "uri": uri, "languageId": "rust", "version": 1,
            "text": open(path, encoding="utf-8", errors="replace").read()}})
        rid = req("textDocument/documentSymbol", {"textDocument": {"uri": uri}})
        syms = wait_for(rid)
    finally:
        proc.terminate()
    lines = []
    def walk(nodes, depth):
        for s in nodes:
            r = s.get("range") or s.get("location", {}).get("range", {})
            sl = r.get("start", {}).get("line", 0) + 1
            el = r.get("end", {}).get("line", 0) + 1
            kind = RA_KINDS.get(s.get("kind", 0), "?")
            lines.append(f"{'  ' * depth}L{sl}-{el}  {kind:9s} {s.get('name', '?')}")
            walk(s.get("children") or [], depth + 1)
    walk(syms, 0)
    return lines

# ---------- Kotlin: ast-grep 模式枚举 ----------

KT_PATTERNS = [
    ("fun",     "fun $NAME($$$) { $$$ }"),
    ("fun",     "fun $NAME($$$) = $$$"),
    ("class",   "class $NAME($$$) { $$$ }"),
    ("class",   "class $NAME($$$)"),
    ("class",   "class $NAME { $$$ }"),
    ("class",   "data class $NAME($$$)"),
    ("class",   "data class $NAME($$$) { $$$ }"),
    ("class",   "internal class $NAME($$$) { $$$ }"),
    ("class",   "private class $NAME($$$) { $$$ }"),
    ("class",   "internal class $NAME { $$$ }"),
    ("object",  "object $NAME { $$$ }"),
    ("object",  "internal object $NAME { $$$ }"),
    ("object",  "private object $NAME { $$$ }"),
    ("object",  "companion object { $$$ }"),
    ("iface",   "interface $NAME { $$$ }"),
    ("enum",    "enum class $NAME { $$$ }"),
]

def kotlin_brief(path):
    sg = shutil.which("ast-grep")
    if not sg:
        sys.exit("未找到 ast-grep，安装: https://ast-grep.github.io/guide/install.html")
    seen = {}
    for dtype, pat in KT_PATTERNS:
        r = subprocess.run([sg, "run", "-p", pat, "-l", "kotlin", "--json=compact", path],
                           capture_output=True, text=True)
        try:
            matches = json.loads(r.stdout)
        except json.JSONDecodeError:
            continue
        for m in matches:
            line = m["range"]["start"]["line"] + 1
            if line in seen:
                continue
            name = (m.get("metaVariables", {}).get("single", {}).get("NAME", {}).get("text")
                    or re.sub(r"\(.*", "", m["text"].strip().split("\n")[0]).split()[-1])
            seen[line] = (line, m["range"]["end"]["line"] + 1, dtype, name)
    return [f"L{sl}-{el}  {dtype:9s} {name}"
            for sl, el, dtype, name in sorted(seen.values())]

def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    for path in sys.argv[1:]:
        if not os.path.exists(path):
            print(f"!! 文件不存在: {path}", file=sys.stderr)
            continue
        ext = os.path.splitext(path)[1]
        print(f"===== {path} =====")
        if ext == ".rs":
            lines = rust_brief(path)
        elif ext == ".kt":
            lines = kotlin_brief(path)
        else:
            print(f"!! 不支持的类型 {ext}（仅 .rs/.kt）", file=sys.stderr)
            continue
        for l in lines:
            print(l)

if __name__ == "__main__":
    main()
