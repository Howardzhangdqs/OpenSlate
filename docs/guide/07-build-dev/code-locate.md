# 代码快速定位工具（symbol_brief + ast-grep + LSP）

> 建立时间：2026-10-23。背景：对 OpenCode 会话轨迹的分析（244 会话、2598 次 read）显示，
> 子 agent 约 33% 的 read 调用耗在 12 个热点文件（runtime.rs / RuntimeBridge.kt / SettingsScreen.kt 等）
> 的反复翻页定位上。本工具链用「符号→行号」地图替代盲目翻页，`read` 直奔目标窗口。

## 工具组成

| 工具 | 覆盖 | 角色 | 安装 |
|---|---|---|---|
| `scripts/symbol_brief.py` | .rs / .kt | 文件符号大纲（统一入口） | 项目自带，依赖下两项 |
| rust-analyzer | .rs | LSP `documentSymbol`，层级+精确起止行 | `rustup component add rust-analyzer` |
| ast-grep | .kt（+.rs 模式搜索） | tree-sitter AST 模式匹配 | [单二进制安装](https://ast-grep.github.io/guide/install.html)，≥0.51，命令名 `ast-grep`（`sg` 已弃用） |

未采用：JetBrains kotlin-lsp（1.2GB + EULA + 冷启动 ~3s + AGP experimental，只适合未来常驻代码智能服务）、
fwcd/kotlin-language-server（已休眠、JDK 25 崩溃、内存无界增长）、universal-ctags（无结束行号，精度不足）。
选型依据是 2026-10-23 的实测对比（延迟/精度/内存/健壮性），结论：Kotlin 走 ast-grep，Rust 走 LSP。

## 场景与命令

### 1. 文件符号大纲（接手文件任务的第一步）

```bash
python3 scripts/symbol_brief.py <文件> [文件...]
```

- `.rs` → rust-analyzer headless LSP：输出层级大纲（struct 字段、impl 块→方法），精确起止行
- `.kt` → ast-grep 模式枚举：fun / class / object / interface / enum 声明清单 + 起止行
- 输出示例：`L800-802  fun  setApiKey` → 后续 `read` 用 `offset=800 limit=40` 精确开窗

### 2. 找符号定义 / 找调用点（ast-grep 模式搜索）

```bash
# Rust 定义——⚠ 可见性是语法节点的一部分：fn 匹配不到 pub fn，需分别匹配
ast-grep run -p 'pub fn $NAME($$$) -> $RET { $$$ }' -l rust crates/
ast-grep run -p 'fn $NAME($$$) { $$$ }' -l rust crates/
ast-grep run -p 'pub struct $NAME $$$' -l rust crates/
ast-grep run -p 'pub trait $NAME { $$$ }' -l rust crates/
# 调用点（AST 级，不命中注释/字符串同名文本）
ast-grep run -p '$OBJ.send($$$)' -l rust crates/
# Kotlin——fun 不受 private/suspend/override 修饰符影响；class/object 必须把修饰符写进模式
ast-grep run -p 'fun $NAME($$$) { $$$ }' -l kotlin android/
ast-grep run -p 'fun $NAME($$$) = $$$' -l kotlin android/
ast-grep run -p 'data class $NAME($$$)' -l kotlin android/
ast-grep run -p 'internal object $NAME { $$$ }' -l kotlin android/
# JSON 输出（拿精确起止区间；行号 0 基，使用时 +1）
ast-grep run -p '<模式>' -l <rust|kotlin> --json=compact <路径>
```

默认输出为 `file:line: 匹配文本`（人类可读，适合快速浏览）；`--json=compact` 结构：
`range.start.line` / `range.end.line`（0 基）、`metaVariables.single.NAME.text`（捕获名）。

## 模式语法与坑

- `$VAR`：单节点捕获；`$$$`：多节点通配（参数列表/函数体/语句块）
- **模式必须是完整合法代码**：函数模式必须带函数体，`fn $NAME($$$)` 裸签名不匹配
- Rust：`fn` ≠ `pub fn`（可见性是节点一部分）；`-> $RET` 与无返回值是两种形态
- Kotlin：`fun` 对修饰符免疫（`private fun`/`suspend fun` 都能被裸 `fun` 模式命中），
  但 `class`/`object` 带修饰符（`internal`/`data`/`private`）必须写进模式
- 已知限制：极少数 Kotlin 文件 tree-sitter 有解析缺口（实测 `RuntimeBridge.kt` 的顶层
  `object RuntimeBridge` 节点匹配不到，字面量模式也不行），此时大纲仍能列出全部 fun/class，
  顶层 object 名可用 grep 兜底
- `symbol_brief.py` 的 Rust 分支需要文件位于 git 工作区内（脚本以 git root 作 LSP workspace root）

## 维护

- 新语言的outline需求：在 `scripts/symbol_brief.py` 的 `KT_PATTERNS`/`RA_KINDS` 同层扩展；
  pattern 集合变更前先用合成样本文件验证修饰符覆盖
- 本文档与 AGENTS.md「代码快速定位」章节同步维护；命令变更两处都要改
