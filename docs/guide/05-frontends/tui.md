# 05 · openslate-tui（二进制 `openslate-tui`）：终端前端

> 面向接手本项目的 AI agent。TUI 是 **纯 server 客户端**（web-1 client edition）：
> 不建引擎上下文、不碰 SQLite；一切真相在 server（snapshot），本地只留 config 镜像
> 做展示决策。

---

## 1. 定位与依赖钉子

- 不依赖 openslate-app / openslate-server（纯客户端）；只依赖 openslate-protocol
  （ClientMsg/ServerMsg/DTO）。
- **ratatui `=0.30.2` 精确锁定**（workspace Cargo.toml L57-63）：补丁带 CJK 宽单元格
  尾随样式修复（上游 #2585）；crossterm `0.29`（+`event-stream` feature）必须与
  ratatui 后端版本对齐，否则双事件队列 / 终端状态损坏。**升级 ratatui 前先确认补丁
  是否进了上游**。
- 二进制 `main.rs` 是薄壳（CLI 参数、文件日志、panic hook、连 server、终端 init/restore），
  可测逻辑全在 lib（integration test 不能 import bin target）。

## 2. 启动/退出序（main.rs）

1. clap（L52-70 附近）：
   - `--server`：优先级 **flag > env `OPENSLATE_SERVER` > server.json 自动发现**。
     发现链在 `discovery.rs`：`./.openslate/` → `~/.config/openslate/`，取第一个可用文件；
     JSON 损坏或 proto 版本不符 → WARN 跳过。
   - `--server-token` 只从 env 读（`OPENSLATE_TOKEN`，不进 shell history）。
   - `--config` 只作用于**本地显示偏好**（`[tui]` / `[tui.icons]` 段，GAP-2）。
2. 文件 tracing 到 `~/.local/share/openslate/logs/tui.log`（`resolve` L134 附近；
   启动时截断超 1MiB 的旧文件）——客户端/服务端链路**唯一**调试窗口。
3. panic hook（`install_panic_hook` L218）：同步恢复终端（raw mode/alternate screen 的
   undo 存了裸指针态，hook 可无条件调用）后再链回原 hook——release 是 `panic=abort`，
   Drop 恢复不可靠。
4. 连 server：初连阶梯 0.5s/1s/2s ×3，耗尽 exit 1。
5. raw mode + alternate screen + bracketed paste（`enter` L185 附近）→ `App::run`。
6. 退出序对称：App teardown（link drop）→ 终端恢复 → 日志 flush → stdout 一行摘要
   （session_id / turns / cost，`SessionSummary`）。

## 3. lib 模块与冻结接口（P2b）

| 模块 | 职责 |
|------|------|
| `client.rs` | `ServerLink`（生产 `WsLink` = WS 写队列；测试 `MemLink`）；`to_tui`（L70）——**单一** `ServerMsg → TuiEvent` 纯转换（保序：WS 帧序即事件序）；`connect`（L527）传输任务 |
| `app.rs` | `App` 聚合 + `ClientBootstrap{link, events, config, root_agent_id}`（main 交给 App 的种子）+ `SessionSummary{run_id, turns, total_cost_usd}`；action 派发主循环 |
| `action.rs` / `event.rs` | `Action` + `map_event`（冻结接口）；`TuiEvent` / `TurnSummary` |
| `components/` | `Component` trait + `AppCtx`（冻结）；八组件 |
| `panel.rs` / `theme.rs` / `icons.rs` | 圆角覆盖框、主题（`Theme`/`ThemeMode`）、图标字形 |
| `md.rs` | markdown → 行渲染（CJK 宽度安全） |
| `slash.rs` / `complete.rs` / `clipboard.rs` / `discovery.rs` | `/` 命令、补全、剪贴板 sink、server.json 发现链 |

八组件：`transcript`（转写）/ `input` / `status` / `approval`（审批应答）/
`agents` / `models`（模型/provider 管理，**经 WS 写操作**）/ `session` / `help`；
焦点按 `Focus` Tab 轮换。

### 3.1 连接与重连阶梯（client.rs）

- `LADDER_MS: [u64; 3] = [500, 1000, 2000]`（L499）。
- `connect(url, token)`（L527）：**初连**阶梯 0.5s/1s/2s，耗尽返回硬错误（main exit 1），
  错误文案「无法连接服务器（0.5s/1s/2s 重试均失败），请检查 --server 地址与服务器状态」；
  返回点 = 首份 snapshot 到达（link up + hello accepted）。
- **运行时掉线重连**：同一阶梯循环不止（不退出）；每次 (重)连发 `hello`（ClientMsg，
  带 proto + token），成功即整份 snapshot 重建状态。ready 前的断线算初连失败、
  ready 后的算重连（L572 附近注释）。
- 用户输入在重连期间直接丢弃（排队在重连后面是错的语义，L581 附近）。

### 3.2 `to_tui` 与协议转换

- `to_tui` 是唯一的 ServerMsg 解释器（`entries_from_dto` L142、`config_from_view` L199、
  `view_from_config` L250 / `snapshot_from_config` L316 为 hello-snapshot 测试/引导构造器）。
- 审批应答：`approval_choice_to_msg`（L363）/ `approval_choice_label`（L374）。
- server URL 归一化：`normalize_server_url`（L392，容忍 ws:// / http:// / host:port 拼写）。

### 3.3 transcript rebuild 的 reasoning 映射（存储 v5 L6 修复）

`components/transcript.rs` 的 `entry_from_message`（rebuild 路径）：assistant 消息携带
非空 `reasoning_content` 时，在 ToolCall / Assistant 条目**之前**补一条
`TranscriptEntry::Reasoning`（空串不产生条目）——与 mobile 侧共享投影
（`openslate_protocol::project_from_messages`）语义对齐。修复前 rebuild 丢弃持久化的
思维链（merge 路径保留、rebuild 路径丢失的不一致）；测试
`rebuild_restores_reasoning_content_entries`。TUI 的 `TranscriptEntry` 是自己的枚举，
与 protocol `EntryDto` 的完全统一仍是 follow-up（设计 §4 记档）。

## 4. openslate-protocol（server/tui 共享协议，也是 mobile FFI 的载荷）

`crates/openslate-protocol/src/lib.rs`：

- `PROTOCOL_VERSION`（现 v4，见 02-core/openslate-protocol.md §1）；`ClientMsg` / `ServerMsg` 枚举——**写操作与 hello 走
  ClientMsg；snapshot/事件流/审批请求走 ServerMsg**。
- 全套 DTO：`SnapshotDto` / `EntryDto` / `ConfigViewDto`…；
- `SERVER_INFO_FILE`（server.json 文件名常量，**单一真相源**，server 写 / tui 读两端共用）；
- 裁剪常量如 `APPROVAL_SUMMARY_CLIP_CHARS`。
- **mobile FFI 复用同一对 ClientMsg/ServerMsg**（JSON 序列化过 UniFFI）——协议三端一份。

## 5. 改动检查清单

- 动 TUI 冻结接口（`Action` / `TuiEvent` / `Component` / `App` 公共面）→ P2b 冻结契约，
  扩展方式见 `lib.rs` 注释。
- 动 server 端点/消息 → `openslate-protocol` 单一真相源；tui 的 `to_tui` 与 mobile 的
  EventSink 序列化都要跟着对齐。
- 升级 ratatui/crossterm → 先看 workspace 注释（补丁 #2585、版本对齐）。
- 动 server.json 语义 → server（写）/ tui（读）两端一致：health 探测、无 pid 校验
  （见 `server.md` §discovery）。
