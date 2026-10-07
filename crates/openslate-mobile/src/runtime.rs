//! `MobileRuntime`：FFI 边界的组合根（create / send / set_api_key /
//! resolve_host_call / shutdown）。
//!
//! 组装 = `build_app_context_with`（显式配置路径 + host 工具注入）+
//! server `build_state` 同款的状态装配（core + sink + 审批桥），origin
//! 标注 `"mobile"`。事件全序经单条 [`EventSink`] 泵线程送达宿主回调。
//!
//! Hello 语义：`create()` 本身即握手（进程内单客户端，无需网络握手），
//! 创建即推送首份 snapshot；`send(Hello)` 校验协议版本后补发新
//! snapshot。其余消息全部走 [`openslate_session::session::dispatch`]。

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use openslate_app::wiring::{apply_approval, build_app_context_with, AppContext};
use openslate_core::approval::ApprovalManager;
use openslate_core::tool::Tool;
use openslate_core::types::{Message, MessageRole};
use openslate_protocol::{
    ClientMsg, EntryDto, NoticeLevel, ServerMsg, ToolStatusDto, PROTOCOL_VERSION,
};
use openslate_session::approval::SessionApprovalBridge;
use openslate_session::state::{
    build_snapshot, AppState, ConfigPaths, CoreInner, ProviderFactory, SessionCore, MsgSink,
};
use openslate_session::session as session_dispatch;

use crate::bootstrap::MobilePaths;
use crate::events::{EventCallback, EventSink, MOBILE_CONN_ID};
use crate::exec::{register_bash_tools, ExecSelection, ExecSelectionCell};
use crate::hostcall::{HostCallRouter, HostTool, HOST_CALL_TIMEOUT};
use crate::provider::{mobile_provider_factory, MobileSecrets};

/// 组装选项（FFI 层填默认值；测试注入 scripted provider / 额外 host 工具）。
pub struct RuntimeOptions {
    /// None = mobile secret factory（set_api_key 注入 + env 兜底）。
    pub provider_factory: Option<ProviderFactory>,
    /// 额外 host 工具（默认含 `mobile.ping` 演示工具，真机连通用）。
    pub extra_tools: Vec<Box<dyn Tool>>,
    pub host_call_timeout: Duration,
    /// 启动保留策略：装配期只保留最近 N 个 run，其余连同子表数据删除
    /// （默认 100，防历史库无限膨胀；0 = 清空，生产不建议）。
    pub run_retention: i64,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            provider_factory: None,
            extra_tools: Vec::new(),
            host_call_timeout: HOST_CALL_TIMEOUT,
            run_retention: 100,
        }
    }
}

/// mobile 运行时（FFI 对象）。所有方法非阻塞（除 `shutdown` 有限等待）。
pub struct MobileRuntime {
    /// `shutdown` 需要拿走 Runtime 收束（shutdown_timeout 消费所有权）；
    /// 单次取用 + 幂等标志。
    rt: std::sync::Mutex<Option<tokio::runtime::Runtime>>,
    state: Arc<AppState>,
    sink: Arc<EventSink>,
    secrets: Arc<MobileSecrets>,
    host_router: Arc<HostCallRouter>,
    /// bash 工具注册表（设置页多选热换名）。
    registry: Arc<openslate_core::tool::ToolRegistry>,
    /// 模型元数据注册表（数据源页面 / 元数据自动补全）。
    model_registry: Arc<crate::registry::RegistryStore>,
    /// bash 工具工作区路径（重注册时用）。
    workspace_dir: PathBuf,
    /// bash 后端多选状态（设置页热切换；bash / termux_bash 命名规则）。
    exec_selection: ExecSelectionCell,
    shutdown_flag: std::sync::atomic::AtomicBool,
}

impl MobileRuntime {
    /// 默认装配（生产入口：FFI 调这个）。
    pub fn create(
        paths: MobilePaths,
        callback: Arc<dyn EventCallback>,
    ) -> Result<Arc<Self>> {
        Self::create_with(paths, callback, RuntimeOptions::default())
    }

    /// 完整装配（测试/定制入口）。
    pub fn create_with(
        paths: MobilePaths,
        callback: Arc<dyn EventCallback>,
        opts: RuntimeOptions,
    ) -> Result<Arc<Self>> {
        crate::alog::install_panic_hook();
        crate::alog::install_raw_usage_logger();
        crate::alog!("create_with: entry (config_dir={:?})", paths.config_dir);
        let config_path = paths.ensure_layout()?;
        // DB 路径对齐（宿主迁移 data_dir 后，把 [database] path 与旧库
        // 文件一并归位，避免新旧数据分叉）；best-effort，失败仅告警。
        if let Err(e) = paths.realign_database_path() {
            crate::alog!("create_with: realign_database_path failed: {e}");
        }
        crate::alog!("create_with: layout ensured, config={:?}", config_path);
        let sink = EventSink::new(callback);
        crate::alog!("create_with: event sink started");
        let host_router = HostCallRouter::new(sink.clone(), opts.host_call_timeout);

        // host/本地工具集：mobile.ping（演示）+ bash 工具（按设置多选动
        // 态注册：单选 → bash；双选 → bash=native + termux_bash）+ 注入项。
        let exec_selection = ExecSelectionCell::new(ExecSelection::default());
        let mut extra_tools = opts.extra_tools;
        extra_tools.push(Box::new(HostTool::new(
            "mobile.ping",
            "Ping the OpenSlate mobile host bridge. Returns {'pong': true, 'runtime': 'rust'}. No parameters.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            host_router.clone(),
        )));
        // bash 工具不进 extra_tools（命名随设置变化）：先注册默认
        // native-only，装配完成后由 set_exec_backends 按偏好重注册。

        let secrets = Arc::new(MobileSecrets::default());
        let factory = opts
            .provider_factory
            .unwrap_or_else(|| mobile_provider_factory(secrets.clone()));
        // 模型元数据注册表（数据源页面 / 自动补全；本地 ZSTD 持久化）。
        let model_registry = Arc::new(crate::registry::RegistryStore::new(&paths.config_dir));

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("openslate-mobile")
            .enable_all()
            .build()
            .context("创建 tokio runtime 失败")?;
        crate::alog!("create_with: tokio runtime built, assembling state…");

        let state = rt.block_on(assemble_state(
            config_path,
            sink.clone(),
            factory,
            extra_tools,
            opts.run_retention,
        ))?;
        crate::alog!("create_with: state assembled, emitting first snapshot");
        // 拿到运行期注册表并按当前（默认）多选注册 bash 工具。
        let runtime_registry = state.core.lock().manager.as_ref()
            .expect("manager present").tool_registry.clone();
        register_bash_tools(
            &runtime_registry,
            exec_selection.get(),
            paths.workspace_dir.clone(),
            host_router.clone(),
        );
        let runtime = Arc::new(Self {
            rt: std::sync::Mutex::new(Some(rt)),
            state,
            sink,
            secrets,
            host_router: host_router.clone(),
            registry: runtime_registry.clone(),
            model_registry,
            workspace_dir: paths.workspace_dir.clone(),
            exec_selection: exec_selection.clone(),
            shutdown_flag: std::sync::atomic::AtomicBool::new(false),
        });

        // 首份 snapshot（create 即握手）。
        runtime.emit_snapshot();
        Ok(runtime)
    }

    /// 发送一条 `ClientMsg`（JSON 文本）。解析失败返回错误（不产生事件）。
    pub fn send(&self, msg_json: &str) -> Result<()> {
        if self.shutdown_flag.load(Ordering::SeqCst) {
            anyhow::bail!("runtime 已关闭");
        }
        crate::alog!("send: {:?} bytes", msg_json.len());
        let msg: ClientMsg = serde_json::from_str(msg_json)
            .with_context(|| format!("无法解析的 ClientMsg: {msg_json}"))?;
        // 机制化封禁 set_api_key 协议消息：协议层的实现会把密钥明文写
        // 入 config_dir/.env（mobile 的存储区不受 Keystore 保护，等于
        // 密钥落盘）。此前靠"宿主自觉不发该消息"的约定约束，这里把
        // 约定变成机制——无论谁发，一律拒绝并广播错误，绝不进
        // dispatch。mobile 的正道是 FFI set_api_key（内存注入，持久化
        // 由宿主 Keystore 负责）。
        if matches!(msg, ClientMsg::SetApiKey { .. }) {
            self.sink.broadcast(ServerMsg::Error {
                code: "set_api_key_forbidden".into(),
                message: "mobile 不支持 set_api_key 协议消息（会把密钥明文写入 .env）；请使用 FFI set_api_key 注入"
                    .into(),
            });
            return Ok(());
        }
        match msg {
            ClientMsg::Hello { proto, .. } => {
                if proto != PROTOCOL_VERSION {
                    self.sink.broadcast(ServerMsg::Error {
                        code: "proto_mismatch".into(),
                        message: format!(
                            "协议版本不匹配：宿主 {proto}，运行时 {PROTOCOL_VERSION}"
                        ),
                    });
                    return Ok(());
                }
                self.emit_snapshot();
            }
            other => {
                let state = self.state.clone();
                // 从 Option<Runtime> 借出 Handle spawn（不移动 runtime 本体）。
                let handle = {
                    let guard = self.rt.lock().expect("mobile runtime lock poisoned");
                    let rt = guard.as_ref().expect("runtime already shut down");
                    rt.handle().clone()
                };
                handle.spawn(async move {
                    session_dispatch::dispatch(&state, MOBILE_CONN_ID, other).await;
                });
            }
        }
        Ok(())
    }

    /// 注入 provider API key（内存；宿主负责 Keystore 持久化，PLAN §18）。
    pub fn set_api_key(&self, provider: &str, value: &str) {
        self.secrets.set(provider, value);
    }

    /// 设置 bash 工具后端多选（逗号分隔："native"、"termux"、
    /// "native,termux"；热生效，无需重启）。命名规则：
    /// 单选 → `bash`；双选 → `bash`=native + `termux_bash`=termux。
    pub fn set_exec_backends(&self, backends: &str) {
        let sel = ExecSelection::parse_csv(backends).normalized();
        self.exec_selection.set(sel);
        register_bash_tools(&self.registry, sel, self.workspace_dir.clone(), self.host_router.clone());
        crate::alog!(
            "exec backends set to {backends:?} (native={}, termux={})",
            sel.native,
            sel.termux
        );
    }

    /// 宿主应答 host call。`ok=true` → `payload` 为结果 JSON；
    /// `ok=false` → `payload` 为错误说明。返回 false = 无此在途调用。
    pub fn resolve_host_call(&self, id: u64, ok: bool, payload: String) -> bool {
        self.host_router.resolve(id, ok, payload)
    }

    /// 会话统计快照（宿主诊断用）。
    pub fn stats(&self) -> openslate_protocol::SessionStatsDto {
        self.state.core.lock().stats
    }

    /// 优雅关停：host call 全部失败 + 审批全拒 + 连接关闭 + runtime 收束。
    /// 有限等待（最多 ~5s），残余任务随 runtime drop 强制终止。幂等。
    pub fn shutdown(&self) {
        use std::sync::atomic::Ordering;
        if self.shutdown_flag.swap(true, Ordering::SeqCst) {
            return; // 幂等
        }
        crate::alog!("shutdown: begin");
        self.host_router.fail_all("runtime shutting down");
        self.state.approval.deny_all();
        self.sink.close_all();
        // run 终态必须先于 runtime 收束落库：此刻不 finish，recorder 随
        // runtime 一起消失后该 run 永远停留在 running（僵尸记录，只能
        // 等下次启动的 mark_running_runs_interrupted 清扫）。关停语义 =
        // 被打断 → "interrupted"。错误仅告警，不阻断关停。
        {
            let (session_run, cost) = {
                let mut inner = self.state.core.lock();
                (inner.session_run.take(), inner.stats.total_cost_usd)
            }; // 先放锁再 block_on：不能持锁等异步。
            if let Some(run) = session_run {
                let handle = {
                    let guard = self.rt.lock().expect("mobile runtime lock poisoned");
                    // 幂等标志已挡住重入；rt 必然还在（take 在本块之后）。
                    guard
                        .as_ref()
                        .expect("runtime already shut down")
                        .handle()
                        .clone()
                };
                if let Err(e) = handle.block_on(run.recorder.finish("interrupted", None, cost)) {
                    crate::alog!("shutdown: finish run as interrupted failed (non-fatal): {e}");
                }
            }
        }
        let rt = self
            .rt
            .lock()
            .expect("mobile runtime lock poisoned")
            .take();
        if let Some(rt) = rt {
            rt.shutdown_timeout(Duration::from_secs(5));
        }
    }

    fn emit_snapshot(&self) {
        let snap = {
            let inner = self.state.core.lock();
            build_snapshot(&inner, &self.state.approval)
        };
        self.sink.send_to(
            MOBILE_CONN_ID,
            ServerMsg::Snapshot {
                session: Box::new(snap),
            },
        );
    }

    /// 当前会话的 run id（Kotlin transcript 本地持久化的键）。
    pub fn current_run_id(&self) -> Option<String> {
        self.state
            .core
            .lock()
            .session_run
            .as_ref()
            .map(|r| r.run_id.0.clone())
    }

    // ── Provider 模型自动检测（设置页 Provider 域）────────────────

    /// 数据源列表（含各源本地状态：条目数 / 更新时间 / 体积）。
    pub fn registry_sources_json(&self) -> String {
        self.model_registry.sources_json()
    }

    /// 手动更新一个数据源（阻塞至完成，最长 120s；宿主须在 IO 线程
    /// 调用）。返回 `{"ok":true,"entries":N}` 或 `{"ok":false,"error"}`。
    pub fn registry_update_source_json(&self, id: String) -> String {
        let handle = {
            let guard = self.rt.lock().expect("mobile runtime lock poisoned");
            let rt = guard.as_ref().expect("runtime shut down").handle().clone();
            rt
        };
        let store = self.model_registry.clone();
        handle.block_on(async move {
            match tokio::time::timeout(
                std::time::Duration::from_secs(120),
                store.update_source(&id),
            )
            .await
            {
                Ok(Ok(entries)) => serde_json::json!({ "ok": true, "entries": entries }).to_string(),
                Ok(Err(e)) => serde_json::json!({ "ok": false, "error": format!("{e:#}") }).to_string(),
                Err(_) => serde_json::json!({ "ok": false, "error": "更新超时（120s）" }).to_string(),
            }
        })
    }

    /// 启动时的按需自动更新（后台 spawn，不阻塞；本地缺失或超过各源
    /// stale_after 才拉取）。
    pub fn registry_schedule_auto_update(&self) {
        let handle = {
            let guard = self.rt.lock().expect("mobile runtime lock poisoned");
            let rt = guard.as_ref().expect("runtime shut down").handle().clone();
            rt
        };
        let store = self.model_registry.clone();
        handle.spawn(async move {
            store.auto_update_if_stale().await;
        });
    }

    /// 本地条目搜索 / 浏览（数据源页面「查看条目 / 跨源搜索」）。纯
    /// 本地文件 + 内存缓存，同步返回：
    /// `{"total":N,"results":[{source,sourceId,id,ctx,out,vision,reasoning,tool,priceIn,priceOut},…]}`。
    /// `source_id` 空 = 跨全部已缓存源；`query` 空 = 浏览模式（前
    /// `limit` 条，键字典序）；`limit <= 0` 按 50 处理；数值 / 能力 /
    /// 计价字段源没给为 null；`id` 为源 JSON 原始键（保留 provider
    /// 前缀）。未知 source_id → `{"total":0,"results":[]}`。
    pub fn registry_search_json(&self, source_id: String, query: String, limit: i32) -> String {
        self.model_registry.search_json(&source_id, &query, limit)
    }

    /// 模型元数据查询：本地三源合并（源优先级 models.dev > LiteLLM >
    /// CloudPrice）即时返回；本地全 miss 时在线单查 CloudPrice 兜底
    /// （10s 超时）。字段缺失为 null。
    pub fn lookup_model_meta_json(&self, model_id: String) -> String {
        let local = self.model_registry.lookup_local(&model_id);
        if !local.is_empty() {
            return meta_to_json(&local);
        }
        let handle = {
            let guard = self.rt.lock().expect("mobile runtime lock poisoned");
            let rt = guard.as_ref().expect("runtime shut down").handle().clone();
            rt
        };
        let store = self.model_registry.clone();
        let id = model_id.clone();
        let meta = handle.block_on(async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                store.lookup_with_online_fallback(&id),
            )
            .await
            .unwrap_or_default()
        });
        meta_to_json(&meta)
    }

    /// 拉取 provider 的可用模型清单（GET /models，adapter 感知）。
    /// 返回 `{"ok":true,"models":["glm-4.7",...]}`；失败返回
    /// `{"ok":false,"error":"..."}`（密钥未配置 / 网络 / 协议错误均走
    /// 此路，UI 直接展示 error 文案）。
    pub fn list_provider_models(&self, provider: String) -> String {
        let handle = {
            let guard = self.rt.lock().expect("mobile runtime lock poisoned");
            let rt = guard.as_ref().expect("runtime shut down").handle().clone();
            rt
        };
        let state = self.state.clone();
        let secrets = self.secrets.clone();
        let provider_id = provider.clone();
        handle.block_on(async move {
            let pc = {
                let inner = state.core.lock();
                inner.config.providers.get(&provider_id).cloned()
            };
            let Some(pc) = pc else {
                return serde_json::json!({
                    "ok": false,
                    "error": format!("provider '{provider_id}' 不存在"),
                })
                .to_string();
            };
            // 密钥：FFI 注入的内存 key 优先，env 兜底（ollama 等本地
            // 端点允许无 key）。
            let key = secrets
                .get(&provider_id)
                .or_else(|| std::env::var(&pc.api_key_env).ok());
            match openslate_core::provider::list_remote_models(
                &pc.base_url,
                pc.adapter.as_deref().unwrap_or("openai"),
                key.as_deref(),
            )
            .await
            {
                Ok(models) => serde_json::json!({ "ok": true, "models": models }).to_string(),
                Err(e) => serde_json::json!({
                    "ok": false,
                    "error": format!("{e:#}"),
                })
                .to_string(),
            }
        })
    }

    // ── 历史会话（列表 / 切换）──────────────────────────────────

    /// 历史会话列表（JSON 数组，分页：每页 50 条，offset 递增）：
    /// [{id,title,status,started_ms,cost_usd}]。title 直接读 run 行的
    /// title 列（新 run 建立时已写 prompt 摘要；旧 run 由 open_session
    /// 治愈），不再逐 run 查 messages——N+1 已除。
    pub fn list_sessions_json(&self, offset: u32) -> String {
        let handle = {
            let guard = self.rt.lock().expect("mobile runtime lock poisoned");
            let rt = guard.as_ref().expect("runtime shut down").handle().clone();
            rt
        };
        let state = self.state.clone();
        handle.block_on(async move {
            let Some(store) = state.core.lock().store.clone() else {
                return "[]".to_owned();
            };
            let Ok(runs) = store.list_runs(50, offset as i64).await else {
                return "[]".to_owned();
            };
            let mut arr = Vec::with_capacity(runs.len());
            for r in runs {
                arr.push(serde_json::json!({
                    "id": r.id,
                    "title": r.title.clone().unwrap_or_else(|| "(空会话)".into()),
                    "status": r.status,
                    "started_ms": r.started_at,
                    "cost_usd": r.cost_usd,
                }));
            }
            serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_owned())
        })
    }

    /// 删除历史会话（run + 全部关联子表数据）。返回 false = 拒绝/失败：
    /// - 目标是当前活动会话（recorder 还在写它，删了会 FK 断裂——先
    ///   新建/切换会话再删）；
    /// - store 不可用 / run 不存在 / 数据库错误。
    ///
    /// 成功后 best-effort `wal_checkpoint`（截断 WAL，防删除后文件膨胀）。
    pub fn delete_session(&self, run_id: String) -> bool {
        // 活动会话拒绝删。
        {
            let inner = self.state.core.lock();
            if let Some(run) = &inner.session_run {
                if run.run_id.0 == run_id {
                    crate::alog!("delete_session: refuse to delete active session {run_id}");
                    return false;
                }
            }
        }
        let handle = {
            let guard = self.rt.lock().expect("mobile runtime lock poisoned");
            match guard.as_ref() {
                Some(rt) => rt.handle().clone(),
                // 已关停：无 runtime 可执行异步删除。
                None => return false,
            }
        };
        let state = self.state.clone();
        handle.block_on(async move {
            let Some(store) = state.core.lock().store.clone() else {
                return false;
            };
            match store.delete_run(&run_id).await {
                Ok(n) if n > 0 => {
                    if let Err(e) = store.wal_checkpoint().await {
                        crate::alog!("delete_session: wal_checkpoint failed (non-fatal): {e}");
                    }
                    crate::alog!("delete_session: deleted run {run_id}");
                    true
                }
                // run 不存在（可能已被删）。
                Ok(_) => false,
                Err(e) => {
                    crate::alog!("delete_session: delete_run failed: {e}");
                    false
                }
            }
        })
    }

    /// 切换到指定历史会话：加载消息 → 续用该 run → 广播新 snapshot。
    pub fn open_session(&self, run_id: String) -> Result<()> {
        // busy 守卫（对照 session.rs NewSession 的模式）：回合进行中切换
        // 会话会撕裂在途回合与 run 状态——先取消再切换。
        {
            let inner = self.state.core.lock();
            if inner.busy() {
                drop(inner);
                self.sink.broadcast(ServerMsg::Notice {
                    text: "回合进行中，无法切换会话（先取消）".into(),
                    level: NoticeLevel::Warn,
                });
                anyhow::bail!("回合进行中，无法切换会话（先取消）");
            }
        }
        let handle = {
            let guard = self.rt.lock().expect("mobile runtime lock poisoned");
            guard.as_ref().expect("runtime shut down").handle().clone()
        };
        let state = self.state.clone();
        handle.block_on(async move {
            let store = state
                .core
                .lock()
                .store
                .clone()
                .ok_or_else(|| anyhow::anyhow!("store 不可用"))?;
            let run = store
                .get_run(&run_id)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .ok_or_else(|| anyhow::anyhow!("会话 {run_id} 不存在"))?;
            let messages =
                openslate_store_sqlite::recorder::RunRecorder::load_messages(&store, &run.id)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            // ── 存量 run 标题治愈 ──
            // 旧版本的 run 标题是占位值（None / "mobile session"），历史
            // 会话列表只能显示占位。messages 已在手，零额外查询取首条
            // user 消息前 40 字符补写回库——治愈后的标题随 list_sessions
            // 直接从 run.title 读出。新 run 建立时已直接写入 prompt 摘
            // 要，不会命中本分支。
            if run
                .title
                .as_deref()
                .is_none_or(|t| t == "mobile session")
            {
                if let Some(healed) = messages
                    .iter()
                    .find(|m| m.role == MessageRole::User)
                    .map(|m| m.content.chars().take(40).collect::<String>())
                {
                    if let Err(e) = store.update_run_title(&run.id, &healed).await {
                        crate::alog!("open_session: heal run title failed (non-fatal): {e}");
                    }
                }
            }
            let root_agent_id = state.core.lock().agent_tree.get_root().id.0.clone();
            let recorder = openslate_store_sqlite::recorder::RunRecorder::resume(
                store,
                openslate_core::types::RunId(run.id.clone()),
                &root_agent_id,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            // ── 切换前收束旧 run ──
            // 对旧 run 而言本次切换是被打断 → "interrupted"（与
            // NewSession 的 "completed" 语义不同）；不收束则会留下永远
            // running 的僵尸记录。同一 run 重开无需收束（状态无谓抖动）。
            // 错误仅告警，不阻断切换。
            let (old_run, old_cost) = {
                let mut inner = state.core.lock();
                let same_run = inner
                    .session_run
                    .as_ref()
                    .is_some_and(|r| r.run_id.0 == run.id);
                let old_run = if same_run {
                    None
                } else {
                    inner.session_run.take()
                };
                (old_run, inner.stats.total_cost_usd)
            }; // guard 随块结束释放，之后的 await 不持锁。
            if let Some(old_run) = old_run {
                if let Err(e) = old_run.recorder.finish("interrupted", None, old_cost).await {
                    crate::alog!("open_session: finish previous run failed (non-fatal): {e}");
                }
            }
            let transcript = messages_to_transcript(&messages);
            let cost = run.cost_usd;
            {
                let mut inner = state.core.lock();
                inner.history = messages;
                inner.transcript = transcript;
                inner.session_run = Some(openslate_session::state::SessionRun {
                    run_id: openslate_core::types::RunId(run.id),
                    recorder: Arc::new(recorder),
                });
                inner.stats.total_cost_usd = cost;
                inner.running = false;
            }
            crate::alog!("open_session: switched to run {run_id}");
            Ok::<(), anyhow::Error>(())
        })?;
        self.emit_snapshot();
        Ok(())
    }
}

/// 状态装配（server `build_state` 的 mobile 变体：origin / 显式路径 /
/// 注入 factory）。`run_retention` = 启动保留策略（见
/// [`RuntimeOptions::run_retention`]）。
async fn assemble_state(
    config_path: std::path::PathBuf,
    sink: Arc<EventSink>,
    provider_factory: ProviderFactory,
    extra_tools: Vec<Box<dyn Tool>>,
    run_retention: i64,
) -> Result<Arc<AppState>> {
    let config_flag = config_path.to_str().map(|s| s.to_owned());
    crate::alog!("assemble: build_app_context begin");
    let ctx: AppContext = build_app_context_with(config_flag.as_deref(), extra_tools)
        .await
        .context("装配 AppContext 失败")?;
    crate::alog!("assemble: context built, wiring approval + core");
    let AppContext {
        config,
        agents,
        store,
        agent_tree,
        mut manager,
        skills,
        mcp_connections,
        config_path: active,
        ..
    } = ctx;

    apply_approval(&mut manager, &config, true, false);
    let policy = manager.approval.policy().clone();
    let approval_bridge = Arc::new(SessionApprovalBridge::new(sink.clone()));
    manager.approval = ApprovalManager::new(policy).with_callback(approval_bridge.clone());

    // 初始会话模型：capabilities.main（用户在设置页改的主对话模型）
    // 优先；无该节/代号不可解析时回退 root agent 的别名（旧配置行为）。
    let root_alias = agent_tree.get_root().model_alias.clone();
    let main_alias =
        openslate_core::model_config::capability_alias(&config, openslate_core::model_config::CAP_MAIN);
    let initial_alias = if openslate_core::model_config::resolve_model(&config, &main_alias).is_ok()
    {
        main_alias
    } else {
        root_alias
    };
    let session_label = config
        .project
        .as_ref()
        .and_then(|p| p.name.clone())
        .unwrap_or_else(|| "OpenSlate Mobile".to_owned());
    let root_agent_id = agent_tree.get_root().id.0.clone();

    // ── 启动清扫 + 保留策略（必须在恢复最近 run 之前）──────────────
    if let Some(store) = store.clone() {
        // 清扫语义：上次进程被杀（OOM / 崩溃 / 宿主直接杀 Activity）时
        // run 停留在 running，永远无人收尾 → 统一标 interrupted。顺序安
        // 全：get_last_resumable_run 只排除 failed，interrupted 仍可续聊，
        // 先清扫再恢复不影响重启续聊。
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        match store.mark_running_runs_interrupted(now_ms).await {
            Ok(n) if n > 0 => {
                crate::alog!("startup sweep: {n} stale running run(s) → interrupted")
            }
            Ok(_) => {}
            Err(e) => crate::alog!("startup sweep failed (non-fatal): {e}"),
        }
        // 保留策略：只留最近 run_retention 个 run，其余连同子表数据删
        // 除；删了东西才 checkpoint 截断 WAL。失败仅告警不阻断启动。
        match store.prune_runs_keep_last(run_retention).await {
            Ok(deleted) if !deleted.is_empty() => {
                crate::alog!(
                    "retention: pruned {} old run(s) (keep {})",
                    deleted.len(),
                    run_retention
                );
                if let Err(e) = store.wal_checkpoint().await {
                    crate::alog!("retention: wal_checkpoint failed (non-fatal): {e}");
                }
            }
            Ok(_) => {}
            Err(e) => crate::alog!("retention prune failed (non-fatal): {e}"),
        }
    }

    // ── 重启续聊：恢复最近一次可续 run 的 history + transcript 镜像 ──
    let mut restored_history: Vec<Message> = Vec::new();
    let mut restored_transcript: Vec<openslate_protocol::EntryDto> = Vec::new();
    let mut restored_run: Option<openslate_session::state::SessionRun> = None;
    let mut restored_cost = 0.0f64;
    if let Some(store) = store.clone() {
        match store.get_last_resumable_run().await {
            Ok(Some(run)) => {
                match openslate_store_sqlite::recorder::RunRecorder::load_messages(
                    &store, &run.id,
                )
                .await
                {
                    Ok(messages) if !messages.is_empty() => {
                        crate::alog!(
                            "restore: resuming run {} ({} messages, status={})",
                            run.id,
                            messages.len(),
                            run.status
                        );
                        restored_cost = run.cost_usd;
                        restored_transcript = messages_to_transcript(&messages);
                        restored_history = messages;
                        if let Ok(recorder) =
                            openslate_store_sqlite::recorder::RunRecorder::resume(
                                store.clone(),
                                openslate_core::types::RunId(run.id.clone()),
                                &root_agent_id,
                            )
                            .await
                        {
                            restored_run = Some(openslate_session::state::SessionRun {
                                run_id: openslate_core::types::RunId(run.id.clone()),
                                recorder: Arc::new(recorder),
                            });
                        }
                    }
                    Ok(_) => crate::alog!("restore: last run has no messages; fresh session"),
                    Err(e) => crate::alog!("restore: load_messages failed: {e}; fresh session"),
                }
            }
            Ok(_) => crate::alog!("restore: no resumable run; fresh session"),
            Err(e) => crate::alog!("restore: query failed: {e}; fresh session"),
        }
    }

    let inner = CoreInner {
        session_id: uuid::Uuid::new_v4().simple().to_string(),
        session_label,
        history: restored_history,
        transcript: restored_transcript,
        running: false,
        compacting: false,
        depth_cur: 0,
        agents_running: 0,
        tool_calls_cur: 0,
        model_alias: initial_alias,
        cancel: None,
        config,
        agents_cfg: agents,
        agent_tree,
        skills: skills
            .skills()
            .iter()
            .map(|s| openslate_protocol::SkillInfoDto {
                name: s.name.clone(),
                description: s.description.clone(),
            })
            .collect(),
        store,
        session_run: restored_run,
        manager: Some(manager),
        engine_task: None,
        paths: ConfigPaths {
            active: active.clone(),
            global: None,
            local: Some(active),
        },
        stats: openslate_protocol::SessionStatsDto {
            total_cost_usd: restored_cost,
            ..Default::default()
        },
        stream: Default::default(),
        tool_timers: std::collections::HashMap::new(),
        pending_step_meta: None,
    };

    Ok(Arc::new(AppState {
        core: Arc::new(SessionCore::new(inner)),
        sink,
        approval: approval_bridge,
        auth_token: None,
        provider_factory,
        origin: "mobile",
        _mcp: mcp_connections,
    }))
}

/// ModelMeta → FFI JSON（字段缺失为 null；completeness 便于调用方判断）。
fn meta_to_json(m: &openslate_core::model_registry::ModelMeta) -> String {
    serde_json::json!({
        "ok": !m.is_empty(),
        "display_name": m.display_name,
        "context_tokens": m.context_tokens,
        "max_output_tokens": m.max_output_tokens,
        "supports_vision": m.supports_vision,
        "supports_reasoning": m.supports_reasoning,
        "supports_tool_call": m.supports_tool_call,
        "input_price_per_mtok": m.input_price_per_mtok,
        "output_price_per_mtok": m.output_price_per_mtok,
        "source": m.source,
    })
    .to_string()
}

/// 恢复的 history → transcript 镜像投影（User/Assistant/Tool 三类核心
/// 条目；reasoning/meta 不在持久化里，从简）。
fn messages_to_transcript(messages: &[Message]) -> Vec<EntryDto> {
    let mut out = Vec::new();
    for m in messages {
        match m.role {
            MessageRole::User => out.push(EntryDto::User {
                text: m.content.clone(),
            }),
            MessageRole::Assistant => {
                // 思维链随消息持久化——恢复时还原为折叠的 Reasoning 条目。
                if let Some(rc) = m.reasoning_content.as_ref().filter(|r| !r.is_empty()) {
                    out.push(EntryDto::Reasoning { text: rc.clone() });
                }
                if let Some(tcs) = m.tool_calls.as_ref() {
                    for tc in tcs {
                        out.push(EntryDto::ToolCall {
                            name: tc.name.clone(),
                            args: tc
                                .arguments
                                .to_string()
                                .chars()
                                .take(40)
                                .collect::<String>(),
                            call_id: Some(tc.id.0.clone()),
                            status: ToolStatusDto::Done {
                                bytes: 0,
                                truncated: false,
                                elapsed_ms: None,
                            },
                            detail: openslate_protocol::ToolEntryDetailDto {
                                args: tc.arguments.to_string(),
                                output: None,
                            },
                        });
                    }
                }
                if !m.content.trim().is_empty() {
                    out.push(EntryDto::Assistant {
                        text: m.content.clone(),
                    });
                }
            }
            MessageRole::Tool => {
                // 工具结果并入上一条同名 ToolCall（简化：追加 output）。
                if let Some(EntryDto::ToolCall { detail, .. }) = out.iter_mut().rev().find_map(|e| {
                    if let EntryDto::ToolCall {
                        name, call_id, ..
                    } = e
                    {
                        if call_id.as_deref()
                            == m.tool_call_id.as_ref().map(|x| x.0.as_str())
                            || (m.tool_call_id.is_none() && name == m.name.as_deref().unwrap_or(""))
                        {
                            return Some(e);
                        }
                        None
                    } else {
                        None
                    }
                }) {
                    detail.output = Some(
                        m.content
                            .chars()
                            .take(2000)
                            .collect::<String>(),
                    );
                }
            }
            _ => {}
        }
    }
    out
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! runtime 生命周期 / 安全守卫的单元级集成测试（desktop 可跑）。
    //! harness（scripted provider + 事件采集）与 tests/runtime_flow.rs
    //! 同构；放这里以触达 runtime 的私有状态（core.session_run 等）。
    //! 数据库校验一律经独立 scratch 连接（mobile runtime 的池归其私有
    //! runtime 所有，关停后不复用）。

    use std::collections::VecDeque;
    use std::sync::mpsc as std_mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use openslate_core::error::ProviderError;
    use openslate_core::provider::{GenerateRequest, ModelProvider};
    use openslate_core::types::{ModelResponse, ModelStreamEvent, Usage};
    use openslate_session::state::ProviderFactory;

    use super::*;

    // ── harness（与 tests/runtime_flow.rs 同构）─────────────────────

    #[derive(Clone)]
    struct ScriptedProvider {
        scripts: Arc<Mutex<VecDeque<Vec<ModelStreamEvent>>>>,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Vec<ModelStreamEvent>>) -> Self {
            Self {
                scripts: Arc::new(Mutex::new(scripts.into())),
            }
        }
    }

    #[async_trait]
    impl ModelProvider for ScriptedProvider {
        async fn generate(&self, _request: GenerateRequest) -> Result<ModelResponse, ProviderError> {
            Err(ProviderError::ServerError(500))
        }

        async fn generate_stream(
            &self,
            _request: GenerateRequest,
        ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default();
            tokio::spawn(async move {
                for event in script {
                    let _ = tx.send(Ok(event)).await;
                }
            });
            rx
        }

        fn provider_name(&self) -> &str {
            "scripted"
        }
    }

    fn factory_for(provider: ScriptedProvider) -> ProviderFactory {
        Arc::new(move |_config, _alias| {
            Ok(Box::new(provider.clone()) as Box<dyn ModelProvider>)
        })
    }

    struct Collector {
        events: Mutex<Vec<serde_json::Value>>,
        notify: std_mpsc::Sender<()>,
    }

    impl EventCallback for Collector {
        fn on_event(&self, event_json: String) {
            if let Ok(v) = serde_json::from_str(&event_json) {
                self.events.lock().unwrap().push(v);
            }
            let _ = self.notify.send(());
        }
    }

    impl Collector {
        fn new() -> (Arc<Self>, std_mpsc::Receiver<()>) {
            let (tx, rx) = std_mpsc::channel();
            (
                Arc::new(Self {
                    events: Mutex::new(Vec::new()),
                    notify: tx,
                }),
                rx,
            )
        }

        /// 阻塞等待直到出现匹配 type 的事件（超时失败）。
        fn wait_for(
            &self,
            rx: &std_mpsc::Receiver<()>,
            want_type: &str,
            deadline: Duration,
        ) -> Vec<serde_json::Value> {
            self.wait_for_count(rx, want_type, 1, deadline)
        }

        /// 阻塞等待直到匹配 type 的事件达到 n 条。
        fn wait_for_count(
            &self,
            rx: &std_mpsc::Receiver<()>,
            want_type: &str,
            n: usize,
            deadline: Duration,
        ) -> Vec<serde_json::Value> {
            let start = Instant::now();
            loop {
                let all = self.events.lock().unwrap().clone();
                let hit = all
                    .iter()
                    .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some(want_type))
                    .count();
                if hit >= n {
                    return all;
                }
                assert!(
                    start.elapsed() < deadline,
                    "等待 {n} 条 {want_type} 事件超时（已有 {hit}）；已收到: {:?}",
                    all.iter()
                        .map(|e| e.get("type").and_then(|t| t.as_str()).unwrap_or("?"))
                        .collect::<Vec<_>>()
                );
                rx.recv_timeout(Duration::from_millis(200)).ok();
            }
        }
    }

    fn temp_paths() -> (MobilePaths, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let paths = MobilePaths {
            config_dir: dir.path().join("config"),
            workspace_dir: dir.path().join("workspace"),
            data_dir: dir.path().join("data"),
            cache_dir: dir.path().join("cache"),
        };
        (paths, dir)
    }

    fn text_response(text: &str) -> Vec<ModelStreamEvent> {
        vec![
            ModelStreamEvent::Delta(text.to_owned()),
            ModelStreamEvent::Usage(Usage {
                input_tokens: 8,
                output_tokens: 4,
                cached_input_tokens: None,
                reasoning_tokens: None,
            }),
            ModelStreamEvent::Done(ModelResponse {
                content: Some(text.to_owned()),
                tool_calls: vec![],
                reasoning_content: None,
                usage: None,
                finish_reason: Some("stop".into()),
            }),
        ]
    }

    fn scripted_opts(scripts: Vec<Vec<ModelStreamEvent>>) -> RuntimeOptions {
        RuntimeOptions {
            provider_factory: Some(factory_for(ScriptedProvider::new(scripts))),
            ..Default::default()
        }
    }

    /// 在独立小 runtime 上打开 data_dir 的库（返回的 runtime 供后续
    /// block_on 查询复用）。
    fn scratch_store(
        paths: &MobilePaths,
    ) -> (
        tokio::runtime::Runtime,
        openslate_store_sqlite::store::SqliteStore,
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let db = paths.data_dir.join("openslate.db").display().to_string();
        let store = rt
            .block_on(async {
                let s = openslate_store_sqlite::store::SqliteStore::new(&db).await?;
                s.run_migrations().await?;
                anyhow::Ok(s)
            })
            .unwrap();
        (rt, store)
    }

    // ── 1. send 封禁 set_api_key ─────────────────────────────────────

    #[test]
    fn send_rejects_set_api_key_protocol_message() {
        let (paths, _dir) = temp_paths();
        let (collector, rx) = Collector::new();
        let runtime =
            MobileRuntime::create_with(paths.clone(), collector.clone(), scripted_opts(vec![]))
                .unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

        runtime
            .send(r#"{"type":"set_api_key","provider":"zhipu","value":"sk-secret"}"#)
            .unwrap();

        let events = collector.wait_for(&rx, "error", Duration::from_secs(5));
        let err = events.iter().find(|e| e["type"] == "error").unwrap();
        assert_eq!(err["code"].as_str().unwrap(), "set_api_key_forbidden");
        assert!(
            err["message"].as_str().unwrap().contains(".env"),
            "错误说明应指向 .env 风险: {err}"
        );
        // 密钥绝不能落盘（协议层实现会写 config_dir/.env）。
        assert!(!paths.config_dir.join(".env").exists());

        runtime.shutdown();
    }

    // ── 2. open_session busy 守卫 ────────────────────────────────────

    #[test]
    fn open_session_rejects_while_busy() {
        let (paths, _dir) = temp_paths();
        let (collector, rx) = Collector::new();
        let runtime =
            MobileRuntime::create_with(paths, collector.clone(), scripted_opts(vec![])).unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

        // 模拟回合进行中。
        {
            let mut inner = runtime.state.core.lock();
            inner.running = true;
        }
        assert!(runtime.open_session("any-run".into()).is_err());

        // 广播 Warn notice 引导用户先取消。
        let events = collector.wait_for(&rx, "notice", Duration::from_secs(5));
        let notice = events.iter().find(|e| e["type"] == "notice").unwrap();
        assert_eq!(notice["level"].as_str().unwrap(), "warn");
        assert!(notice["text"].as_str().unwrap().contains("回合进行中"));

        runtime.shutdown();
    }

    // ── 3. shutdown 收束 run 为 interrupted ──────────────────────────

    #[test]
    fn shutdown_marks_open_run_interrupted() {
        let (paths, _dir) = temp_paths();
        let (collector, rx) = Collector::new();
        let runtime = MobileRuntime::create_with(
            paths.clone(),
            collector.clone(),
            scripted_opts(vec![text_response("回答")]),
        )
        .unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

        runtime
            .send(r#"{"type":"submit","text":"你好"}"#)
            .unwrap();
        collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));
        let run_id = runtime.current_run_id().expect("run 应已打开");

        runtime.shutdown();

        let (srt, store) = scratch_store(&paths);
        let run = srt
            .block_on(async { store.get_run(&run_id).await })
            .unwrap()
            .expect("run 仍应存在");
        assert_eq!(run.status, "interrupted");
        assert!(run.finished_at.is_some(), "finished_at 应已补上");
    }

    // ── 4. delete_session 规则 ───────────────────────────────────────

    #[test]
    fn delete_session_refuses_active_and_deletes_inactive() {
        let (paths, _dir) = temp_paths();
        let (collector, rx) = Collector::new();
        let runtime = MobileRuntime::create_with(
            paths.clone(),
            collector.clone(),
            scripted_opts(vec![
                text_response("第一段回答"),
                text_response("第二段回答"),
            ]),
        )
        .unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

        // 会话 A：submit → 完成回合。
        runtime
            .send(r#"{"type":"submit","text":"第一问"}"#)
            .unwrap();
        collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));
        let run_a = runtime.current_run_id().expect("run A 应已打开");

        // 活动会话拒绝删。
        assert!(!runtime.delete_session(run_a.clone()), "活动会话不可删");

        // 新建会话（A 收束）→ 会话 B。
        runtime.send(r#"{"type":"new_session"}"#).unwrap();
        collector.wait_for(&rx, "session_reset", Duration::from_secs(5));
        runtime
            .send(r#"{"type":"submit","text":"第二问"}"#)
            .unwrap();
        collector.wait_for_count(&rx, "turn_ok", 2, Duration::from_secs(20));
        let run_b = runtime.current_run_id().expect("run B 应已打开");
        assert_ne!(run_a, run_b);

        // 非活动会话可删；不存在 / 活动的返回 false。
        assert!(runtime.delete_session(run_a.clone()), "非活动会话应可删");
        assert!(!runtime.delete_session(run_b.clone()), "run B 现在是活动的");
        assert!(
            !runtime.delete_session("no-such-run".into()),
            "不存在的 run 返回 false"
        );

        let (srt, store) = scratch_store(&paths);
        let gone = srt
            .block_on(async { store.get_run(&run_a).await })
            .unwrap()
            .is_none();
        assert!(gone, "run A 应已连同消息被删除");

        runtime.shutdown();
    }

    // ── 5. assemble 启动清扫 + retention ─────────────────────────────

    #[test]
    fn assemble_sweeps_stale_running_and_applies_retention() {
        let (paths, _dir) = temp_paths();
        paths.ensure_layout().unwrap();
        // 预置 5 个 run（旧 → 新）：seed-0/1/2 completed、seed-3 running
        // （僵尸）、seed-4 completed。retention=3 → 只留 seed-2/3/4。
        {
            let (srt, store) = scratch_store(&paths);
            srt.block_on(async {
                for (id, status, started) in [
                    ("seed-0", "completed", 1_000i64),
                    ("seed-1", "completed", 2_000),
                    ("seed-2", "completed", 3_000),
                    ("seed-3", "running", 4_000),
                    ("seed-4", "completed", 5_000),
                ] {
                    store
                        .insert_run(id, None, "root", status, "{}", started)
                        .await
                        .unwrap();
                }
            });
        }

        let (collector, _rx) = Collector::new();
        let opts = RuntimeOptions {
            run_retention: 3,
            ..scripted_opts(vec![])
        };
        let runtime = MobileRuntime::create_with(paths.clone(), collector, opts).unwrap();

        let (srt, store) = scratch_store(&paths);
        let (count, s0, s1, s3, s4) = srt.block_on(async {
            let count = store.count_runs().await.unwrap();
            let s0 = store.get_run("seed-0").await.unwrap();
            let s1 = store.get_run("seed-1").await.unwrap();
            let s3 = store.get_run("seed-3").await.unwrap();
            let s4 = store.get_run("seed-4").await.unwrap();
            (count, s0, s1, s3, s4)
        });
        assert_eq!(count, 3, "retention=3 应只留最近 3 个 run");
        assert!(s0.is_none() && s1.is_none(), "最旧的 2 个应被 prune");
        assert_eq!(
            s3.expect("seed-3 在保留窗口内").status,
            "interrupted",
            "stale running 应被清扫为 interrupted"
        );
        assert_eq!(
            s4.expect("seed-4 在保留窗口内").status,
            "completed",
            "非 running 状态不应被清扫改动"
        );

        runtime.shutdown();
    }

    // ── 6. list_sessions 分页 ────────────────────────────────────────

    #[test]
    fn list_sessions_paginates_with_offset() {
        let (paths, _dir) = temp_paths();
        paths.ensure_layout().unwrap();
        // 预置 55 个 run（旧 → 新），最新的带标题；retention 默认 100
        // → 不触发 prune。
        {
            let (srt, store) = scratch_store(&paths);
            srt.block_on(async {
                for i in 0..55 {
                    let id = format!("page-{i:02}");
                    let title = (i == 54).then_some("带标题的最新会话");
                    store
                        .insert_run(&id, title, "root", "completed", "{}", 1_000 + i)
                        .await
                        .unwrap();
                }
            });
        }

        let (collector, _rx) = Collector::new();
        let runtime = MobileRuntime::create_with(paths, collector, scripted_opts(vec![])).unwrap();

        let page0: Vec<serde_json::Value> =
            serde_json::from_str(&runtime.list_sessions_json(0)).unwrap();
        assert_eq!(page0.len(), 50, "每页 50 条");
        assert_eq!(page0[0]["id"].as_str().unwrap(), "page-54", "最新在前");
        assert_eq!(
            page0[0]["title"].as_str().unwrap(),
            "带标题的最新会话",
            "标题直接来自 run.title（无 N+1 查询）"
        );
        assert_eq!(
            page0[1]["title"].as_str().unwrap(),
            "(空会话)",
            "无标题 run 显示占位"
        );

        let page1: Vec<serde_json::Value> =
            serde_json::from_str(&runtime.list_sessions_json(50)).unwrap();
        assert_eq!(page1.len(), 5, "第二页余 5 条");
        assert_eq!(page1[0]["id"].as_str().unwrap(), "page-04");
        assert_eq!(page1[4]["id"].as_str().unwrap(), "page-00");

        let page2: Vec<serde_json::Value> =
            serde_json::from_str(&runtime.list_sessions_json(100)).unwrap();
        assert!(page2.is_empty(), "翻过头 → 空页");

        runtime.shutdown();
    }
}
