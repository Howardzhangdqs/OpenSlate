# 08-conventions · 错误处理与日志

> 从源码提炼。分工原则：内部实现用 `anyhow`，对外枚举错误用 `thiserror`；时序敏感的等待一律带超时。

## 1. Rust：anyhow / thiserror 分工

- **内部实现**：`anyhow::Result` + `.context()`（错误链带中文上下文，跨 FFI 时以 `{e:#}` 全链打印进 reason）。
- **对外枚举错误**：`thiserror`——FFI `MobileError`（`CreateFailed{reason}` / `SendFailed{reason}`）、protocol 层错误。
- FFI 错误保持**单一 Kotlin catch 类型**：分类只有两档，粒度靠 reason 文本（`err_dup` 克隆填值，见 [ffi-rules.md](ffi-rules.md) §1）。
- **Best-effort 分层**：非关键路径失败只告警不阻断（如 `realign_database_path` 迁移失败 warn 放弃、`wal_checkpoint` 失败 non-fatal、启动清扫失败 non-fatal）——关键路径（装配、DB 指针解析）才上抛。写法：`if let Err(e) = … { crate::alog!("… failed (non-fatal): {e}"); }`。

## 2. Kotlin：可判别结果与错误三态

- 桥层对可预期失败返回**可判别结果**，不静默吞：
  - `SecretStore.KeyLoadResult` 三态：`Missing`（未存储）/ `Ok(value)` / `Failed(error)`——**Failed 必须与 Missing 区分**，不得混作「未配置」；解密失败在 UI 显式展示「需重新录入」（`RuntimeBridge.start` 的恢复循环与 `KeysScreen` 均遵守）。
  - `deleteSession` 返回 false 需覆盖「当前活动会话不可删」等语义并在 UI 说明；`setApiKeyAndPersist` 返回 false = 持久化失败（内存注入仍完成，本回合可用但重启丢失），调用方不得置「已配置」。
- FFI 抛出的 `MobileError` 在 Kotlin 是单一异常类型，`reason` 文本直接给 UI 展示。

## 3. 优雅收尾（graceful shutdown）

- `AgentService.onDestroy` 用 `runBlocking { withTimeoutOrNull(2_500) { RuntimeBridge.shutdown() } }` **同步等**——lifecycleScope 协程会被 DESTROYED 竞态截断；宁可短暂阻塞主线程也要让 Rust 侧 run 终态（`finish("interrupted")`）与 SQLite 收尾做完。
- Rust `MobileRuntime::shutdown` 顺序：fail_all host call → deny_all 审批 → close_all 连接 → **run 落库终态** → `shutdown_timeout(5s)`（见 [../06-mobile/call-chains.md](../06-mobile/call-chains.md) §4.3）。
- 会话切换同样收束旧 run（`open_session`：旧 run finish 为 "interrupted"；同 run 重开不收束）。

## 4. 超时预算总表（不允许无限等）

| 场景 | 上限 | 定义处 |
|------|------|--------|
| host call 等宿主应答 | **120s**（`HOST_CALL_TIMEOUT`） | `crates/openslate-mobile/src/hostcall.rs:48` |
| Termux RUN_COMMAND 结果 | **90s**（`RESULT_TIMEOUT_MS`） | `bridge/TermuxExec.kt:47` |
| provider 模型检测（/models） | 10s | `runtime.rs` `list_provider_models` |
| registry 手动更新 | 120s | `runtime.rs` `registry_update_source_json` |
| registry 在线兜底单查 | 10s | `runtime.rs` `lookup_model_meta_json` |
| native bash 命令默认 / 上限 | 30s / 120s（`DEFAULT_TIMEOUT_MS` / `MAX_TIMEOUT_MS`） | `crates/openslate-mobile/src/exec.rs:29-31` |
| Rust runtime 关停收束 | 5s（`shutdown_timeout`） | `runtime.rs:409` |
| Kotlin 等 shutdown | 2.5s（`AgentService.onDestroy`） | `service/AgentService.kt:51` |
| LLM 单请求挂死防护 | 60s 无新数据即断（流级） | 引擎层（见配置注释 `limits.timeout_ms`） |

新增任何时序敏感等待必须带上限并填入本表。

## 5. 日志规范

### 5.1 tag 分层

| tag | 来源 | 用法 |
|-----|------|------|
| `OpenSlateRust` | Rust `alog!` 宏（`TAG`，`crates/openslate-mobile/src/alog.rs:39`） | Rust 侧阶段打点（Info 级）、panic hook（Error 级 `PANIC: …`）、raw usage 诊断 |
| `OpenSlateBridge` | Kotlin 桥层（`RuntimeBridge` 等） | 装配 / 事件泵 / 恢复日志 |
| `OpenSlateTermux` | `bridge/TermuxExec.kt` 等 | Termux 执行器 |

新增日志沿用对应 tag；`adb logcat -s OpenSlateRust OpenSlateBridge` 组合过滤。

### 5.2 内容型日志的 debug 门控

- 事件内容（可含对话/工具参数）的日志必须 `BuildConfig.DEBUG` 门控且采样（RuntimeBridge 事件泵：前 20 条 + 每 100 条）——**release 不剥离 logcat**，防泄漏。
- `alog!` 是 Info 级阶段打点（`alog.rs:42-47`）；desktop 上为空操作，测试零噪音。

### 5.3 panic 外显

- Android 上 `install_panic_hook`（Once 幂等）把 panic 打到 logcat 再走默认 hook——tokio spawn 会吞 panic，不装 hook 表现为无声的「空回合」。
