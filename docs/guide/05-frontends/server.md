# 05 · openslate-server（库，经 `openslate serve` 拉起）：REST/WS 后端

> 面向接手本项目的 AI agent。共享**单会话** HTTP+WS 后端（web-1）。被 `openslate serve`
> 拉起（也可在测试里直接 `build_router` + 随机端口）。会话核心已抽到 **openslate-session**
> （传输无关，server WS 与 mobile FFI 共用），server 只做传输与组合根。

---

## 1. 模块地图（crates/openslate-server/src/）

| 文件 | 职责 |
|------|------|
| `lib.rs` | `ServeOptions`（L51）/ `build_state`（L75）/ `build_router`（L182）/ `serve()`（L194）/ `serve_with_shutdown`（L208）；`STDOUT_TARGET = "serve_stdout"`（L39，stdout fmt layer 只放行它） |
| `hub.rs` | `ConnectionHub`：WS fan-out——每连接一个 unbounded 队列 + 写任务，队列天然给出「同一连接内事件全序」；实现 session 的 `MsgSink` |
| `ws.rs` | `/api/ws` 生命周期：握手（hello）、注册 + snapshot 入队、每连接读写两任务、Ping→Pong |
| `rest.rs` | REST **只读**：`rest_router`（L111）挂 `GET /health` `/config` `/agents` `/skills` `/sessions`（L113-117）；写操作一律走 WS；`--auth-token` 时 `?token=` 校验 |
| `discovery.rs` | server.json 落盘/删除/防双 |

`state` / `engine` / `approval` / `config_ops` / `session` 实际住在 **openslate-session**
（`crates/openslate-session/src/`），`lib.rs:21` re-export 保持 `openslate_server::<module>`
路径可用。

## 2. ServeOptions 与默认值

```rust
pub struct ServeOptions {          // lib.rs:51-59
    pub bind: IpAddr,              // 默认 127.0.0.1（Default impl L61-71）
    pub port: u16,                 // 默认 7800
    pub auth_token: Option<String>,// --auth-token；None = hello 不校验 token
    pub config_flag: Option<String>,// CLI --config 直通（None = 发现链 global→local merge）
    pub provider_factory: Option<ProviderFactory>,  // 测试接缝：None = 生产工厂
}
```

- 日志：`~/.local/share/openslate/logs/server.log`（`init_logging`，与 tui.log 同规格）；
  stdout 单行事件走 `stdout_event!` 宏（L43-48，target = `serve_stdout`，英文文案
  grep 友好，**token / api key 值绝不进**）。

## 3. build_state 装配（lib.rs:75-171）

1. `build_app_context(opts.config_flag)` → 解构 `AppContext`（见 `app-wiring.md`）。
2. `ConnectionHub` + `SessionApprovalBridge::new(hub.clone())`（审批请求经 hub 广播给
   WS 客户端）。
3. `apply_approval(&mut manager, &config, /*interactive*/ true, /*yes*/ false)` +
   挂桥回调（server = 交互式语义：默认 `auto_except(["shell","run_code"])`）。
4. 组 `CoreInner`（session_id = uuid simple、`ConfigPaths{active, global, local}`：
   发现链里 active==global 说明无本地叠加 → local=None，否则 local=active——含
   `--config` 显式的情形，L110-123）→ `SessionCore` → `AppState`。
5. 生产 provider 工厂 = `Arc::new(openslate_app::build_provider_for_model)`
   （`default_provider_factory` L173）；测试经 `provider_factory` 注入 scripted 工厂。
6. `AppState { core, sink, approval, auth_token, provider_factory, origin: "server",
   _mcp }`——`_mcp: McpConnectionGuard` 字段名带下划线，纯生命周期持有（drop 序见
   `app-wiring.md`）。

## 4. 路由与 WS 生命周期

### 4.1 build_router（lib.rs:182-187）

```rust
Router::new()
    .route("/api/ws", get(ws::ws_handler))
    .nest("/api", rest::rest_router())
    .with_state(state)
```

### 4.2 ws.rs 握手（L58-109）

- **首条必须是 hello**（30s 超时防挂死连接，L60）：`ClientMsg::Hello { proto, token }`。
  - `proto != PROTOCOL_VERSION` → `proto_mismatch` 拒绝（文案带两侧版本）；
  - token 与 `state.auth_token` 不符 → `bad_token`；
  - 非 hello / 解析失败 → `bad_message`。
  拒绝路径统一：发 `ServerMsg::Error { code, message }` 后 close（`deny_and_close` L45）。
- **注册 + snapshot 在 core 锁内原子完成**（L111 起）：通道拓扑 hub 广播 → per-conn
  hub 队列 → forwarder → 写队列 → sink；snapshot 直入写队列（严格首条），其后事件经
  forwarder 保序——与引擎回调的「持锁广播」互斥，保证 snapshot 严格先于其后事件。
- 每连接读写两任务；Ping 由读任务转写任务回 Pong。

### 4.3 openslate-session（会话核心，改前必读 crate 文档）

`crates/openslate-session/src/`：`state`（`SessionCore`/`CoreInner`/`MsgSink` trait）、
`engine`（回合引擎任务 + pre-turn auto-compact）、`session`（`ClientMsg` 派发，传输无关）、
`approval`（`SessionApprovalBridge`）、`config_ops`（模型/provider 配置 CRUD 写回链）。

锁与顺序不变量：

- `SessionCore::inner` 是 **std Mutex**（`ProgressCallback` 是同步 trait）；
- 锁方向只有 `core → sink` / `core → approval bridge`，**严禁反向**（会与 snapshot
  构成环）；
- `RunManager` 用 `Option` 槽位表达「在家 / 引擎持有」两态。

## 5. discovery.rs（server.json 与防双）

- 常量 `SERVER_INFO_FILE` 在 openslate-protocol（单一真相源；tui 读同一份文件但不依赖
  server crate）。
- 监听成功后原子写 `ServerInfo`（tmp+rename，**0600 权限——token 在里面**）进配置目录
  （`resolve_info_dir` L153：active config 父目录，发现链同源）。字段：proto / url
  （`client_ws_url` L26，按 bind 算 ws:// 或 wss://… 实际给 client 的地址）/ pid / port /
  bind / started_at / token / cwd。
- **防双唯一判据 = health 探测**（`guard_against_running_server` L114，~1s，
  `health_url_from_ws` L38 从 server.json 的 ws url 推 health url）——**不做 pid 校验**
  （pid 复用会误判活服务器为死）。已有活实例 → 启动失败并报地址。
- 优雅停机末尾 best-effort 删除（`remove_server_info` L212）。
- `port 0` → 实际分配端口（server.json 必须写真值，lib.rs:228-229）。

## 6. serve() 停机序（spec §4 钉死）

Ctrl-C（`ctrl_c_signal` L200，注册失败降级为立即完成）触发：

1. deny 全部 pending 审批；
2. cancel 当前回合；
3. await 引擎任务；
4. 收尾 session run 落库；
5. 关闭全部 WS；
6. 删 server.json；
7. 退出。

`serve_with_shutdown`（L208）是测试接缝版（停机信号可注入）；server.json 的
落盘/删除与防双守卫**只在这里**（serve 家族唯一入口）。

## 7. 改动检查清单

- 动端点/消息 → `openslate-protocol` 是单一真相源；tui `to_tui` 与 mobile EventSink
  都要对齐。
- 动停机序 → 顺序是 spec 钉死的，别调换。
- 动防双 → health 探测（非 pid）语义两端（server 写 / tui 读）一致。
- 动会话逻辑 → 优先改 openslate-session（mobile 也共用），server 只动传输层。
