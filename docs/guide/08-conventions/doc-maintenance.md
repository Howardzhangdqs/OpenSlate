# 08-conventions · 文档维护硬规则

> 与根 `AGENTS.md` 呼应的硬性规则。`docs/guide/` 是本项目的权威知识库——**改完代码必须同步更新文档**。

## 1. 核心规则

1. **任何影响文档所载内容的代码变更，必须在同一次任务中更新 `docs/guide/` 对应文档**（映射表见 §3）。
2. 文档与代码冲突时：**以代码为准修正文档**，不要让过时文档误导后续 agent。
3. 新增文档文件时，同步更新 `docs/guide/README.md` 索引。
4. 接手任务时**先读文档**（`README.md` 索引 → 对应章节），确认既有设计与约定再动手，避免重复探索和破坏既有决策。

## 2. 章节结构（2026-10 起：每章一个文件夹）

`docs/guide/` 采用「章文件夹 + 主题文件」结构；本章（08-conventions）已拆分为：[ui.md](ui.md)（UI 规范）、[compat.md](compat.md)（API 兼容）、[ffi-rules.md](ffi-rules.md)（FFI 硬约定）、[errors-and-logging.md](errors-and-logging.md)（错误与日志）、[style.md](style.md)（命名与注释风格）、本文（文档维护）。01～05 章与 README.md 的结构以其当前实际形态为准（索引见 `README.md`）。

## 3. 变更 → 文档映射表

| 变更内容 | 必须更新 |
|---------|---------|
| 新增/删除 crate、crate 间依赖变化 | `01-architecture`（架构章） |
| core / protocol / session / 存储层的 API、数据模型、行为变化 | `02-core`（核心与数据章） |
| 模型接入、registry 数据源/合并策略、能力检测变化 | `03-models`（模型章） |
| MCP / PTC 协议或内置工具变化 | `04-tools`（工具扩展章） |
| cli / tui / server / app 入口或行为变化 | `05-frontends`（桌面前端章） |
| **mobile 组合层**（MobileRuntime / EventSink / HostCallRouter / ExecSelection 等八模块） | `docs/guide/06-mobile/openslate-mobile.md` |
| **UniFFI 边界**（暴露面 / 类型映射 / panic 防线 / 泵线程契约 / 再生成流程） | `docs/guide/06-mobile/mobile-ffi.md` |
| **android 工程**（gradle 三件套 / Manifest / debug 源集 / Kotlin 文件角色 / 状态管理） | `docs/guide/06-mobile/android-app.md` |
| **调用链语义**（上行消息 / 下行事件 / host call 往返 / 生命周期时序与超时） | `docs/guide/06-mobile/call-chains.md` |
| **工具链**（Rust / NDK / ~/.cargo-android / ndk-stub / 16KB 对齐 / SDK 路径） | `docs/guide/07-build-dev/toolchain.md` |
| **本地构建与测试**（cargo 命令 / profile 语义 / vendor 测试 / 迭代回路） | `docs/guide/07-build-dev/build-and-test.md` |
| **Android 构建脚本**（build-android.sh / uniffi-gen.sh / build-mcp-host.sh / build-apk.sh / adb / logcat / 广播注入 / 变更→产物矩阵） | `docs/guide/07-build-dev/android-build.md` |
| **新出现的构建/运行故障模式** | `docs/guide/07-build-dev/troubleshooting.md`（对照表追加条目） |
| **UI 图标规范基线变化** | `docs/guide/08-conventions/ui.md` |
| **API 兼容写法新范例 / min-target 变化** | `docs/guide/08-conventions/compat.md` |
| **FFI 边界新增/修改硬约定** | `docs/guide/08-conventions/ffi-rules.md` |
| **错误处理模式 / 日志 tag / 超时预算变化** | `docs/guide/08-conventions/errors-and-logging.md` |
| **命名 / 模块组织 / 注释风格约定变化** | `docs/guide/08-conventions/style.md` |
| **文档规则本身 / 映射表调整** | 本文 |

## 4. 领域知识补充（权威优先级）

- 测试设备信息、UI 图标规范、API 兼容区间、模型 registry 数据源策略 → 根 `AGENTS.md`（**权威**）；
- mobile 阶段计划（Phase 1 闭环、已知限制）→ `docs/PLAN-phase1.md` 与 `android/README.md`；
- PTC 设计 → `PTC_PLAN.md`。

## 5. 写作规范（对齐现有文档风格）

- 面向**接手项目的 AI agent**：给出具体文件路径（必要时带 `:行号`）、关键类型名、数据流向与模块职责边界；命令给全文；限额/默认值给数值。
- 引用其他章节用相对链接（如 `[../07-build-dev/toolchain.md](../07-build-dev/toolchain.md)`）；引用根文件用仓库相对路径。
- 行号会漂移：作为定位辅助而非精确锚点，与类型名/函数名一起给。
