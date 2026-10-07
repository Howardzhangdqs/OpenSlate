//! OpenSlate mobile runtime 的 UniFFI 边界（proc-macro 模式）。
//!
//! 暴露面与 PLAN §5-§6 一一对应：
//! - `create(paths, callback)` → 装配 + 首份 snapshot（create 即握手）
//! - `send(client_msg_json)` → 复用 ClientMsg/ServerMsg 协议语义
//! - `set_api_key(provider, value)` → 内存注入（宿主负责 Keystore 持久化）
//! - `resolve_host_call(id, ok, payload)` → host tool request/resolve
//! - `shutdown()` → host call fail-all + 审批全拒 + runtime 收束
//! - `list_sessions(offset)` / `open_session` / `delete_session` → 历史会话
//! - `set_mcp_host_token(token)`（namespace 函数）→ MCP host 鉴权注入
//!
//! 线程约定：所有方法立即返回（send 内部 spawn；shutdown 有限等待）；
//! `callback.on_event` 从 Rust 泓线程调用——宿主实现必须线程安全、
//! 尽快返回（重活转投宿主队列）。
//!
//! Panic 防线：workspace release 是 panic=abort，本 crate 以 `mobile`
//! profile（panic=unwind）构建，且每个入口 catch_unwind → Err(String)，
//! 绝不让 Rust panic 穿过 FFI 边界。

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use openslate_mobile::{MobilePaths, MobileRuntime, RuntimeOptions};

// uniffi 0.32 proc-macro 模式：命名空间 + UniFfiTag + FFI 脚手架。
uniffi::setup_scaffolding!("openslate_mobile");

/// Android 宿主注入的真实路径（全部必须可写；Rust 不做路径发现）。
#[derive(uniffi::Record)]
pub struct MobilePathsDto {
    pub config_dir: String,
    pub workspace_dir: String,
    pub data_dir: String,
    pub cache_dir: String,
}

/// 宿主事件回调：每条事件 = 一个 JSON 对象（ServerMsg 或
/// `host_call_requested` 信封；`type` 字段区分，snake_case 命名空间不重叠）。
#[uniffi::export(callback_interface)]
pub trait EventCallback: Send + Sync {
    fn on_event(&self, event: String);
}

/// FFI 错误（uniffi 要求错误为枚举 + Display，不能是裸 String）。
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum MobileError {
    /// runtime 装配失败（目录 bootstrap / AppContext 构建）。
    #[error("create failed: {reason}")]
    CreateFailed { reason: String },
    /// 消息发送失败（无法解析 / runtime 已关停）。
    #[error("send failed: {reason}")]
    SendFailed { reason: String },
}

/// FFI 对象（包一层 MobileRuntime，避免 mobile crate 依赖 uniffi）。
#[derive(uniffi::Object)]
pub struct OpenSlateRuntime {
    inner: Arc<MobileRuntime>,
}

fn catch<T>(label: &str, err: MobileError, f: impl FnOnce() -> anyhow::Result<T>) -> Result<T, MobileError> {
    std::panic::catch_unwind(AssertUnwindSafe(f)).map_err(|p| {
        let panic_text = p
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_owned());
        let reason = format!("{label}: rust panic: {panic_text}");
        tracing::error!("{reason}");
        err_dup(&err, reason)
    })
    .and_then(|r| r.map_err(|e| err_dup(&err, format!("{label}: {e}"))))
}

/// 把 reason 填进同类错误的克隆（保持 Kotlin 侧单一 catch 类型）。
fn err_dup(err: &MobileError, reason: String) -> MobileError {
    match err {
        MobileError::CreateFailed { .. } => MobileError::CreateFailed { reason },
        MobileError::SendFailed { .. } => MobileError::SendFailed { reason },
    }
}

#[uniffi::export]
impl OpenSlateRuntime {
    /// 装配 runtime：磁盘 bootstrap → 会话核心 → 首份 snapshot 事件。
    #[uniffi::constructor]
    pub fn create(
        paths: MobilePathsDto,
        callback: Box<dyn EventCallback>,
    ) -> Result<Self, MobileError> {
        openslate_mobile::alog!("ffi create: entry");
        let result = catch(
            "create",
            MobileError::CreateFailed { reason: String::new() },
            || {
                let mobile_paths = MobilePaths {
                    config_dir: paths.config_dir.into(),
                    workspace_dir: paths.workspace_dir.into(),
                    data_dir: paths.data_dir.into(),
                    cache_dir: paths.cache_dir.into(),
                };
                // UniFFI 0.32 回调接口以 Box<dyn> 抬升；转 Arc 进 runtime。
                let cb = Arc::new(UniffiCallbackAdapter(callback));
                let runtime =
                    MobileRuntime::create_with(mobile_paths, cb, RuntimeOptions::default())
                        .map_err(|e| anyhow::anyhow!("{e:#}"))?;
                openslate_mobile::alog!("ffi create: success");
                Ok(Self { inner: runtime })
            },
        );
        match &result {
            Ok(_) => {}
            Err(e) => openslate_mobile::alog!("ffi create: FAILED: {e}"),
        }
        result
    }

    /// 发送 ClientMsg（JSON 文本）。非阻塞（内部 spawn）。
    pub fn send(&self, msg: String) -> Result<(), MobileError> {
        catch(
            "send",
            MobileError::SendFailed { reason: String::new() },
            || {
                self.inner.send(&msg).map_err(|e| anyhow::anyhow!("{e:#}"))
            },
        )
    }

    /// 注入 provider API key（内存；不落盘）。
    pub fn set_api_key(&self, provider: String, value: String) {
        self.inner.set_api_key(&provider, &value);
    }

    /// 设置 HTTP(S) 出站代理（受限网络：宿主可经 adb reverse 共享电脑
    /// 代理）。进程级、立即生效于后续 provider 构建；空串清除。
    pub fn set_http_proxy(&self, url: String) {
        // NOTE: 进程级 env；约定由宿主主线程在回合间调用（2021 edition
        // 中 set_var 为 safe；workspace 尚未迁 2024）。
        if url.is_empty() {
            std::env::remove_var("OPENSLATE_HTTP_PROXY");
            openslate_mobile::alog!("ffi: http proxy cleared");
        } else {
            std::env::set_var("OPENSLATE_HTTP_PROXY", &url);
            openslate_mobile::alog!("ffi: http proxy set to {url}");
        }
    }

    /// 设置 bash 工具后端多选（逗号分隔："native" / "termux" /
    /// "native,termux"）。热生效：单选 → 工具名 `bash`；双选 →
    /// `bash`（native）+ `termux_bash`（termux）。
    pub fn set_exec_backends(&self, backends: String) {
        self.inner.set_exec_backends(&backends);
    }

    /// 应答 host call。`ok=true` → payload 为结果 JSON；`ok=false` →
    /// payload 为错误说明。返回 false = 无此在途调用（超时/已答/关停）。
    pub fn resolve_host_call(&self, id: u64, ok: bool, payload: String) -> bool {
        self.inner.resolve_host_call(id, ok, payload)
    }

    /// 历史会话列表（JSON 数组：id/title/status/started_ms/cost_usd）。
    /// 分页：每页 50 条，offset 递增（0、50、100…；首页传 0）。
    pub fn list_sessions(&self, offset: u32) -> String {
        openslate_mobile::alog!("ffi: list_sessions(offset={offset})");
        self.inner.list_sessions_json(offset)
    }

    /// 拉取 provider 的可用模型清单（设置页 Provider 域"自动检测"）。
    /// 返回 `{"ok":true,"models":[...]}` 或 `{"ok":false,"error":"..."}`
    /// （密钥未配置 / 网络 / 协议错误）。含 10s 超时，宿主须在 IO 线程调用。
    pub fn list_provider_models(&self, provider: String) -> String {
        openslate_mobile::alog!("ffi: list_provider_models(provider={provider})");
        self.inner.list_provider_models(provider)
    }

    // ── 模型元数据注册表（数据源页面 / 元数据自动补全）────────────

    /// 数据源列表 + 各源本地状态（条目数 / 更新时间 / 体积）。
    pub fn registry_sources(&self) -> String {
        openslate_mobile::alog!("ffi: registry_sources");
        self.inner.registry_sources_json()
    }

    /// 手动更新一个数据源（阻塞，最长 120s；IO 线程调用）。
    /// 返回 `{"ok":true,"entries":N}` 或 `{"ok":false,"error":"..."}`。
    pub fn registry_update_source(&self, id: String) -> String {
        openslate_mobile::alog!("ffi: registry_update_source(id={id})");
        self.inner.registry_update_source_json(id)
    }

    /// 启动时按需自动更新（后台执行；本地缺失或过期才拉取）。
    pub fn registry_schedule_auto_update(&self) {
        openslate_mobile::alog!("ffi: registry_schedule_auto_update");
        self.inner.registry_schedule_auto_update();
    }

    /// 本地条目搜索 / 浏览（数据源页面「查看条目 / 跨源搜索」）。纯
    /// 本地（首次会读盘解压，宿主建议 IO 线程调用）。`source_id` 空 =
    /// 跨全部已缓存源搜索；`query` 空 = 浏览模式（前 limit 条，键字
    /// 典序）；`limit <= 0` 按 50 处理。返回
    /// `{"total":N,"results":[{source,sourceId,id,ctx,out,vision,reasoning,tool,priceIn,priceOut}]}`：
    /// `total` = 匹配总数（分页计数），`results` 截断到 limit；`id`
    /// 为源 JSON 原始键（保留 provider 前缀）；数值 / 能力 / 计价字段
    /// 源没给为 null。未知 source_id → `{"total":0,"results":[]}`。
    pub fn registry_search(&self, source_id: String, query: String, limit: i32) -> String {
        openslate_mobile::alog!(
            "ffi: registry_search(source_id={source_id:?}, query={query:?}, limit={limit})"
        );
        self.inner.registry_search_json(source_id, query, limit)
    }

    /// 模型元数据查询（本地优先，miss 时在线兜底；IO 线程调用）。
    pub fn lookup_model_meta(&self, model_id: String) -> String {
        openslate_mobile::alog!("ffi: lookup_model_meta(model_id={model_id})");
        self.inner.lookup_model_meta_json(model_id)
    }

    /// 当前会话 run id（无进行中会话返回 null）。
    pub fn current_run_id(&self) -> Option<String> {
        self.inner.current_run_id()
    }

    /// 切换到指定历史会话（成功后回推新 snapshot 事件）。
    pub fn open_session(&self, run_id: String) -> Result<(), MobileError> {
        openslate_mobile::alog!("ffi: open_session {run_id}");
        self.inner
            .open_session(run_id)
            .map_err(|e| MobileError::SendFailed {
                reason: format!("open_session: {e:#}"),
            })
    }

    /// 删除历史会话（连同全部消息/步骤数据）。返回是否删除成功：
    /// false = 拒绝或失败——当前活动会话不可删（先新建会话切换走）；
    /// run 不存在 / 数据库错误也返回 false。Kotlin 宿主负责同步删除
    /// 自己的 transcript 文件（Rust 只管数据库行）。
    pub fn delete_session(&self, run_id: String) -> bool {
        openslate_mobile::alog!("ffi: delete_session {run_id}");
        self.inner.delete_session(run_id)
    }

    /// 优雅关停（幂等）。
    pub fn shutdown(&self) {
        self.inner.shutdown();
    }
}

/// 注入 MCP host 鉴权 token（进程级，对所有 HTTP MCP server 生效：
/// 请求头加 `Authorization: Bearer <token>`，toml 不必持久化 token）。
///
/// **必须在 `create` 之前调用**——MCP 连接在装配期建立，之后注入不
/// 影响已建连接。namespace 级函数（非对象方法）：它配置的是进程而非
/// 某个 runtime 实例。
#[uniffi::export]
pub fn set_mcp_host_token(token: String) {
    openslate_mobile::alog!("ffi: set_mcp_host_token ({} bytes)", token.len());
    openslate_mobile::set_mcp_host_token(&token);
}

/// UniFFI foreign callback → mobile `EventCallback`（Box 装进适配器，
/// 生命周期由 runtime 侧 Arc 持有）。
struct UniffiCallbackAdapter(Box<dyn EventCallback>);

impl openslate_mobile::EventCallback for UniffiCallbackAdapter {
    fn on_event(&self, event_json: String) {
        // FFI 边界最薄处：不做任何解析，直接转发字符串。
        self.0.on_event(event_json);
    }
}
