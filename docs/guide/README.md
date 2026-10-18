# OpenSlate 项目文档（docs/guide/）

本目录是 OpenSlate 的权威知识库，面向**接手项目的 AI agent**。结构：每章一个文件夹，每个主题一个文件。写法约定：给出具体文件路径、关键类型名、函数签名、源码路径:行号、限额与默认值；文档与代码冲突时，以代码为准修正文档。

## 文档索引

### 01-architecture/ — 整体架构（新接手必读）

| 文件 | 内容 |
|------|------|
| [overview.md](01-architecture/overview.md) | 项目定位、14 crate 一览、技术栈 |
| [crate-layers.md](01-architecture/crate-layers.md) | 依赖分层图、分层红线规则、每层职责 |
| [domain-concepts.md](01-architecture/domain-concepts.md) | 15 个核心领域概念（Provider/Agent/Run/Tool/Approval/Session…）定义与生命周期 |
| [traits-and-abstractions.md](01-architecture/traits-and-abstractions.md) | 8 个接缝型 trait 完整签名与实现者清单 |
| [dataflow.md](01-architecture/dataflow.md) | 端到端数据流：submit → 引擎循环 → 存储/展示，审批/取消/持久化横切时序 |

### 02-core/ — 核心与数据层

| 文件 | 内容 |
|------|------|
| [openslate-core.md](02-core/openslate-core.md) | core 全部 20 个模块：类型、API、边界条件 |
| [openslate-protocol.md](02-core/openslate-protocol.md) | 线格式、ClientMsg/ServerMsg 全变体、DTO 原则、契约测试红线 |
| [openslate-session.md](02-core/openslate-session.md) | session 五模块、CoreInner、锁序不变量、测试接缝 |
| [openslate-store-sqlite.md](02-core/openslate-store-sqlite.md) | 8 张表逐表 DDL、迁移机制、连接策略、RunRecorder 生命周期 |

### 03-models/ — 模型接入层

| 文件 | 内容 |
|------|------|
| [model-genai.md](03-models/model-genai.md) | genai 适配层、vendor/genai 补丁机制、错误映射与重试 |
| [model-registry.md](03-models/model-registry.md) | 三源元数据 registry：合并策略、归一化匹配、ZSTD 缓存、在线兜底 |
| [capability-detection.md](03-models/capability-detection.md) | /models 自动检测、鉴权矩阵、能力绑定 |

### 04-tools/ — 工具与扩展

| 文件 | 内容 |
|------|------|
| [mcp-host.md](04-tools/mcp-host.md) | Termux 聚合宿主：连接管理、token 鉴权、重连策略 |
| [mcp-builtin.md](04-tools/mcp-builtin.md) | 内置工具 server（fs/shell/edit/skill）与各限额 |
| [ask-user.md](04-tools/ask-user.md) | ask_user 提问工具：schema/下发契约、问卷持久化与 TTL、迟到答案等价不变量、secret 凭据接缝 |
| [ptc.md](04-tools/ptc.md) | QuickJS 沙箱、ToolCallMode/Disclosure、PtcLimits、安全模型 |
| [adding-a-tool.md](04-tools/adding-a-tool.md) | 手把手新增内置工具（含测试模板与检查清单） |

### 05-frontends/ — 前端与服务入口

| 文件 | 内容 |
|------|------|
| [cli.md](05-frontends/cli.md) | CLI 子命令全 flag 表、REPL |
| [tui.md](05-frontends/tui.md) | 纯 WS 客户端、八组件、重连阶梯 |
| [server.md](05-frontends/server.md) | REST/WS 路由、握手拒绝码、停机序 |
| [app-wiring.md](05-frontends/app-wiring.md) | AppContext / build_app_context 九步装配、drop 序不变量 |

### 06-mobile/ — Android 移动端

| 文件 | 内容 |
|------|------|
| [openslate-mobile.md](06-mobile/openslate-mobile.md) | Rust 组合层八模块详解 |
| [mobile-ffi.md](06-mobile/mobile-ffi.md) | UniFFI 完整暴露面、类型映射、panic 防线、再生成流程 |
| [android-app.md](06-mobile/android-app.md) | android/ 工程：gradle、Manifest、Kotlin 文件角色表 |
| [call-chains.md](06-mobile/call-chains.md) | 四条调用链时序、分层超时表、MCP host 子链 |

### 07-build-dev/ — 构建与开发

| 文件 | 内容 |
|------|------|
| [toolchain.md](07-build-dev/toolchain.md) | Rust/NDK/cargo-android 配置、16KB 页对齐 |
| [build-and-test.md](07-build-dev/build-and-test.md) | cargo 构建测试、mobile profile（panic=unwind）、vendor patch |
| [android-build.md](07-build-dev/android-build.md) | 交叉编译脚本解析、APK 构建安装、logcat |
| [troubleshooting.md](07-build-dev/troubleshooting.md) | 19 条构建/运行问题：症状/原因/解法 |
| [reference-repos.md](07-build-dev/reference-repos.md) | `.ref/` 参考项目源码：浅克隆、更新脚本、不入库 |

### 08-conventions/ — 代码与协作约定（改动前必读）

| 文件 | 内容 |
|------|------|
| [ui.md](08-conventions/ui.md) | Material 图标硬规范与基线组件 |
| [compat.md](08-conventions/compat.md) | API 26~37 兼容与版本判断写法 |
| [ffi-rules.md](08-conventions/ffi-rules.md) | FFI 边界八条硬约定 |
| [errors-and-logging.md](08-conventions/errors-and-logging.md) | 错误处理分工、超时预算总表、日志分层 |
| [style.md](08-conventions/style.md) | 命名、模块组织、注释风格 |
| [doc-maintenance.md](08-conventions/doc-maintenance.md) | 文档维护硬规则与变更→文档映射表 |

## 阅读顺序建议

**第一次接手的 agent：**

1. **本文件** → `01-architecture/overview.md` → `01-architecture/crate-layers.md` → `01-architecture/dataflow.md`（先建立全局图景）
2. `08-conventions/` 全部（协作红线，改动前必知）
3. 按任务涉及模块读对应章节，不必通读。

**按任务类型入口：**

- 改核心执行/存储/协议 → `02-core/`
- 加模型/Provider、模型元数据 → `03-models/`
- 加工具/MCP/PTC → `04-tools/`（先读 adding-a-tool.md）
- 改 CLI/TUI/server/装配 → `05-frontends/`
- 改 Android 端 → `06-mobile/`（先读 dataflow）
- 构建/装包/交叉编译问题 → `07-build-dev/`（先查 troubleshooting.md）

**补充材料**（非 guide 目录，但常被引用）：

- `AGENTS.md`（仓库根）：测试设备信息（权威来源）、模型元数据数据源可达性结论。
- `PTC_PLAN.md`（仓库根）：PTC 设计文档。
- `docs/PLAN-phase1.md`：mobile 第一阶段计划。
- 各 crate 源码内的 `//!` 模块注释：设计决策的第一手记录（本 guide 大量提炼自它们；细节冲突时以注释 + 代码为准）。
