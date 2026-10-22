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
use openslate_app::wiring::{
    apply_approval, build_app_context_with, connect_external_mcp, load_config, AppContext,
};
use openslate_core::approval::{rules_from_config, ApprovalManager, ToolApprovalLevel};
use openslate_core::error::ToolError;
use openslate_core::mcp::McpConnectionGuard;
use openslate_core::tool::Tool;
use openslate_core::types::{Message, MessageRole, ToolOutput};
use openslate_protocol::{ClientMsg, EntryDto, NoticeLevel, ServerMsg, PROTOCOL_VERSION};
use openslate_session::approval::SessionApprovalBridge;
use openslate_session::session as session_dispatch;
use openslate_session::state::{
    build_snapshot, AppState, ConfigPaths, CoreInner, MsgSink, ProviderFactory, SessionCore,
};

use crate::approval_review::AiReviewApproval;
use crate::attachment_tokens::AttachmentTokenBackfill;
use crate::askuser::{
    AskUserContext, AskUserTool, MobileQuestionHandler, SideEffectFreeApproval,
    startup_question_maintenance,
};
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
    /// 审批 AI 审查等待上限（tool-approval P1b；默认 20s，测试注 200ms
    /// 级别覆盖超时回退路径）。
    pub approval_review_timeout: Duration,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            provider_factory: None,
            extra_tools: Vec::new(),
            host_call_timeout: HOST_CALL_TIMEOUT,
            run_retention: 100,
            approval_review_timeout: crate::approval_review::DEFAULT_REVIEW_TIMEOUT,
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
    /// 外部 MCP server 连接 guard（热重载整体替换；builtin 句柄在
    /// `AppState._mcp`，不在此列）。声明在 `registry` 之后：drop 时工具
    /// 先于连接消失。
    mcp_external: std::sync::Mutex<McpConnectionGuard>,
    /// 当前已注册的外部 MCP 工具名（热重载时精确反注册的名单）。
    mcp_tool_names: std::sync::Mutex<Vec<String>>,
    /// 生效配置路径（热重载读最新落盘的 `[mcp]` 配置）。
    config_path: PathBuf,
    /// ask_user 共享上下文（工具 / QuestionHandler / 快照回填共用）。
    ask: Arc<AskUserContext>,
    /// bash 工具的环境注入源（ask_user secret env 形态；热切换/重载
    /// 重注册时随工具传递）。
    bash_env: crate::exec::EnvSource,
    /// MCP host 死亡自愈共享上下文（所有 RecoveringMcpTool 装饰器共用：
    /// 单飞恢复闩 + 冷却 + 注册表弱引用；reload_mcp 迁移新工具时复用）。
    mcp_recovery: Arc<crate::mcp_recovery::RecoveryCtx>,
    /// 关停标志（Arc：transcript 同步任务与 shutdown 共享）。
    shutdown_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 转写同步共享状态（C1：sync_loop 周期写库与 NewSession / open_
    /// session / shutdown 的强制冲刷共用同一游标表；见 transcripts.rs）。
    sync: std::sync::Arc<crate::transcripts::SyncState>,
    /// 附件 image tokens 响应侧回填（协议 v5）：EventSink tap 闩首次
    /// 调用值，sync_loop 落 transcript，重开经 meta_json 重放。见
    /// `attachment_tokens` 模块文档。
    attachment_tokens: Arc<AttachmentTokenBackfill>,
}

impl MobileRuntime {
    /// 默认装配（生产入口：FFI 调这个）。
    pub fn create(paths: MobilePaths, callback: Arc<dyn EventCallback>) -> Result<Arc<Self>> {
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
        // 附件 image tokens 回填（协议 v5）：tap 收 Usage/TurnOk/TurnError
        //（广播线程，只动自身 mutex，见模块文档）。
        let attachment_tokens = AttachmentTokenBackfill::new();
        {
            let bf = attachment_tokens.clone();
            sink.set_tap(Arc::new(move |msg: &ServerMsg| bf.on_event(msg)));
        }
        let host_router = HostCallRouter::new(sink.clone(), opts.host_call_timeout);

        // ask_user secret 凭据库（会话内存 + store v4 持久层；装配完成后
        // attach_store 加载镜像）。工具与注入源共用同一实例。
        let vault = Arc::new(crate::secrets::PersistentSecretVault::new());

        // ask_user 共享上下文：工具先构造（AppState 尚不存在），装配后
        // wire 注入（Weak 防引用环，见 question.rs 模块文档）。
        let ask_ctx = AskUserContext::new(host_router.clone(), vault.clone());

        // env 注入源（ask_user secret env 形态）：
        // - native bash（本 crate NativeShellTool）经 register_bash_tools
        //   传入，spawn 时取最新值；
        // - mcp-builtin shell/bash server（若启用；mobile 默认
        //   `[builtin_tools] enabled = false`，此线为兜底）经进程级
        //   `set_shell_env_source` 注册——shell.rs 共享执行核心统一消费，
        //   值不打日志。
        let env_source = crate::secrets::env_source(vault.clone());
        openslate_mcp_builtin::set_shell_env_source(Some(env_source.clone()));

        // host/本地工具集：mobile.ping（演示）+ ask_user（Agent 向用户
        // 提问）+ bash 工具（按设置多选动态注册：native ⇒ `bash`；
        // termux ⇒ `termux_bash`，名字与后端一一绑定）+ 注入项
        // （+ output_query，见下方 output_store_handle 处——它要复用
        // 同一 store 接缝句柄）。
        let exec_selection = ExecSelectionCell::new(ExecSelection::default());
        let mut extra_tools = opts.extra_tools;
        extra_tools.push(Box::new(AskUserTool::new(ask_ctx.clone())));
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
        // bash 工具不进 extra_tools（注册名随设置变化）：装配期按默认
        // 多选（空 = 暂不注册）注册，宿主随后由 set_exec_backends 按持
        // 久化偏好热注册。

        let secrets = Arc::new(MobileSecrets::default());
        // provider 工厂统一包一层附件字节装载代理（设计 §6.1 混合式）：
        // history 里的 user 消息只带附件引用，请求进 provider 前由代理
        // 从 files 按需回填 data（单点执行 max_inline_total_mb 封顶）。
        // store 装配后才存在 → OnceLock 后注。
        let attachment_store: crate::attachments::SharedStore = Arc::new(std::sync::OnceLock::new());
        // 长输出落盘 storer（C3）：core 工具输出分叉点的宿主实现。store
        // 同样经 OnceLock 后注（AttachmentDataProxy 同款接缝，共用
        // SharedStore 类型）；进程级单槽注册一次全进程生效，**shutdown
        // 无需清理**（mobile 单进程 App，随进程生灭；桌面端不注册，core
        // 直通旧截断路径）。
        let output_store_handle: crate::attachments::SharedStore =
            Arc::new(std::sync::OnceLock::new());
        openslate_core::output_store::set_tool_output_storer(Some(Arc::new(
            crate::output_storer::MobileOutputStorer::new(output_store_handle.clone()),
        )));
        // output_query（长输出落盘 C4）：查落盘输出全文的内置工具。store
        // 复用上面的 SharedStore 接缝（同一注入点后注）；AppState 弱引用
        // 经共享 OnceLock 在 assemble_state 后接线（当前 run 优先 +
        // [limits].max_output_bytes 生效值）。
        let output_query_tool =
            crate::output_query::OutputQueryTool::new(output_store_handle.clone());
        let output_query_state = output_query_tool.state_handle();
        extra_tools.push(Box::new(output_query_tool));
        let inner_factory = opts
            .provider_factory
            .unwrap_or_else(|| mobile_provider_factory(secrets.clone()));
        let factory_store = attachment_store.clone();
        let factory: ProviderFactory = Arc::new(move |config, alias| {
            let provider = inner_factory(config, alias)?;
            Ok(Box::new(crate::attachments::AttachmentDataProxy::new(
                provider,
                factory_store.clone(),
                config.attachments,
            )) as Box<dyn openslate_core::provider::ModelProvider>)
        });
        // 模型元数据注册表（数据源页面 / 自动补全；本地 ZSTD 持久化）。
        let model_registry = Arc::new(crate::registry::RegistryStore::new(&paths.config_dir));

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("openslate-mobile")
            .enable_all()
            .build()
            .context("创建 tokio runtime 失败")?;
        crate::alog!("create_with: tokio runtime built, assembling state…");

        // 外部 MCP 连接 guard + 工具名册随装配交出（热重载整体替换；
        // builtin 句柄已随 AppState._mcp 保活）。
        let (state, mcp_external, mcp_tool_names) = rt.block_on(assemble_state(
            config_path.clone(),
            sink.clone(),
            factory.clone(),
            extra_tools,
            opts.run_retention,
            opts.approval_review_timeout,
        ))?;
        crate::alog!("create_with: state assembled, wiring approval + ask_user");
        // ask_user 接线：上下文注入 store/配置 + QuestionHandler 装配
        // （dispatch 的 question_reply / cancel 由此进 mobile 实现）。
        ask_ctx.wire(&state);
        // output_query 接线：AppState 弱引用（当前 run 优先 + 输出封顶
        // 生效值；AskUserContext.wire 同款装配后注入）。
        let _ = output_query_state.set(Arc::downgrade(&state));
        state
            .core
            .set_question_handler(Arc::new(MobileQuestionHandler::new(ask_ctx.clone())));
        // vault 持久层接线：加载 credentials 镜像（remember 凭据跨会话
        // 可用——env 注入/lookup/脱敏值源全走镜像）。附件字节装载代理
        // 的 store 同步后注（provider 工厂闭包持有 OnceLock）。
        if let Some(store) = state.core.lock().store.clone() {
            let _ = attachment_store.set(Arc::new(store.clone()));
            // 长输出 storer 的 store 同步后注（与附件代理同一注入点）。
            let _ = output_store_handle.set(Arc::new(store.clone()));
            rt.block_on(vault.attach_store(store));
        }
        // 输出脱敏接线：core 的进程级 masker 消费 vault 已知密钥值；
        // `[ask_user] mask_secrets` 开关在闭包内读 context 的配置快照
        // （默认 true = 开）。历史遗留：配置快照随装配固定，运行期改
        // toml 需重启（或后续 lane 的 FFI 写入口顺带 refresh）。
        {
            let ctx = ask_ctx.clone();
            openslate_core::ask_user::push_secret_masker(Arc::new(move |text| {
                if !ctx.mask_enabled() {
                    return text.to_owned();
                }
                ctx.mask(text)
            }));
        }
        // 拿到运行期注册表并按当前（默认）多选注册 bash 工具。
        let runtime_registry = state
            .core
            .lock()
            .manager
            .as_ref()
            .expect("manager present")
            .tool_registry
            .clone();
        // 选择点 1（装配期）：外部 MCP 工具已在 assemble_state 里注册，
        // 其名册 ∩ bash 域 = MCP 接管名（builtin 模板按装配时的同一份
        // 配置展开）——HostTool/native 让位：不清理、不顶位。
        let mcp_cfg = state.core.lock().config.mcp.clone().unwrap_or_default();
        let mcp_owned = crate::exec::mcp_owned_bash_names(&mcp_tool_names, &mcp_cfg);
        if !mcp_owned.is_empty() {
            crate::alog!("create_with: MCP 已接管 bash 名 {mcp_owned:?}（host 侧让位）");
        }
        register_bash_tools(
            &runtime_registry,
            exec_selection.get(),
            paths.workspace_dir.clone(),
            host_router.clone(),
            env_source.clone(),
            &mcp_owned,
        );
        // MCP host 死亡自愈：装配期包装点——外部 MCP 工具名册整体包上
        // RecoveringMcpTool（连接失败签名 → 单飞恢复 host call → 注册表
        // 同名重发一次，见 mcp_recovery.rs）。bash 域被 MCP 接管的名字也
        // 在名册内（名册里的都是 McpTool 本体，让位判定按名不按身份）；
        // HostTool 壳 / native bash 不在名册内不会被包。
        let mcp_recovery = crate::mcp_recovery::RecoveryCtx::new(
            host_router.clone(),
            &runtime_registry,
        );
        crate::mcp_recovery::wrap_mcp_tools(&runtime_registry, &mcp_tool_names, &mcp_recovery);
        if !mcp_tool_names.is_empty() {
            crate::alog!(
                "create_with: {} 个外部 MCP 工具已包恢复装饰器（host 死亡自愈）",
                mcp_tool_names.len()
            );
        }
        let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // C1：转写前置冲刷钩子注册（NewSession 清转写前强制落库）；返回
        // 的 SyncState 与 sync_loop 共享（同一游标表，写者收敛）。
        let sync_state = crate::transcripts::install_flusher(&state.core);
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
            mcp_external: std::sync::Mutex::new(mcp_external),
            mcp_tool_names: std::sync::Mutex::new(mcp_tool_names),
            config_path,
            ask: ask_ctx,
            bash_env: env_source,
            mcp_recovery,
            shutdown_flag: shutdown_flag.clone(),
            sync: sync_state.clone(),
            attachment_tokens,
        });

        // transcript 持久化同步任务（设计 §4：行级增量落 transcript_
        // entries + 存储健康横幅；详见 transcripts.rs）。随 shutdown_flag
        // 退出；runtime drop 前最多再跑一个周期。
        {
            let handle = {
                let guard = runtime.rt.lock().expect("mobile runtime lock poisoned");
                guard
                    .as_ref()
                    .expect("runtime alive at create_with tail")
                    .handle()
                    .clone()
            };
            handle.spawn(crate::transcripts::sync_loop(
                runtime.state.clone(),
                runtime.sink.clone(),
                shutdown_flag,
                runtime.attachment_tokens.clone(),
                runtime.sync.clone(),
            ));
        }

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
                        message: format!("协议版本不匹配：宿主 {proto}，运行时 {PROTOCOL_VERSION}"),
                    });
                    return Ok(());
                }
                self.emit_snapshot();
            }
            other => {
                let state = self.state.clone();
                let sink = self.sink.clone();
                let model_registry = self.model_registry.clone();
                let attachment_tokens = self.attachment_tokens.clone();
                // 从 Option<Runtime> 借出 Handle spawn（不移动 runtime 本体）。
                let handle = {
                    let guard = self.rt.lock().expect("mobile runtime lock poisoned");
                    let rt = guard.as_ref().expect("runtime already shut down");
                    rt.handle().clone()
                };
                handle.spawn(async move {
                    // Submit 附件预处理（多模态输入 §3.3/§3.4/§6.4）：门控
                    // （模态能力 / 数量 / 体积 / 图视频互斥）→ 通过则原样
                    // dispatch（session 侧把引用构进 user 消息）→ 引擎 spawn
                    // 完成后把附件绑定到本 run。拒绝经 notice 通道返回。
                    let (msg, bind_ids) =
                        match gate_submit_attachments(&state, &sink, &model_registry, other).await
                        {
                            Some(v) => v,
                            None => return, // 已 notice 拒绝，不进 dispatch
                        };
                    // 附件 image tokens 回填标记（协议 v5，dispatch 前设置
                    // ——引擎首次 Usage 事件不会跑赢本地路径）：**单图**
                    // Submit 才观测（多图 usage 不分图，回填哪个都是错）。
                    let mark_single_image = match &msg {
                        ClientMsg::Submit {
                            attachments: Some(dtos),
                            ..
                        } if dtos.len() == 1 && dtos[0].mime.starts_with("image/") => {
                            Some(dtos[0].id.clone())
                        }
                        _ => None,
                    };
                    let marked = mark_single_image.is_some();
                    if let Some(id) = mark_single_image {
                        attachment_tokens.mark_submit(id);
                    }
                    session_dispatch::dispatch(&state, MOBILE_CONN_ID, msg).await;
                    if !bind_ids.is_empty() {
                        if !bind_landed_attachments(&state, &bind_ids).await {
                            // 提交未落地（busy/回滚）：本回合与该附件无关。
                            attachment_tokens.clear_pending();
                        }
                    } else if marked {
                        attachment_tokens.clear_pending();
                    }
                });
            }
        }
        Ok(())
    }

    /// 注入 provider API key（内存；宿主负责 Keystore 持久化，PLAN §18）。
    pub fn set_api_key(&self, provider: &str, value: &str) {
        self.secrets.set(provider, value);
    }

    // ── 附件（多模态输入，设计 §3/§6/§7）───────────────────────────

    /// 导入附件（FFI `import_attachment` 的后端；同步签名内部 block_on，
    /// registry_update_source_json 先例——宿主须在 IO 线程调用）。
    /// 返回 `{"ok":true,"id","mime","filename","size_bytes"}` 或
    /// `{"ok":false,"error":"…"}`。**导入侧绝不发起任何网络请求**（附件
    /// 字节只在真正发起模型请求时随请求上行）；token 数由回合结束后
    /// 响应侧回填（[`crate::attachment_tokens`]，经
    /// `EntryDto::User.attachments[].tokens` 呈现）。
    pub fn import_attachment_json(
        &self,
        source_path: &str,
        declared_mime: &str,
        filename: &str,
    ) -> String {
        let t_start = std::time::Instant::now();
        crate::alog!(
            "attachment timing: import begin path={source_path:?} declared={declared_mime:?}"
        );
        let result = self.import_attachment_json_inner(source_path, declared_mime, filename);
        crate::alog!(
            "attachment timing: import end total={}ms ok={}",
            t_start.elapsed().as_millis(),
            serde_json::from_str::<serde_json::Value>(&result)
                .ok()
                .and_then(|v| v.get("ok").and_then(|b| b.as_bool()))
                .unwrap_or(false)
        );
        result
    }

    /// [`Self::import_attachment_json`] 的实现体（入口/出口耗时日志在
    /// 外层包裹）。
    fn import_attachment_json_inner(
        &self,
        source_path: &str,
        declared_mime: &str,
        filename: &str,
    ) -> String {
        let handle = {
            let guard = self.rt.lock().expect("mobile runtime lock poisoned");
            let Some(rt) = guard.as_ref() else {
                return serde_json::json!({"ok": false, "error": "runtime 已关停"}).to_string();
            };
            rt.handle().clone()
        };
        let state = self.state.clone();
        let source = source_path.to_owned();
        let mime = declared_mime.to_owned();
        let name = filename.to_owned();
        handle.block_on(async move {
            let (store, cfg) = {
                let inner = state.core.lock();
                (inner.store.clone(), inner.config.attachments)
            };
            let Some(store) = store else {
                return serde_json::json!({
                    "ok": false, "error": "存储不可用（内存库无附件支持）"
                })
                .to_string();
            };
            match crate::attachments::import_attachment(
                &store,
                &cfg,
                std::path::Path::new(&source),
                &mime,
                if name.trim().is_empty() { None } else { Some(name.as_str()) },
            )
            .await
            {
                Ok(imported) => {
                    // 成功 JSON 契约（chip 点击展开的对比信息）：
                    // `{"ok","id","mime","filename","size_bytes","orig_bytes",
                    //   "processed","orig_width","orig_height","width","height"}`；
                    // 尺寸四键图像恒在（快速通道 = 原图尺寸、转码 = 缩放
                    // 后），视频省略（嗅探层不解析容器头）；`processed` =
                    // "passthrough" | "transcode" | "video_passthrough"。
                    let mut env = serde_json::json!({
                        "ok": true,
                        "id": imported.dto.id,
                        "mime": imported.dto.mime,
                        "filename": imported.dto.filename,
                        "size_bytes": imported.size_bytes,
                        "orig_bytes": imported.orig_bytes,
                        "processed": imported.processed,
                    });
                    if let Some(w) = imported.orig_width {
                        env["orig_width"] = serde_json::json!(w);
                        env["orig_height"] =
                            serde_json::json!(imported.orig_height.expect("成对出现"));
                    }
                    if let Some(w) = imported.width {
                        env["width"] = serde_json::json!(w);
                        env["height"] = serde_json::json!(imported.height.expect("成对出现"));
                    }
                    env.to_string()
                }
                Err(e) => serde_json::json!({ "ok": false, "error": format!("{e:#}") }).to_string(),
            }
        })
    }

    /// 附件缩略图（FFI `getAttachmentThumbnail` 后端，des-2 Android
    /// lane 契约；同步签名内部 block_on，import_attachment_json 先例
    /// ——宿主须在 IO 线程调用）。id → files 原图 → 长边缩到 max_px
    /// （只缩不放）→ JPEG q80 → base64。返回
    /// `{"ok":true,"data":"<b64>","mime":"image/jpeg"}` 或
    /// `{"ok":false,"error":"…"}`（附件不存在 / 视频 / 文件缺失 /
    /// 解码失败均走 error）。
    pub fn get_attachment_thumbnail_json(&self, id: &str, max_px: i32) -> String {
        let handle = {
            let guard = self.rt.lock().expect("mobile runtime lock poisoned");
            let Some(rt) = guard.as_ref() else {
                return serde_json::json!({"ok": false, "error": "runtime 已关停"}).to_string();
            };
            rt.handle().clone()
        };
        let state = self.state.clone();
        let id = id.to_owned();
        handle.block_on(async move {
            let store = {
                let inner = state.core.lock();
                inner.store.clone()
            };
            let Some(store) = store else {
                return serde_json::json!({
                    "ok": false, "error": "存储不可用（内存库无附件支持）"
                })
                .to_string();
            };
            match crate::attachments::attachment_thumbnail_base64(&store, &id, max_px).await {
                Ok(b64) => serde_json::json!({
                    "ok": true,
                    "data": b64,
                    "mime": "image/jpeg",
                })
                .to_string(),
                Err(e) => serde_json::json!({ "ok": false, "error": format!("{e:#}") }).to_string(),
            }
        })
    }

    /// 当前会话模型的附件模态能力（§6.4 门控数据源）：
    /// image = registry 合并 meta 的 `supports_vision`；video = models.dev
    /// 条目的 input modalities（`ModelMeta` 无 video 维，见 registry.rs）。
    /// `None` = caps 未知（手填模型无元数据）→ 门控放行，provider 错误
    /// 原样透出。
    fn attachment_caps(
        model_registry: &crate::registry::RegistryStore,
        config: &openslate_core::config::OpenSlateConfig,
        alias: &str,
    ) -> (Option<bool>, Option<bool>) {
        let Ok(resolved) = openslate_core::model_config::resolve_model(config, alias) else {
            return (None, None);
        };
        let meta = model_registry.lookup_local(&resolved.model_id);
        let image = if meta.is_empty() { None } else { meta.supports_vision };
        let video = model_registry.supports_video(&resolved.model_id);
        (image, video)
    }

    /// 设置 bash 工具后端多选（逗号分隔："native"、"termux"、
    /// "native,termux"；热生效，无需重启）。命名规则（名字与后端一一
    /// 绑定）：native ⇒ `bash`（不选则 `bash` 不注册）；termux ⇒
    /// `termux_bash`。
    ///
    /// 选择点 3：热切换前重算 MCP 接管（统一谓词 [`crate::exec::
    /// mcp_owned_bash_names`]）——MCP 已接管的名字不清理、不顶替，修掉
    /// 旧实现「切后端 = 无条件 unregister 全部 bash 名」对 MCP bash
    /// 工具的误杀。
    pub fn set_exec_backends(&self, backends: &str) {
        let sel = ExecSelection::parse_csv(backends).normalized();
        self.exec_selection.set(sel);
        let mcp_owned = self.current_mcp_owned_bash_names();
        register_bash_tools(
            &self.registry,
            sel,
            self.workspace_dir.clone(),
            self.host_router.clone(),
            self.bash_env.clone(),
            &mcp_owned,
        );
        crate::alog!(
            "exec backends set to {backends:?} (native={}, termux={}, mcp_owned={mcp_owned:?})",
            sel.native,
            sel.termux
        );
    }

    /// 当前 MCP bash 接管名（统一谓词的 runtime 侧入口）：名册取
    /// `mcp_tool_names` 快照，模板读最新落盘配置（与 `reload_mcp` 同源；
    /// 读失败回空配置——无 server 时展开名集为空，仅剩「名册 ∩ bash 域」
    /// 防线）。
    fn current_mcp_owned_bash_names(&self) -> Vec<String> {
        let roster = self
            .mcp_tool_names
            .lock()
            .expect("mcp_tool_names lock poisoned")
            .clone();
        let mcp_cfg = load_config(&self.config_path)
            .ok()
            .and_then(|c| c.mcp)
            .unwrap_or_default();
        crate::exec::mcp_owned_bash_names(&roster, &mcp_cfg)
    }

    // ── MCP 外部 server 热重载（设置页增删改后免重启生效）──────────

    /// 热重载外部 MCP：读最新落盘配置 → 连新（工具先进 scratch 注册表，
    /// 批内名字冲突此时以 Err 体现，真实注册表未动、旧工具仍在）→
    /// 反注册旧名 → **bash 域让位**（新名册含 bash 名而 HostTool/native
    /// 正占着时先卸载让位，修掉撞名整体回滚死锁）→ 注册新工具（此时
    /// 撞名只可能是与非 MCP 工具冲突，Err 则回滚：清掉已注册的新名、
    /// 旧工具与让位的 bash 工具原样装回、保留旧 guard）→ 成功后整体换
    /// guard（旧 guard drop，旧连接自然关闭）→ 重算 host bash 注册
    /// （MCP 接管名让位保持 / MCP 缺席时 HostTool 兜底回装）。
    /// 完全失败（配置读不了 / 所有 enabled server 都失败且旧态非空）→
    /// 保留旧工具，返回错误。返回 JSON：
    /// `{"ok":bool,"tools":N,"servers":{"name":count,…},"failed":[…],"error":"…"}`。
    ///
    /// 锁序红线：全程不碰 `state.core`（配置路径装配期已存字段），无锁
    /// 跨 await；registry 自带内部锁，注册/反注册均为同步调用。
    pub fn reload_mcp(&self) -> String {
        let handle = {
            let guard = self.rt.lock().expect("mobile runtime lock poisoned");
            let Some(rt) = guard.as_ref() else {
                return serde_json::json!({
                    "ok": false, "tools": 0, "servers": {}, "failed": [],
                    "error": "runtime 已关停",
                })
                .to_string();
            };
            rt.handle().clone()
        };

        // 最新配置以落盘为准。mobile 装配 = 显式路径单文件语义，此处
        // load_config(active) 与装配期 load_effective_config(Some(path))
        // 等价（无全局库合并参与）。
        let mcp_cfg = match load_config(&self.config_path) {
            Ok(cfg) => cfg.mcp.unwrap_or_default(),
            Err(e) => {
                let error = format!("读取配置失败: {e:#}");
                crate::alog!("reload_mcp: {error}");
                return serde_json::json!({
                    "ok": false, "tools": 0, "servers": {}, "failed": [], "error": error,
                })
                .to_string();
            }
        };

        // 1) 连新 + scratch 注册（并发结构/日志与装配期一致；工具暂存
        //    scratch——真实注册表未动，旧工具/旧连接原样保留）。
        let scratch = openslate_core::tool::ToolRegistry::new();
        let (new_guard, counts, new_names, failed) =
            match handle.block_on(connect_external_mcp(&mcp_cfg, &scratch)) {
                Ok(v) => v,
                Err(e) => {
                    let error = format!("{e:#}");
                    crate::alog!("reload_mcp: connect failed: {error}");
                    return serde_json::json!({
                        "ok": false, "tools": 0, "servers": {}, "failed": [], "error": error,
                    })
                    .to_string();
                }
            };

        let old_names = self
            .mcp_tool_names
            .lock()
            .expect("mcp_tool_names lock poisoned")
            .clone();
        let enabled = mcp_cfg.servers.values().filter(|c| c.enabled).count();

        // 2) 完全失败判定：有 enabled server 且全军覆没、旧态非空 →
        //    保旧报错（旧工具仍可用；用户可稍后重试或重启）。
        if enabled > 0 && counts.is_empty() && !old_names.is_empty() {
            let error = format!(
                "全部 {enabled} 个 MCP server 连接失败：[{}]",
                failed.join(", ")
            );
            crate::alog!("reload_mcp: {error}");
            return serde_json::json!({
                "ok": false, "tools": 0, "servers": {}, "failed": failed, "error": error,
            })
            .to_string();
        }

        // 3) 备份旧工具 Arc（撞名回滚时原样装回；旧 guard 不动，连接
        //    仍活，回滚后一切照旧）→ 反注册旧名。
        let old_tools: Vec<(String, Arc<dyn Tool>)> = old_names
            .iter()
            .filter_map(|n| self.registry.get(n).map(|t| (n.clone(), t)))
            .collect();
        for name in &old_names {
            self.registry.unregister(name);
        }

        // 3.5) 先让位再 swap（选择点 2，修撞名回滚死锁）：新名册含 bash
        //      域名（HostTool 兜底 / native 正占着）时先卸载让位，否则
        //      try_register 必撞名 → 整体回滚 → 下次 reload 还是撞（MCP
        //      永远接管不了）。让位范围只限 bash 工具域
        //      （exec::ALL_BASH_NAMES）；其它 builtin/host 工具撞名仍是
        //      真冲突，保持回滚报错语义。让位的 Arc 备份，回滚时装回。
        let yield_names: Vec<String> = new_names
            .iter()
            .filter(|n| crate::exec::ALL_BASH_NAMES.contains(&n.as_str()))
            .filter(|n| !old_names.contains(n))
            .filter(|n| self.registry.contains(n))
            .cloned()
            .collect();
        let yielded_tools: Vec<(String, Arc<dyn Tool>)> = yield_names
            .iter()
            .filter_map(|n| self.registry.get(n).map(|t| (n.clone(), t)))
            .collect();
        for name in &yield_names {
            self.registry.unregister(name);
        }
        if !yield_names.is_empty() {
            crate::alog!("reload_mcp: host bash 工具 {yield_names:?} 让位给 MCP 接管");
        }

        // 4) 注册新工具（从 scratch 取 Arc 包装迁移；此处撞名只可能是
        //    与 builtin/host 等非 MCP 工具冲突——装配期同样致命）。迁移时
        //    逐工具包恢复装饰器（reload 期包装点，见 mcp_recovery.rs）：
        //    旧装饰器持有的 inner 是旧 McpTool，失败 → 恢复 → registry
        //    重发拿到的就是这里的新装饰器/新连接。回滚路径装回的旧 Arc
        //    本就是已包装的装饰器，不会双层包装。
        let mut registered: Vec<String> = Vec::with_capacity(new_names.len());
        for name in &new_names {
            let Some(tool) = scratch.get(name) else {
                continue;
            };
            let wrapped = Arc::new(crate::mcp_recovery::RecoveringMcpTool::new(
                tool,
                self.mcp_recovery.clone(),
            ));
            if let Err(e) = self.registry.try_register(SharedTool(wrapped)) {
                for n in &registered {
                    self.registry.unregister(n);
                }
                for (_, t) in old_tools {
                    self.registry.register_boxed(Box::new(SharedTool(t)));
                }
                // 让位的 host bash 工具原样装回：回滚路径兜底在场。
                for (_, t) in yielded_tools {
                    self.registry.register_boxed(Box::new(SharedTool(t)));
                }
                let error = format!(
                    "MCP 工具名冲突: '{}' 与非 MCP 工具撞名（已回滚，旧工具恢复）",
                    e.0
                );
                crate::alog!("reload_mcp: {error}");
                return serde_json::json!({
                    "ok": false, "tools": 0, "servers": {}, "failed": failed, "error": error,
                })
                .to_string();
            }
            registered.push(name.clone());
        }

        // 5) 换 guard（旧 guard drop → 旧连接关闭）+ 更新名册。
        {
            let mut guard_cell = self
                .mcp_external
                .lock()
                .expect("mcp_external lock poisoned");
            *guard_cell = new_guard;
        }
        // 名册接管名先算（new_names 随后被 move 进名册）。
        let mcp_owned = crate::exec::mcp_owned_bash_names(&new_names, &mcp_cfg);
        *self
            .mcp_tool_names
            .lock()
            .expect("mcp_tool_names lock poisoned") = new_names;

        // 6) swap 成功后重算 host bash 注册（同一统一谓词）：MCP 接管名
        //    保持让位；MCP 缺席（host 掉线 / 清单无 bash）时 HostTool
        //    兜底回装——已启用后端的 bash 能力在 reload 前后始终有人
        //    在场，不因重载出现空窗（未启用的后端不回装：native 关了
        //    `bash` 消失是预期，不算空窗）。运行期 host 死亡不做热切换
        //    （工具列表已进上下文，McpTool 失败语义由调用方感知），
        //    接管/兜底的交接只在 reload 边界发生。
        crate::exec::register_bash_tools(
            &self.registry,
            self.exec_selection.get(),
            self.workspace_dir.clone(),
            self.host_router.clone(),
            self.bash_env.clone(),
            &mcp_owned,
        );
        if !mcp_owned.is_empty() {
            crate::alog!("reload_mcp: MCP 接管 bash 名 {mcp_owned:?}，host 侧让位保持");
        }

        let total: usize = counts.values().sum();
        crate::alog!("reload_mcp: ok tools={total} servers={counts:?} failed={failed:?}");
        serde_json::json!({
            "ok": true,
            "tools": total,
            "servers": counts,
            "failed": failed,
        })
        .to_string()
    }

    /// 当前生效的 MCP 工具名模板（设置页回显）。读最新落盘配置——与
    /// `reload_mcp` 同源（`load_config(active)`），保证回显值 = 下次
    /// 热重载实际展开的值；未设置 / 空白 → 默认
    /// `"{prefix}_{server}__{tool}"`（core 常量引用，不写第二份字面）。
    /// 配置读不了（理论上装配期已挡）也回默认：回显接口不设错误通道，
    /// 默认值与装配期兜底一致。
    pub fn get_mcp_tool_name_template(&self) -> String {
        match load_config(&self.config_path) {
            Ok(cfg) => cfg
                .mcp
                .and_then(|m| m.tool_name_template)
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| openslate_core::mcp::DEFAULT_TOOL_NAME_TEMPLATE.to_owned()),
            Err(e) => {
                crate::alog!("get_mcp_tool_name_template: 读取配置失败: {e:#}");
                openslate_core::mcp::DEFAULT_TOOL_NAME_TEMPLATE.to_owned()
            }
        }
    }

    /// 当前生效的 builtin 工具名模板（小模板，设置页回显；Phase C 的
    /// Kotlin 侧判断「MCP 名册里的 bash 名」也经 FFI 读它）。作用于
    /// 不含 `__` 的裸上游名——host-builtin 裸名与桌面直连裸名同一条
    /// 展开分支。读最新落盘配置——与 `reload_mcp` 同源
    /// （`load_config(active)`），保证回显值 = 下次热重载实际展开的值；
    /// 未设置 / 空白 → 默认 `"{prefix}_{tool}"`。解析统一经 core 的
    /// `McpConfig::builtin_template_or_default()`（跨 crate 接缝契约，
    /// 空白语义只此一份），此处不写第二份 blank-filter、不抄字面常量。
    /// 配置读不了（理论上装配期已挡）也回默认：回显接口不设错误通道，
    /// 默认值与装配期兜底一致。
    pub fn get_mcp_builtin_tool_name_template(&self) -> String {
        match load_config(&self.config_path) {
            Ok(cfg) => match cfg.mcp {
                Some(mcp) => mcp.builtin_template_or_default().to_owned(),
                None => openslate_core::mcp::DEFAULT_BUILTIN_TOOL_NAME_TEMPLATE.to_owned(),
            },
            Err(e) => {
                crate::alog!("get_mcp_builtin_tool_name_template: 读取配置失败: {e:#}");
                openslate_core::mcp::DEFAULT_BUILTIN_TOOL_NAME_TEMPLATE.to_owned()
            }
        }
    }

    /// 写 MCP 工具名模板并落盘：空白 = 清除（回默认）。校验复用 core
    /// 的 `validate_mcp_tool_name_template`——与启动校验同一套规则，
    /// 避免 FFI 收下的值把下次启动变成 validation error。**不自动
    /// reload**：新模板对已注册工具名的生效由调用方显式走既有
    /// `reload_mcp`（热重载会按最新落盘配置重连重注册）。返回
    /// `{"ok":true}` / `{"ok":false,"error":"…"}`。
    pub fn set_mcp_tool_name_template(&self, template: String) -> String {
        let blank = template.trim().is_empty();
        if !blank {
            if let Err(message) =
                openslate_core::config::validation::validate_mcp_tool_name_template(&template)
            {
                crate::alog!("set_mcp_tool_name_template: 校验失败: {message}");
                return serde_json::json!({ "ok": false, "error": message }).to_string();
            }
        }
        // 空白归一成空串：persist 的清空语义（删键回默认）。
        let effective: &str = if blank { "" } else { template.as_str() };
        match openslate_core::config::persist::set_mcp_tool_name_template(
            &self.config_path,
            effective,
        ) {
            Ok(()) => {
                crate::alog!("set_mcp_tool_name_template: 已落盘 {effective:?}");
                serde_json::json!({ "ok": true }).to_string()
            }
            Err(e) => {
                let error = format!("写入配置失败: {e:#}");
                crate::alog!("set_mcp_tool_name_template: {error}");
                serde_json::json!({ "ok": false, "error": error }).to_string()
            }
        }
    }

    /// 写 builtin（裸名）工具名模板并落盘：空白 = 清除（回默认）。校验
    /// 复用 core 的 `validate_mcp_builtin_tool_name_template`（必须含
    /// `{tool}`、占位符仅 `{prefix}`/`{tool}`、拒 `{server}`、禁字面
    /// `__`——与启动校验同一套规则，避免 FFI 收下的值把下次启动变成
    /// validation error）。**不自动 reload**：新模板经调用方显式走既有
    /// `reload_mcp` 生效——reload 的让位/兜底重算会按新模板展开名交接
    /// HostTool。返回 `{"ok":true}` / `{"ok":false,"error":"…"}`。
    pub fn set_mcp_builtin_tool_name_template(&self, template: String) -> String {
        let blank = template.trim().is_empty();
        if !blank {
            if let Err(message) =
                openslate_core::config::validation::validate_mcp_builtin_tool_name_template(
                    &template,
                )
            {
                crate::alog!("set_mcp_builtin_tool_name_template: 校验失败: {message}");
                return serde_json::json!({ "ok": false, "error": message }).to_string();
            }
        }
        // 空白归一成空串：persist 的清空语义（删键回默认）。
        let effective: &str = if blank { "" } else { template.as_str() };
        match openslate_core::config::persist::set_mcp_builtin_tool_name_template(
            &self.config_path,
            effective,
        ) {
            Ok(()) => {
                crate::alog!("set_mcp_builtin_tool_name_template: 已落盘 {effective:?}");
                serde_json::json!({ "ok": true }).to_string()
            }
            Err(e) => {
                let error = format!("写入配置失败: {e:#}");
                crate::alog!("set_mcp_builtin_tool_name_template: {error}");
                serde_json::json!({ "ok": false, "error": error }).to_string()
            }
        }
    }

    /// mobile 聚合 host 在 toml 里的默认 server 键名（App 一键安装 /
    /// applyServers 经 upsert_mcp_server 写入的名字）。get 的多键回退
    /// 与 set 的"当前名"都锚定它。占位符模型上它是 `{prefix}` 的值
    /// （`{server}` 留给上游聚合名里的服务器段）。
    const DEFAULT_MCP_SERVER_NAME: &str = "termux";

    /// 当前 mobile 聚合 host 的 server 名（`[mcp.servers]` 的键，即
    /// 模板 `{prefix}` 占位符的展开值；设置页回显）。读最新落盘配置，
    /// 与 `reload_mcp` 同源：恰好一个键 → 该键；多键 → `termux` 优先、
    /// 否则任取一个并 alog 一行（多键是异常态：本 FFI 面向单 server
    /// 聚合场景，多键说明用户手改过 toml）；空 / 读失败 → 默认
    /// `termux`（回显接口不设错误通道，默认值与 App 一键安装写入的
    /// 键一致，行为可预期）。
    pub fn get_mcp_server_name(&self) -> String {
        let servers = match load_config(&self.config_path) {
            Ok(cfg) => cfg.mcp.map(|m| m.servers).unwrap_or_default(),
            Err(e) => {
                crate::alog!("get_mcp_server_name: 读取配置失败: {e:#}");
                return Self::DEFAULT_MCP_SERVER_NAME.to_owned();
            }
        };
        match servers.len() {
            0 => Self::DEFAULT_MCP_SERVER_NAME.to_owned(),
            1 => servers
                .keys()
                .next()
                .expect("len == 1 guarantees a key")
                .clone(),
            n => {
                // 多键异常态：优先回显 termux（App 管的那个），否则任取
                // 一个——回显要给出一个确定值，但绝不猜着改名。
                let fallback = servers
                    .keys()
                    .find(|k| k.as_str() == Self::DEFAULT_MCP_SERVER_NAME)
                    .or_else(|| servers.keys().next())
                    .expect("len > 0 guarantees a key")
                    .clone();
                crate::alog!(
                    "get_mcp_server_name: [mcp.servers] 有 {n} 个键（异常态，应为单 server 聚合），回显 '{fallback}'"
                );
                fallback
            }
        }
    }

    /// 改 mobile 聚合 host 的 server 名（`[mcp.servers]` 键名 = 模板
    /// `{prefix}` 占位符的展开值；改名后工具注册名前缀、PTC namespace
    /// 随之变化——经既有 `reload_mcp` 热生效，此处**不自动 reload**，
    /// 调用方显式调 reloadMcpServers）。三步：
    /// 1. 校验：复用 core `validate_mcp_server_name`（非空、
    ///    [A-Za-z0-9_.-]、不含 `__`——键名会泄入 legacy
    ///    `prefix_upstream` 路径与按 `__` 分段的 UI 启发式，须保持
    ///    `__`-free）；
    /// 2. 与当前名相同 → 幂等 `{"ok":true}`（不落盘）；
    /// 3. `rename_mcp_server` 落盘搬移（transport/headers 原样）。
    /// 返回 `{"ok":true}` / `{"ok":false,"error":"…"}`（空表/键缺失
    /// 由 persist 报错上抛：应先经 App 一键安装 upsert 建 server）。
    pub fn set_mcp_server_name(&self, name: String) -> String {
        if let Err(message) = openslate_core::config::validation::validate_mcp_server_name(&name) {
            crate::alog!("set_mcp_server_name: 校验失败: {message}");
            return serde_json::json!({ "ok": false, "error": message }).to_string();
        }
        let current = self.get_mcp_server_name();
        if current == name {
            // 幂等：与当前名相同（含首键就是 termux 的默认态）→ 直接
            // 成功，不动文件、不触发 reload。
            return serde_json::json!({ "ok": true }).to_string();
        }
        match openslate_core::config::persist::rename_mcp_server(&self.config_path, &current, &name)
        {
            Ok(()) => {
                crate::alog!("set_mcp_server_name: {current:?} → {name:?} 已落盘");
                serde_json::json!({ "ok": true }).to_string()
            }
            Err(e) => {
                let error = format!("重命名失败: {e:#}");
                crate::alog!("set_mcp_server_name: {error}");
                serde_json::json!({ "ok": false, "error": error }).to_string()
            }
        }
    }

    // ── 实验开关（设置页「实验性功能」；存储 v5 §3.5）────────────────

    /// 读实验视频开关当前生效值。读**内存配置**（`CoreInner.config`）：
    /// 装配期从落盘 openslate.toml 装载、[`Self::set_experimental_video`]
    /// 写入时同步更新——内存值即 Submit 门控实际消费的值，回显与行为
    /// 恒一致（get_mcp_* 系列读盘是因为它们的生效走显式 reload；本开
    /// 关没有 reload 概念，内存即真相）。
    pub fn get_experimental_video(&self) -> bool {
        let value = self.state.core.lock().config.attachments.experimental_video;
        crate::alog!("get_experimental_video: {value}");
        value
    }

    /// 写实验视频开关（FFI `set_experimental_video` 的 mobile 侧实现，
    /// L4→L6 交接；签名与 L5 已约定不可变）。两步：
    /// 1. **落盘**：toml_edit 原地改写 openslate.toml
    ///    `[attachments] experimental_video`（其余键/注释/格式零扰动；
    ///    无该节则追加）。实现在本 crate（`persist_experimental_video`）
    ///    而非 core persist.rs——core 侧不在本 lane 写权限内，语义与
    ///    persist 层同一套守卫（非表节点拒绝改写）；
    /// 2. **内存生效**：同步更新 `CoreInner.config.attachments`——
    ///    Submit 附件门控（`gate_submit_attachments`）读内存值，下一轮
    ///    提交即按新开关放行/拒绝视频附件，无需重启。
    /// 落盘失败 → `{"ok":false,"error":"…"}` 且**不动内存**（避免内存
    /// 与磁盘分叉：重启后开关回跳旧值比「看似生效实未持久」可预期）。
    pub fn set_experimental_video(&self, enabled: bool) -> String {
        crate::alog!("set_experimental_video({enabled})");
        if let Err(e) = persist_experimental_video(&self.config_path, enabled) {
            let error = format!("写入配置失败: {e:#}");
            crate::alog!("set_experimental_video: {error}");
            return serde_json::json!({ "ok": false, "error": error }).to_string();
        }
        self.state.core.lock().config.attachments.experimental_video = enabled;
        crate::alog!("set_experimental_video: 已落盘并即时生效 ({enabled})");
        serde_json::json!({ "ok": true }).to_string()
    }

    /// 读图片处理双旋钮当前生效值（FFI `get_image_processing` 的 mobile
    /// 侧）。读**内存配置**（与 `get_experimental_video` 同理：装配期从
    /// 落盘 openslate.toml 装载、setter 写入时同步更新——归一化/快速
    /// 通道消费的就是内存值，回显与行为恒一致）。
    /// 返回 `{"max_image_dim":N,"max_image_mb":M}`（u32/u64）。
    pub fn get_image_processing(&self) -> String {
        let att = self.state.core.lock().config.attachments;
        crate::alog!("get_image_processing: dim={} mb={}", att.max_image_dim, att.max_image_mb);
        serde_json::json!({
            "max_image_dim": att.max_image_dim,
            "max_image_mb": att.max_image_mb,
        })
        .to_string()
    }

    /// 写图片处理双旋钮（FFI `set_image_processing`；设置页「图片最长
    /// 边 / 处理后大小上限」）。语义见 [`crate::attachments::
    /// normalize_image`]：等比缩放目标边长 + 迭代压缩（q85→70→55 →
    /// ×0.85 缩边）达标线。三步：
    /// 1. **值域校验**（core validation 同源：512 ≤ dim ≤ 6000、
    ///    1 ≤ mb ≤ 5）——非法值 `{"ok":false,"error":…}` 且不落盘；
    /// 2. **落盘**：toml_edit 原地改写 `[attachments]` 两键（其余键/
    ///    注释/格式零扰动；无该节则追加）；
    /// 3. **内存生效**：同步更新 `CoreInner.config.attachments`——
    ///    下一次 `import_attachment` 即按新旋钮走快速通道/迭代压缩，
    ///    无需重启。落盘失败 → `{"ok":false}` 且不动内存（内存与磁盘
    ///    不分叉，同 set_experimental_video 语义）。
    pub fn set_image_processing(&self, max_dim: i64, max_mb: i64) -> String {
        crate::alog!("set_image_processing(dim={max_dim}, mb={max_mb})");
        // 值域校验走 core validation（单一真相源）：构造临时配置跑
        // attachments 边界检查，错误逐条拼进 error。
        let candidate = openslate_core::config::AttachmentsConfig {
            max_image_dim: max_dim.try_into().unwrap_or(u32::MAX),
            max_image_mb: max_mb.try_into().unwrap_or(u64::MAX),
            ..openslate_core::config::AttachmentsConfig::default()
        };
        let errors =
            openslate_core::config::validation::attachments_floor_errors(&candidate);
        if !errors.is_empty() {
            let msg = errors
                .iter()
                .map(|e| e.message.clone())
                .collect::<Vec<_>>()
                .join("; ");
            crate::alog!("set_image_processing: 非法值：{msg}");
            return serde_json::json!({ "ok": false, "error": msg }).to_string();
        }
        let (dim, mb) = (max_dim as u32, max_mb as u64);
        if let Err(e) = persist_image_processing(&self.config_path, dim, mb) {
            let error = format!("写入配置失败: {e:#}");
            crate::alog!("set_image_processing: {error}");
            return serde_json::json!({ "ok": false, "error": error }).to_string();
        }
        {
            let mut inner = self.state.core.lock();
            inner.config.attachments.max_image_dim = dim;
            inner.config.attachments.max_image_mb = mb;
        }
        crate::alog!("set_image_processing: 已落盘并即时生效 (dim={dim}, mb={mb})");
        serde_json::json!({ "ok": true }).to_string()
    }

    // ── 工具可见性（工具管理页）──────────────────────────────────────

    /// 全部已注册工具清单（JSON 数组，按 name 排序；宿主渲染工具管理页，
    /// 展开项展示详细参数）：
    /// `[{"name":"bash","description":"…","input_schema":{…},"server":…}, …]`——
    /// `input_schema` 为该工具参数的 JSON Schema 对象原样（无 schema 时
    /// 为 `{}`，键恒存在）；`server` 为审批作用域标识（tool-approval
    /// P1b：聚合形态 = 下游 slug 如 `"github"`，host 内置裸名 = host 前
    /// 缀，非 MCP 工具为 JSON null）——Android 设置页「工具权限」按它
    /// 分组（有则用）。
    pub fn list_tools_json(&self) -> String {
        let mut defs = self.registry.definitions();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        serde_json::json!(defs
            .iter()
            .map(|d| {
                // 参数 schema 原样透传（ToolDefinition.parameters，即各工具
                // parameters_schema() 的 JSON Schema 对象）；工具无 schema
                // （null）时恒输出空对象，键永远存在。
                let input_schema = if d.parameters.is_null() {
                    serde_json::json!({})
                } else {
                    d.parameters.clone()
                };
                // 审批作用域反解走 core 权威实现（namespace = [mcp.servers]
                // 键；native / in-process builtin = None → null）。
                let server = self
                    .registry
                    .get(&d.name)
                    .and_then(|tool| tool.namespace())
                    .and_then(|ns| openslate_core::mcp::server_slug_of(&d.name, Some(&ns)));
                serde_json::json!({
                    "name": d.name,
                    "description": d.description,
                    "input_schema": input_schema,
                    "server": server,
                })
            })
            .collect::<Vec<_>>())
        .to_string()
    }

    /// 当前生效 agent（root）的工具可见性（JSON 对象）：
    /// `{"tools":[…],"hidden_tools":[…]}`——frontmatter 两份名单原样
    /// 回读（含 glob 模式，顺序保持落盘顺序）。
    pub fn get_agent_tool_visibility_json(&self) -> String {
        let (tools, hidden_tools) = {
            let inner = self.state.core.lock();
            let root = inner.agent_tree.get_root();
            (root.tools.clone(), root.hidden_tools.clone())
        };
        serde_json::json!({ "tools": tools, "hidden_tools": hidden_tools }).to_string()
    }

    /// 全量覆写 root agent 的 `tools:` / `hidden_tools:` 并热生效：
    /// 外科手术式改写 `agents/<root>.md` frontmatter（其余字段与正文
    /// 逐字节保留）→ 重装 agent 树 → 热替换（manager 在家就换；回合
    /// 进行中改 `inner.agent_tree`，引擎回收时自动对齐）。下一对话回
    /// 合即按新可见性过滤，无需重启 app。
    /// 返回 `{"ok":true}` 或 `{"ok":false,"error":"…"}`。
    pub fn set_agent_tool_visibility_json(
        &self,
        tools: Vec<String>,
        hidden_tools: Vec<String>,
    ) -> String {
        match self.set_agent_tool_visibility_impl(tools, hidden_tools) {
            Ok(()) => serde_json::json!({ "ok": true }).to_string(),
            Err(e) => {
                crate::alog!("set_agent_tool_visibility failed: {e:#}");
                serde_json::json!({ "ok": false, "error": format!("{e:#}") }).to_string()
            }
        }
    }

    fn set_agent_tool_visibility_impl(
        &self,
        tools: Vec<String>,
        hidden_tools: Vec<String>,
    ) -> Result<()> {
        // 1) 定位 root agent 的 md（frontmatter `id:` 匹配，`<id>.md`
        //    文件名兜底）。agents 目录 = 生效配置同级的 `agents/`。
        let (agents_dir, root_id) = {
            let inner = self.state.core.lock();
            let active = inner.paths.active.clone();
            let root_id = inner.agent_tree.get_root().id.0.clone();
            (
                active
                    .parent()
                    .context("生效配置路径无父目录")?
                    .join("agents"),
                root_id,
            )
        };
        let root_md =
            find_agent_markdown(&agents_dir, &root_id).context("定位 root agent md 失败")?;

        // 2) 外科手术式落盘。
        openslate_core::config::persist::set_agent_tool_lists(&root_md, &tools, &hidden_tools)?;

        // 3) 重装 agent 树 + 热替换。重装失败不回滚磁盘（与
        //    config_ops 落盘语义一致：磁盘已改，运行时保留旧树并报错）。
        let agents =
            openslate_core::config::parse_agents_dir(&agents_dir).context("重装 agents 失败")?;
        let tree = openslate_core::agent_tree::AgentTree::from_configs(&agents.agents)
            .context("重建 agent 树失败")?;
        {
            let mut inner = self.state.core.lock();
            inner.agents_cfg = agents;
            inner.agent_tree = tree.clone();
            if let Some(manager) = inner.manager.as_mut() {
                manager.agent_tree = tree;
            }
        }
        crate::alog!(
            "tool visibility updated: tools={:?} hidden={:?} → {}",
            tools,
            hidden_tools,
            root_md.display()
        );
        Ok(())
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
        // C1：关停前把当前 run 的转写尾部强制落库（force——含回合中推迟
        // 的 ToolCall 中断快照 + INCOMPLETE_TURN_META 收尾行）。必须在
        // 下方 rt.take() 之前同步完成：此后无 runtime 可执行异步，内存
        // 转写随收束蒸发。
        {
            let handle = {
                let guard = self.rt.lock().expect("mobile runtime lock poisoned");
                // 幂等标志已挡住重入；rt 必然还在（take 在本块之后）。
                guard
                    .as_ref()
                    .expect("runtime already shut down")
                    .handle()
                    .clone()
            };
            handle.block_on(self.sync.flush_now(&self.state, true));
        }
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
        let rt = self.rt.lock().expect("mobile runtime lock poisoned").take();
        if let Some(rt) = rt {
            rt.shutdown_timeout(Duration::from_secs(5));
        }
    }

    fn emit_snapshot(&self) {
        // ask_user 待答清单回填（重启恢复回放；build_snapshot 的 server
        // 投影恒空，mobile 装配侧按问卷持久层回填——协议 §5）。关停后
        // （rt 已收束）跳过查询，恒空。锁序对齐同文件其他 block_on 调
        // 用点：先取 handle 放锁，再等异步（不持 rt 槽位锁跨 await）。
        let pending_questions = {
            let handle = {
                let guard = self.rt.lock().expect("mobile runtime lock poisoned");
                guard.as_ref().map(|rt| rt.handle().clone())
            };
            match handle {
                Some(handle) => handle.block_on(self.ask.snapshot_dtos()),
                None => Vec::new(),
            }
        };
        let mut snap = {
            let inner = self.state.core.lock();
            build_snapshot(&inner, &self.state.approval)
        };
        snap.pending_questions = pending_questions;
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

    /// 按 call_id 取该工具调用的完整输出（内存 history 直读 + 长输出
    /// 落盘全文闭环，纯同步）。
    ///
    /// 数据流：ToolDetailDialog 打开时 Kotlin 经 FFI 调用此函数，把 UI
    /// 里的多层截断 preview（tool_end 事件 512 字符 / 恢复 2000 字符 /
    /// 折叠 8KB）替换为 DB/内存 history 里 max_output_bytes 上限内的
    /// 全量版本。命中条件：history 中存在 `role==Tool` 且
    /// `tool_call_id == call_id` 的消息。
    ///
    /// 长输出闭环（C6，零新 FFI 面）：命中消息的 content 若带
    /// `[STORED OUTPUT ref="out-…"` 标记（core 分叉点替换的落盘预览），
    /// 按 `(当前 run, ref)` 反查 `tool_outputs` 行（严格会话隔离）→ 读
    /// `files/outputs/{sha}.zst` 解压 → 校验 `len == orig_bytes` → 返回
    /// 全文（封顶 512 KiB：头 448 KiB + 省略行 + 尾 64 KiB，UTF-8 字符
    /// 边界安全截断）。行缺失 / 文件缺失 / 解压失败 / 长度不符 / 无
    /// 标记 → **返回 content 原文**（预览本身可读，现降级语义不变）。
    ///
    /// `None`（miss）场景与降级：call_id 为空/未知；history 被
    /// auto_compact 换成摘要；call_agent 子代理的工具（子代理 history
    /// 不在顶层会话）——调用方回退显示原 preview。**None 语义未随 C6
    /// 改变**。
    pub fn tool_output_by_call_id(&self, call_id: String) -> Option<String> {
        if call_id.is_empty() {
            return None;
        }
        let (content, run_id) = {
            let inner = self.state.core.lock();
            let msg = inner
                .history
                .iter()
                .find(|m| m.role == MessageRole::Tool && m.tool_call_id.as_ref().is_some_and(|id| id.0 == call_id))?;
            (
                msg.content.clone(),
                inner.session_run.as_ref().map(|r| r.run_id.0.clone()),
            )
        }; // 先放 core 锁：慢路径（store 查询 + 读文件）不持锁。
        Some(self.expand_stored_output(content, run_id.as_deref()))
    }

    /// 长输出全文展开（C6）：content 带落盘标记则取回全文（封顶截断），
    /// 任何失败回落 content 原文。
    fn expand_stored_output(&self, content: String, run_id: Option<&str>) -> String {
        let Some(ref_id) = parse_stored_output_ref(&content) else {
            return content; // 无标记：普通（截断）输出，原文返回
        };
        // 无当前 run（会话未开 run / 已关闭）→ 无处可查（严格会话隔离，
        // 不回落其他 run 的行），回落预览。
        let Some(run_id) = run_id else {
            tracing::debug!(
                target: "openslate_mobile",
                ref_id,
                "stored output row lookup skipped: no active run; fallback to preview"
            );
            return content;
        };
        // store 未装配（无 DB 配置）→ 回落。
        let Some(store) = self.state.core.lock().store.clone() else {
            return content;
        };
        // rt 已收束（shutdown 后）→ 无异步执行环境，回落。
        let handle = {
            let guard = self.rt.lock().expect("mobile runtime lock poisoned");
            let Some(rt) = guard.as_ref() else {
                return content;
            };
            rt.handle().clone()
        };
        let expanded = handle.block_on(async move {
            // 行缺失（ref 失效 / run 已删 / ref 属其他会话）→ None → 回落
            let Some(record) = store
                .find_tool_output(run_id, ref_id)
                .await
                .ok()
                .flatten()
            else {
                tracing::debug!(target: "openslate_mobile", ref_id, "stored output row missing; fallback to preview");
                return None;
            };
            // 文件缺失（崩溃窗口 / 手动清理）→ None → 回落
            let Some(zst) = store.read_tool_output_bytes(&record.sha256).await.ok().flatten() else {
                tracing::debug!(target: "openslate_mobile", sha = %record.sha256, "stored output file missing; fallback to preview");
                return None;
            };
            // 解压 + UTF-8 双失败 → 回落
            let text = zstd::decode_all(zst.as_slice()).ok()?;
            let text = String::from_utf8(text).ok()?;
            // 完整性校验：解压长度必须与落盘时的 orig_bytes 一致（防半写
            // 文件 / 内容错位静默展示）
            if text.len() != record.orig_bytes as usize {
                tracing::warn!(
                    target: "openslate_mobile",
                    ref_id,
                    decoded = text.len(),
                    recorded = record.orig_bytes,
                    "stored output size mismatch; falling back to preview"
                );
                return None;
            }
            Some(cap_stored_full_text(text))
        });
        expanded.unwrap_or(content)
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
                Ok(Ok(entries)) => {
                    serde_json::json!({ "ok": true, "entries": entries }).to_string()
                }
                Ok(Err(e)) => {
                    serde_json::json!({ "ok": false, "error": format!("{e:#}") }).to_string()
                }
                Err(_) => {
                    serde_json::json!({ "ok": false, "error": "更新超时（120s）" }).to_string()
                }
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
                    // run 级 token 聚合（存储 v5 §2；存量 run 无采集 = 0）。
                    "input_tokens": r.input_tokens.max(0),
                    "output_tokens": r.output_tokens.max(0),
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
        // C1：切换前把当前 run 的未落库尾部强制落库（force——含回合中
        // 推迟的 ToolCall 中断快照）。此刻 busy=false（守卫已过），无
        // 并发 submit；当前 run 的内存转写即将被换出，不刷即丢。
        let sync = self.sync.clone();
        handle.block_on(async move {
            sync.flush_now(&state, true).await;
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
            // 保留悬挂加载 + 手动执行 load_messages 的经典丢弃（尾部带
            // 未答 tool_calls 的 assistant）——手动是为了拿到"丢弃发生"
            // 的显式信号（§5：丢弃要显性化为转写 Meta 行）。
            let mut messages =
                openslate_store_sqlite::recorder::RunRecorder::load_messages_keep_dangling(
                    &store, &run.id,
                )
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut popped_dangling = false;
            if matches!(
                messages.last(),
                Some(m) if m.role == MessageRole::Assistant
                    && m.tool_calls.as_ref().is_some_and(|tcs| !tcs.is_empty())
            ) {
                messages.pop();
                popped_dangling = true;
                // C2 pop 回写 DB（修「只改内存不回写 → 反复重开反复 pop、
                // ⚡Meta 行累积」）：删被 pop 的 messages 尾行（load 返回
                // 不带 seq，按 max seq 语义删尾部）+ 全删该 run 的
                // transcript_entries 行（重建走消息投影，无悬挂条目），
                // 再失效 SyncState 游标——sync_loop 下周期自举 = 0，把
                // 完整重建转写全量写回（幂等；悬挂是罕见路径，一次全量
                // 重写可接受）。删行失败 best-effort（下次重开再试）；
                // 删干净后第二次 open 不再命中本分支（G-1）。
                match store.max_message_seq(&run.id).await {
                    Ok(max_seq) => {
                        match store.delete_messages_from_seq(&run.id, max_seq).await {
                            Ok(n) => crate::alog!(
                                "open_session: pop 回写删除 run {} 尾部消息 {n} 行",
                                run.id
                            ),
                            Err(e) => crate::alog!(
                                "open_session: 删除悬挂尾消息失败（非致命）: {e}"
                            ),
                        }
                    }
                    Err(e) => {
                        crate::alog!("open_session: 查询 max seq 失败（非致命）: {e}")
                    }
                }
                if let Err(e) = store.delete_transcript_entries_for_run(&run.id).await {
                    crate::alog!("open_session: 清空转写条目行失败（非致命）: {e}");
                }
                sync.invalidate(&run.id).await;
                tracing::warn!(
                    target: "openslate_store",
                    "run {}: dropped trailing dangling assistant on open_session (已回写 DB 删除)",
                    run.id
                );
            }
            // ── 存量 run 标题治愈 ──
            // 旧版本的 run 标题是占位值（None / "mobile session"），历史
            // 会话列表只能显示占位。messages 已在手，零额外查询取首条
            // user 消息前 40 字符补写回库——治愈后的标题随 list_sessions
            // 直接从 run.title 读出。新 run 建立时已直接写入 prompt 摘
            // 要，不会命中本分支。
            if run.title.as_deref().is_none_or(|t| t == "mobile session") {
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
                store.clone(),
                openslate_core::types::RunId(run.id.clone()),
                &root_agent_id,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            // ── 切换前收束旧 run ──
            // 对旧 run 而言本次切换是被打断 → "interrupted"（与
            // NewSession 的 "completed" 语义不同）；不收束则会留下永远
            // running 的僵尸记录。同一 run 重开无需收束（状态无谓抖动）。
            // C2：same_run 判定上提——供会话身份轮换复用（!same_run 时
            // 换 session_id + 清会话作用域审批白名单）。
            // 错误仅告警，不阻断切换。
            let (old_run, old_cost, same_run) = {
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
                (old_run, inner.stats.total_cost_usd, same_run)
            }; // guard 随块结束释放，之后的 await 不持锁。
            if let Some(old_run) = old_run {
                if let Err(e) = old_run.recorder.finish("interrupted", None, old_cost).await {
                    crate::alog!("open_session: finish previous run failed (non-fatal): {e}");
                }
            }
            // C2：会话身份轮换（!same_run）——换 run 即换会话。session_id
            // 变化驱动 ChatScreen 的会话揭示；会话作用域审批白名单
            // （approve_all / session_mcp 写入的内存集合）随之清空：A 会
            // 话的 approve_all 不能泄漏到 B。锁序对齐 NewSession 先例
            // （session.rs）：clear_session_scopes 在 core 锁外调用（先
            // bridge 后 core）。同一 run 重开不轮换不清（会话未换）。
            if !same_run {
                state.approval.clear_session_scopes();
            }
            // 转写两级重建（设计 §4）：优先 transcript_entries 行（v5 真
            // 相源——Approval/Meta/StepBreak 等投影拿不到的条目都在）；
            // 空（v5 前的 run）→ protocol 共享投影回退（L6 已切换：
            // `project_from_messages`，经 runtime 侧薄委托调用）。pop 过
            // 且非空 → 追加 Meta 显性化（§5）。
            let mut transcript =
                crate::transcripts::rebuild_transcript(&store, &run.id, &messages).await;
            if popped_dangling && !transcript.is_empty() {
                transcript.push(EntryDto::Meta {
                    text: crate::transcripts::DANGLING_TAIL_META.to_owned(),
                });
            }
            // 附件 resume 回填（设计 §6.1）：历史图随每轮重发，总字节封
            // 顶 max_inline_total_mb。
            let attachments_cfg = state.core.lock().config.attachments;
            crate::attachments::refill_message_attachment_data(
                &store,
                &attachments_cfg,
                &mut messages,
            )
            .await;
            let cost = run.cost_usd;
            {
                let mut inner = state.core.lock();
                // C2：会话身份轮换（!same_run）——ChatScreen 的会话揭示
                // 依赖 session_id 变化；同一 run 重开保持不变。
                if !same_run {
                    inner.session_id = session_dispatch::new_session_id();
                }
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

/// Submit 附件门控（设计 §3.3/§3.4/§6.4；`send` 的 spawn 内调用）。
///
/// 非 Submit 或无附件 → 原样放行（`bind_ids` 空）。带附件：
/// 1. 空文本拒绝（session 侧空文本静默丢弃，用户会无感知——fail fast）；
/// 2. 逐 id 加载 attachments 行（不存在/已过期拒绝）+ sha256 去重；
/// 3. 矩阵门控（[`crate::attachments::gate_submit_attachments`]）。
///
/// 返回 `Some((msg, bind_ids))` = 放行（msg 原样进 dispatch；ids 待
/// run 落定后 bind）；`None` = 已 notice 拒绝。
async fn gate_submit_attachments(
    state: &Arc<AppState>,
    sink: &Arc<EventSink>,
    model_registry: &Arc<crate::registry::RegistryStore>,
    msg: ClientMsg,
) -> Option<(ClientMsg, Vec<String>)> {
    let ClientMsg::Submit {
        text,
        attachments,
    } = msg
    else {
        return Some((msg, Vec::new()));
    };
    let Some(dtos) = attachments else {
        return Some((
            ClientMsg::Submit {
                text,
                attachments: None,
            },
            Vec::new(),
        ));
    };
    if dtos.is_empty() {
        // 空数组 ≡ 无附件：剥字段转发，session 侧少一层无谓构造。
        return Some((
            ClientMsg::Submit {
                text,
                attachments: None,
            },
            Vec::new(),
        ));
    }
    let notice = |text: String| {
        sink.broadcast(ServerMsg::Notice {
            text,
            level: NoticeLevel::Error,
        });
    };
    if text.trim().is_empty() {
        notice("请为附件输入文字说明后再发送".into());
        return None;
    }
    let (store, config, alias) = {
        let inner = state.core.lock();
        (
            inner.store.clone(),
            inner.config.clone(),
            inner.model_alias.clone(),
        )
    };
    let Some(store) = store else {
        notice("存储不可用，无法发送附件".into());
        return None;
    };
    let mut records = Vec::with_capacity(dtos.len());
    for dto in &dtos {
        match store.get_attachment(&dto.id).await {
            Ok(Some(row)) => records.push(row),
            _ => {
                notice(format!(
                    "附件不存在或已过期，请重新添加（id={}）",
                    dto.id
                ));
                return None;
            }
        }
    }
    let records = crate::attachments::dedupe_records(records);
    let (image_cap, video_cap) =
        MobileRuntime::attachment_caps(model_registry, &config, &alias);
    if let Err(reason) = crate::attachments::gate_submit_attachments(
        &config.attachments,
        &records,
        image_cap,
        video_cap,
    ) {
        crate::alog!("submit gate: 拒绝附件提交：{reason}");
        notice(reason);
        return None;
    }
    let ids = records.iter().map(|r| r.id.clone()).collect();
    Some((
        ClientMsg::Submit {
            text,
            attachments: Some(dtos),
        },
        ids,
    ))
}

/// dispatch 返回后（引擎已 spawn、run 已落定）把本批附件绑定到当前 run
/// （`attachments.run_id` NULL → run id；设计 §3.1）。绑定成功的判定：
/// history 尾部是刚落地的带附件 user 消息（数量一致）——busy 拒绝 /
/// provider 构建失败回滚等未落地路径不绑定（暂存行留给下次或 7 天 GC）。
/// 返回是否落地（false 时调用方清理附件 token 回填标记）。
async fn bind_landed_attachments(state: &Arc<AppState>, ids: &[String]) -> bool {
    let (store, run_id, landed) = {
        let inner = state.core.lock();
        let landed = matches!(inner.history.last(), Some(m) if m.role == MessageRole::User
            && m.attachments.as_ref().is_some_and(|a| a.len() == ids.len()));
        (
            inner.store.clone(),
            inner.session_run.as_ref().map(|r| r.run_id.0.clone()),
            landed,
        )
    };
    if !landed {
        crate::alog!("attachments: 提交未落地（busy/回滚），跳过 bind（{ids:?}）");
        return false;
    }
    let (Some(store), Some(run_id)) = (store, run_id) else {
        crate::alog!("attachments: 无 store/run，跳过 bind（{ids:?}）");
        return false;
    };
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    match store.bind_attachments(&refs, &run_id).await {
        Ok(n) => crate::alog!("attachments: {n} 个附件已绑定到 run {run_id}"),
        Err(e) => crate::alog!("attachments: bind 失败（非致命，行保持暂存）: {e}"),
    }
    true
}

// ── 长输出全文闭环（C6；常量与解析/封顶助手）────────────────────────

/// 详情弹窗全文封顶：512 KiB = 头 448 KiB + 省略行 + 尾 64 KiB（弹出层
/// 一次性可读上限；更大规模交给 output_query 切片）。
const STORED_FULL_TEXT_CAP: usize = 512 * 1024;
const STORED_FULL_TEXT_HEAD: usize = 448 * 1024;
const STORED_FULL_TEXT_TAIL: usize = 64 * 1024;
/// 封顶省略行文案（与 core 预览省略行同风格）。
const STORED_FULL_TEXT_OMISSION: &str = "[... use output_query for the rest ...]";

/// 字节预算内的最大字符边界下标（向后回退；对齐 core output_store 与
/// mcp-builtin shell.rs 的 head 截断先例：劈进多字节字符就退到前一边界）。
fn floor_char_boundary(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// 字节预算内的最小字符边界下标（向前推进，tail 截断先例）。
fn ceil_char_boundary(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// 超限全文封顶：头 448 KiB + 省略行 + 尾 64 KiB（字符边界安全）。
///
/// 调用前提 `text.len() > CAP`（= HEAD + TAIL，头尾切点必然不重叠）。
fn cap_stored_full_text(text: String) -> String {
    if text.len() <= STORED_FULL_TEXT_CAP {
        return text;
    }
    let head_end = floor_char_boundary(&text, STORED_FULL_TEXT_HEAD);
    let tail_start = ceil_char_boundary(&text, text.len() - STORED_FULL_TEXT_TAIL);
    format!(
        "{}\n{}\n{}",
        &text[..head_end],
        STORED_FULL_TEXT_OMISSION,
        &text[tail_start..]
    )
}

/// 从 Tool 消息 content 解析首个落盘标记的 ref（完整 `out-…` 形态）。
///
/// 头行格式（core `build_preview` 冻结）：
/// `[STORED OUTPUT ref="out-{1..16 位数字}" size=… lines=…]`。ref 是
/// 会话内自增短 id（`out-3`）；只收数字形态（开发期拍板：不做旧
/// 16hex 兼容，含字母一律不解析 → 回落预览）。
/// 解析用 core 的 `STORED_OUTPUT_MARKER` 常量定位 + 前缀逐段剥（不用
/// 正则：格式冻结、单一形态，手工解析零依赖且更快）。无标记 / 形状
/// 不符 → `None`。
fn parse_stored_output_ref(content: &str) -> Option<&str> {
    let marker = openslate_core::output_store::STORED_OUTPUT_MARKER;
    let start = content.find(marker)?;
    let rest = content[start + marker.len()..].trim_start();
    let after = rest.strip_prefix("ref=\"out-")?;
    // ref 以闭引号终结（短 id 如 out-3 的长度不固定）
    let end = after.find('"')?;
    let digits = &after[..end];
    if !digits.is_empty()
        && digits.len() <= 16
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        // 返回完整 ref（含 out- 前缀，与 store 的 ref 列同构）：
        // rest 此刻以 `ref="out-…` 开头，`"ref=\""` 占 5 字节。
        Some(&rest[5..5 + 4 + end])
    } else {
        None
    }
}

/// 状态装配（server `build_state` 的 mobile 变体：origin / 显式路径 /
/// 注入 factory）。`run_retention` = 启动保留策略（见
/// [`RuntimeOptions::run_retention`]）。
/// 返回 `(AppState, 外部 MCP guard, 外部 MCP 工具名)`——后两者由
/// `MobileRuntime` 接手（热重载整体替换），builtin 句柄留在
/// `AppState._mcp` 保活。
/// 重启恢复 pop 兜底的守卫：尾部悬挂 assistant（带未答 `tool_calls` 的
/// 最后一条消息）只有在**没有任何 pending 问卷行引用其声明的
/// tool_call** 时才允许丢弃（经典「模型重新决策」语义）。
///
/// 判定以 store 的 pending 行为准，而非调用方（ask_user 启动清扫）返回
/// 的 recoverable 分类——两者可能存在偏差（行属于本 run 却未被认作
/// recoverable 的细节场景）。有引用时必须保留尾部悬挂：那是迟到答案
/// （question_reply → `append_late_tool_result`）与 TTL 过期合成收尾
/// 的补全点，pop 即不可逆摧毁。
fn may_pop_dangling_tail(history: &[Message], pending_tool_call_ids: &[String]) -> bool {
    let Some(last) = history.last() else {
        return false;
    };
    if last.role != MessageRole::Assistant {
        return false;
    }
    let Some(tool_calls) = last.tool_calls.as_ref() else {
        return false;
    };
    if tool_calls.is_empty() {
        return false;
    }
    !tool_calls
        .iter()
        .any(|tc| pending_tool_call_ids.contains(&tc.id.0))
}

async fn assemble_state(
    config_path: std::path::PathBuf,
    sink: Arc<EventSink>,
    provider_factory: ProviderFactory,
    extra_tools: Vec<Box<dyn Tool>>,
    run_retention: i64,
    review_timeout: Duration,
) -> Result<(Arc<AppState>, McpConnectionGuard, Vec<String>)> {
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
        mcp_external,
        mcp_tool_names,
        config_path: active,
        ..
    } = ctx;

    apply_approval(&mut manager, &config, true, false);
    let approval_bridge = Arc::new(SessionApprovalBridge::new(sink.clone()));
    // 初始审批规则（tool-approval P1b，P0-4）：`rules_from_config` 单一
    // 推导 + **mobile 专属的 [approval] 段缺省 opt-in**（default=Auto
    // 自动判断 = 启发式预过滤 + AI 审查，设计 v3 用户拍板）；显式配置
    // （legacy 或新键）按映射保持原行为。规则写入 bridge 的共享 cell，
    // manager 经 with_rules 挂**同一 cell**——always_* / set_approval_
    // config 的热更直达引擎（游离副本 = 热更失效且无编译错误，P0-5）。
    let mut initial_rules = rules_from_config(&config);
    if config.approval.is_none() {
        initial_rules.default_level = ToolApprovalLevel::Auto;
    }
    approval_bridge.set_rules(initial_rules);
    // AI 审查装饰器（设计 v3，mobile-only）：AutoRisk 请求先过 LLM 审查
    // （safe 静默放行 / risky 弹人 / 失败 fail-safe 弹人）；无副作用工具
    // （ask_user / output_query）豁免保持最外层。SessionCore 此刻尚不
    // 存在（manager 要先进 CoreInner、装饰器又要快照 core 的
    // config/model_alias）——wire 后注，AskUserContext.wire 同款防环先例。
    let sink_dyn: Arc<dyn MsgSink> = sink.clone();
    let review_approval = Arc::new(
        AiReviewApproval::new(
            approval_bridge.clone(),
            sink_dyn,
            provider_factory.clone(),
            manager.tool_registry.clone(),
            tokio::runtime::Handle::current(),
        )
        .with_timeout(review_timeout),
    );
    manager.approval = ApprovalManager::with_rules(approval_bridge.rules_handle()).with_callback(
        Arc::new(SideEffectFreeApproval::new(review_approval.clone())),
    );

    // 初始会话模型：capabilities.main（用户在设置页改的主对话模型）
    // 优先；无该节/代号不可解析时回退 root agent 的别名（旧配置行为）。
    let root_alias = agent_tree.get_root().model_alias.clone();
    let main_alias = openslate_core::model_config::capability_alias(
        &config,
        openslate_core::model_config::CAP_MAIN,
    );
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
        // 附件启动 GC（设计 §3.1）：清 7 天前导入仍未发送的暂存行
        // （composer 选了图没提交），连同其 sha256 文件清扫。失败仅告警。
        match store.gc_stale_unbound_attachments(now_ms).await {
            Ok(n) if n > 0 => crate::alog!("attachments: GC 清扫 {n} 条过期暂存附件"),
            Ok(_) => {}
            Err(e) => crate::alog!("attachments: startup GC failed (non-fatal): {e}"),
        }
        // 长输出孤儿文件 GC（C3）：清 files/outputs 下无行引用的文件
        //（收敛「文件写成功、行未落」的崩溃窗口）。失败仅告警。
        match store.gc_orphan_output_files().await {
            Ok(n) if n > 0 => crate::alog!("outputs: GC 清扫 {n} 个孤儿输出文件"),
            Ok(_) => {}
            Err(e) => crate::alog!("outputs: startup GC failed (non-fatal): {e}"),
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
                // 保留悬挂变体加载（ask-user r5）：尾部带未答 tool_calls 的
                // assistant 消息不丢——pending 的 ask_user 问卷要靠它接受
                // 迟到答案补全；无可恢复问卷时下方再执行经典丢弃。
                match openslate_store_sqlite::recorder::RunRecorder::load_messages_keep_dangling(
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
                        restored_history = messages;
                        // recorder 失败（无法续跑/续写）= 无迟到答案恢复能力
                        // → 回落经典丢弃语义。
                        let mut keep_dangling = false;
                        if let Ok(recorder) = openslate_store_sqlite::recorder::RunRecorder::resume(
                            store.clone(),
                            openslate_core::types::RunId(run.id.clone()),
                            &root_agent_id,
                        )
                        .await
                        {
                            let recorder = Arc::new(recorder);
                            // ask_user 启动清扫：TTL 过期 → 合成 expired 补全
                            // （先落库再进内存，重启重放同序）；已解决行 →
                            // superseded 关卷；返回仍可恢复的悬挂 tool_call_id 集。
                            let recoverable = startup_question_maintenance(
                                &store,
                                &mut restored_history,
                                Some(&recorder),
                            )
                            .await;
                            // 可恢复的 ask_user 悬挂在历史中 → 保留尾部悬挂
                            // assistant（同批兄弟悬挂由下一 turn 前的
                            // superseded 修复收口）。
                            keep_dangling = !recoverable.is_empty();
                            restored_run = Some(openslate_session::state::SessionRun {
                                run_id: openslate_core::types::RunId(run.id.clone()),
                                recorder,
                            });
                        }
                        // 无可恢复问卷且尾部仍是悬挂 assistant → 执行
                        // load_messages 的经典丢弃（模型重新决策）；但以
                        // store 为权威守卫（may_pop_dangling_tail）：尾部
                        // 声明的 tool_call 仍有 pending 行引用时保留悬挂
                        // （迟到答案 / TTL 过期合成收尾的补全点，pop 不可
                        // 逆）。查询失败保守保留——多留的悬挂由下一 turn
                        // 前的修复收口，安全；误 pop 则无法挽回。
                        let mut popped_dangling = false;
                        if !keep_dangling {
                            match store.list_pending_questions().await {
                                Ok(pending) => {
                                    let pending_ids: Vec<String> = pending
                                        .into_iter()
                                        .map(|row| row.tool_call_id)
                                        .collect();
                                    if may_pop_dangling_tail(&restored_history, &pending_ids) {
                                        restored_history.pop();
                                        popped_dangling = true;
                                        // C2 pop 回写 DB（与 open_session
                                        // 同款）：删被 pop 的尾消息行 + 全删
                                        // 该 run 的 transcript_entries 行，
                                        // 下方重建走消息投影。此刻
                                        // SyncState 尚未构造（create_with
                                        // 尾部才装配），游标表为空 → 无需
                                        // 失效，sync_loop 首周期自会从 DB
                                        // 行数（=0）自举后全量写回。注意
                                        // recorder 已在上方建好（last_seq
                                        // 偏大）：后续写入留下 seq 空洞，
                                        // 无害（单调唯一即可）。
                                        match store.max_message_seq(&run.id).await {
                                            Ok(max_seq) => {
                                                if let Err(e) = store
                                                    .delete_messages_from_seq(&run.id, max_seq)
                                                    .await
                                                {
                                                    crate::alog!(
                                                        "restore: 删除悬挂尾消息失败（非致命）: {e}"
                                                    );
                                                }
                                            }
                                            Err(e) => crate::alog!(
                                                "restore: 查询 max seq 失败（非致命）: {e}"
                                            ),
                                        }
                                        if let Err(e) = store
                                            .delete_transcript_entries_for_run(&run.id)
                                            .await
                                        {
                                            crate::alog!(
                                                "restore: 清空转写条目行失败（非致命）: {e}"
                                            );
                                        }
                                        tracing::warn!(
                                            target: "openslate_store",
                                            "run {}: dropped dangling tail on restart (已回写 DB 删除)",
                                            run.id
                                        );
                                    }
                                }
                                Err(e) => crate::alog!(
                                    "restore: pending question guard failed, keep dangling (non-fatal): {e}"
                                ),
                            }
                        }
                        // 附件 resume 回填（设计 §6.1）：历史图随每轮重发，
                        // 总字节封顶 max_inline_total_mb（超限 warn 跳过——
                        // 模型丢图不丢文本）。
                        crate::attachments::refill_message_attachment_data(
                            &store,
                            &config.attachments,
                            &mut restored_history,
                        )
                        .await;
                        // 转写两级重建（设计 §4）：优先 transcript_entries
                        // 行；空（v5 前的 run）→ 消息投影回退。pop 过且
                        // 转写非空 → 追加 Meta 显性化（设计 §5）。
                        restored_transcript =
                            crate::transcripts::rebuild_transcript(&store, &run.id, &restored_history)
                                .await;
                        if popped_dangling && !restored_transcript.is_empty() {
                            restored_transcript.push(EntryDto::Meta {
                                text: crate::transcripts::DANGLING_TAIL_META.to_owned(),
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
        last_network_retry: None,
    };

    let core = Arc::new(SessionCore::new(inner));
    // AI 审查装饰器接线（core 晚于 manager 存在，见上方装配注释）。
    review_approval.wire(&core);
    Ok((
        Arc::new(AppState {
            core,
            sink,
            approval: approval_bridge,
            auth_token: None,
            provider_factory,
            origin: "mobile",
            _mcp: mcp_connections,
        }),
        mcp_external,
        mcp_tool_names,
    ))
}

/// `Arc<dyn Tool>` 再包装：registry 只收 `impl Tool`，热重载从 scratch
/// 注册表迁出的新工具 / 撞名回滚时装回的旧工具都是 `Arc`，包一层委派
/// 实现才能重新注册（工具本体与行为不变，仅多一层引用）。
struct SharedTool(Arc<dyn Tool>);

#[async_trait::async_trait]
impl Tool for SharedTool {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn namespace(&self) -> Option<String> {
        self.0.namespace()
    }

    fn description(&self) -> &str {
        self.0.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.0.parameters_schema()
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        self.0.execute(args).await
    }
}

/// 在 agents 目录中定位 id 匹配的 agent markdown：frontmatter `id:`
/// 优先（id 与文件名可不同），无 frontmatter id 时文件名派生 id 兜底
/// （`<id>.md` 存在即可）。两者都无 → Err。
fn find_agent_markdown(agents_dir: &std::path::Path, id: &str) -> Result<std::path::PathBuf> {
    let entries = std::fs::read_dir(agents_dir)
        .with_context(|| format!("读取 agents 目录失败：{}", agents_dir.display()))?;
    let mut filename_fallback: Option<std::path::PathBuf> = None;
    for entry in entries {
        let entry = entry.context("读取 agents 目录条目失败")?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        if filename == format!("{id}.md") {
            filename_fallback = Some(path.clone());
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(cfg) = openslate_core::config::parse_agent_markdown(&content, &filename) {
            if cfg.id.0 == id {
                return Ok(path);
            }
        }
    }
    filename_fallback.with_context(|| format!("agents 目录下无 id 为 {id} 的 agent md"))
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

/// `[attachments]` 键值对的 toml_edit 原地改写（`set_experimental_video`
/// / `set_image_processing` 的落盘半边共用）。读 → 改值 → 写回：其余
/// 键/注释/排版零扰动（toml_edit 往返保真）；无 `[attachments]` 节则
/// 追加（文档已有内容时前置空行分隔）。`[attachments]` 存在但不是标准
/// 表节点 → 拒绝改写并报错（与 core persist 层同一套守卫语义）。
fn persist_attachments_keys(
    path: &std::path::Path,
    pairs: Vec<(&'static str, toml_edit::Item)>,
) -> Result<()> {
    use toml_edit::{Item, Table};

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("读取配置失败：{}", path.display()))?;
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .with_context(|| format!("解析 {} 失败", path.display()))?;
    if let Some(item) = doc.as_table().get("attachments") {
        anyhow::ensure!(
            item.is_table(),
            "config section [attachments] is not a standard TOML table; refusing to rewrite it"
        );
    }
    let has_content = !doc.to_string().is_empty();
    let root = doc.as_table_mut();
    if root.get("attachments").is_none() {
        let mut fresh = Table::new();
        if has_content {
            fresh.decor_mut().set_prefix("\n");
        }
        root.insert("attachments", Item::Table(fresh));
    }
    let table = root
        .get_mut("attachments")
        .and_then(|item| item.as_table_mut())
        .expect("[attachments] was just ensured to be a table");
    // insert 覆盖旧值：反复设置不产生重复键。
    for (key, value) in pairs {
        table.insert(key, value);
    }
    std::fs::write(path, doc.to_string())
        .with_context(|| format!("写入 {} 失败", path.display()))?;
    Ok(())
}

/// `[attachments] experimental_video` 落盘（语义与守卫见
/// [`MobileRuntime::set_experimental_video`]）。
fn persist_experimental_video(path: &std::path::Path, enabled: bool) -> Result<()> {
    persist_attachments_keys(path, vec![("experimental_video", toml_edit::value(enabled))])
}

/// `[attachments] max_image_dim` / `max_image_mb` 落盘（图片处理双旋钮，
/// [`MobileRuntime::set_image_processing`] 的落盘半边；toml_edit 零扰动
/// 同 [`persist_experimental_video`]）。
fn persist_image_processing(
    path: &std::path::Path,
    max_dim: u32,
    max_mb: u64,
) -> Result<()> {
    persist_attachments_keys(
        path,
        vec![
            ("max_image_dim", toml_edit::value(i64::from(max_dim))),
            ("max_image_mb", toml_edit::value(i64::try_from(max_mb).unwrap_or(i64::MAX))),
        ],
    )
}

/// 恢复的 history → transcript 镜像投影（薄委托 → protocol 共享投影
/// `project_from_messages`，存储 v5 §4 / L6；原实现已搬迁至 protocol，
/// 双份逻辑防漂移）。仅供 `transcripts::rebuild_transcript` 的回退路径
/// 调用——新代码请直接用 protocol 函数。
pub(crate) fn messages_to_transcript(messages: &[Message]) -> Vec<EntryDto> {
    openslate_protocol::project_from_messages(messages)
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
        async fn generate(
            &self,
            _request: GenerateRequest,
        ) -> Result<ModelResponse, ProviderError> {
            Err(ProviderError::ServerError(500))
        }

        async fn generate_stream(
            &self,
            _request: GenerateRequest,
        ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
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
        Arc::new(move |_config, _alias| Ok(Box::new(provider.clone()) as Box<dyn ModelProvider>))
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
                image_tokens: None,
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

        runtime.send(r#"{"type":"submit","text":"你好"}"#).unwrap();
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

    /// P0-5 防回归：装配后 bridge 的 rules cell 与引擎 manager 的
    /// approval 共享同一 cell——直接写 bridge 规则，引擎读到的必须同步
    /// 变化（游离副本会让 always_* / set_approval_config 热更永远到不了
    /// 引擎，且无编译错误）。顺带锚定 mobile 的 [approval] 段缺省
    /// opt-in Auto（P0-4：装配层显式 opt-in，core 段缺省 = Allow）。
    #[test]
    fn approval_rules_cell_shared_between_bridge_and_engine() {
        let (paths, _dir) = temp_paths();
        let (collector, _rx) = Collector::new();
        let runtime =
            MobileRuntime::create_with(paths, collector, scripted_opts(vec![])).unwrap();

        // 装配初始值：无 [approval] 段 → rules_from_config 基线 + Auto。
        assert_eq!(
            runtime.state.approval.rules_snapshot().default_level,
            openslate_core::approval::ToolApprovalLevel::Auto,
            "mobile [approval] 缺省必须 opt-in Auto（P0-4）"
        );

        // 经 bridge cell 写入 → 引擎 manager 读到同一份（共享证明）。
        let engine_handle = {
            let inner = runtime.state.core.lock();
            let manager = inner.manager.as_ref().expect("manager at home");
            manager.approval.rules_handle()
        };
        assert!(
            Arc::ptr_eq(&runtime.state.approval.rules_handle(), &engine_handle),
            "bridge 与引擎必须共享同一 rules cell（P0-5）"
        );
        let mut rules = runtime.state.approval.rules_snapshot();
        rules.default_level = openslate_core::approval::ToolApprovalLevel::Manual;
        runtime.state.approval.set_rules(rules);
        let engine_rules = {
            let inner = runtime.state.core.lock();
            inner
                .manager
                .as_ref()
                .expect("manager at home")
                .approval
                .rules()
        };
        assert_eq!(
            engine_rules.default_level,
            openslate_core::approval::ToolApprovalLevel::Manual,
            "bridge 写入对引擎热生效（游离副本 = 热更失效）"
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

    // ── 7+. termux bash 降级阶梯（MCP 接管 × HostTool 兜底）──────────
    //
    // 挡板与 app wiring 集成测试同款：手写最小 MCP Streamable HTTP
    // server（std::net + 每连接线程），只实现 legacy `initialize` +
    // `tools/list`，通知回 202。不引新依赖。

    /// 起 mock MCP host（后台线程常驻，随测试进程退出回收），返回 /mcp
    /// URL。`tools` 为 tools/list 原样返回的裸名清单。
    fn spawn_mock_mcp_host(tools: &[&str]) -> String {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::{TcpListener, TcpStream};

        fn serve_connection(stream: TcpStream, tools: Vec<(String, String)>) {
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut stream = stream;
            loop {
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                    return;
                }
                let method = request_line
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_uppercase();
                let mut content_length = 0usize;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 {
                        return;
                    }
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = header.split_once(':') {
                        if k.eq_ignore_ascii_case("content-length") {
                            content_length = v.trim().parse().unwrap_or(0);
                        }
                    }
                }
                let mut body = vec![0u8; content_length];
                if content_length > 0 && reader.read_exact(&mut body).is_err() {
                    return;
                }
                if method != "POST" {
                    let _ = stream
                        .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n");
                    let _ = stream.flush();
                    continue;
                }
                let msg: serde_json::Value =
                    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                let rpc_method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let id = msg.get("id").cloned();
                let is_request = id.is_some();
                let response: Option<serde_json::Value> = match (rpc_method, is_request) {
                    ("initialize", true) => Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-11-25"),
                            "capabilities": { "tools": { "listChanged": false } },
                            "serverInfo": { "name": "mobile-mock-host", "version": "0.0.0" }
                        }
                    })),
                    ("tools/list", true) => Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": tools.iter().map(|(name, desc)| serde_json::json!({
                                "name": name,
                                "description": desc,
                                "inputSchema": { "type": "object", "properties": {}, "required": [] }
                            })).collect::<Vec<_>>()
                        }
                    })),
                    ("ping", true) => {
                        Some(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}))
                    }
                    _ => None,
                };
                match response {
                    Some(payload) => {
                        let bytes = payload.to_string().into_bytes();
                        let _ = write!(stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                            bytes.len());
                        let _ = stream.write_all(&bytes);
                    }
                    None if !is_request => {
                        let _ =
                            stream.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n");
                    }
                    None => {
                        let bytes = serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": { "code": -32601, "message": "method not found" }
                        })
                        .to_string()
                        .into_bytes();
                        let _ = write!(stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                            bytes.len());
                        let _ = stream.write_all(&bytes);
                    }
                }
                let _ = stream.flush();
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let addr = listener.local_addr().expect("local addr");
        let tools: Vec<(String, String)> = tools
            .iter()
            .map(|n| ((*n).to_string(), "mock tool".to_string()))
            .collect();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let tools = tools.clone();
                std::thread::spawn(move || serve_connection(stream, tools));
            }
        });
        format!("http://{addr}/mcp")
    }

    /// 在生效配置尾部追加 `[mcp]` 节（默认 toml 无该表，追加合法；
    /// `[mcp]` 必须在 `[mcp.servers.*]` 之前显式出现一次）。
    /// `builtin_template`：Some → 写 `builtin_tool_name_template`。
    fn append_mcp_config(paths: &MobilePaths, url: &str, builtin_template: Option<&str>) {
        let config = paths.config_file();
        let mut body = std::fs::read_to_string(&config).unwrap();
        body.push_str("\n[mcp]\n");
        if let Some(t) = builtin_template {
            body.push_str(&format!("builtin_tool_name_template = \"{t}\"\n"));
        }
        body.push_str(&format!(
            "[mcp.servers.termux]\ntransport = \"http\"\nurl = \"{url}\"\nenabled = true\n"
        ));
        std::fs::write(&config, body).unwrap();
    }

    fn envelope(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("envelope json")
    }

    /// 选择点 3：set_exec_backends 热切换不误杀 MCP 已注册的 bash 工具。
    #[test]
    fn set_exec_backends_does_not_kill_mcp_bash_tool() {
        let (paths, _dir) = temp_paths();
        paths.ensure_layout().unwrap();
        let url = spawn_mock_mcp_host(&["bash"]);
        append_mcp_config(&paths, &url, None);
        let (collector, _rx) = Collector::new();
        let runtime = MobileRuntime::create_with(paths, collector, scripted_opts(vec![])).unwrap();

        // 装配期接管：默认模板 + 键 termux → termux_bash 已是 MCP 工具。
        let roster = runtime.mcp_tool_names.lock().unwrap().clone();
        assert!(roster.iter().any(|n| n == "termux_bash"), "{roster:?}");
        let mcp_tool = runtime.registry.get("termux_bash").expect("MCP bash 工具");

        // 多次热切换（含双选 → 仅 termux → 仅 native）：MCP 的
        // termux_bash 必须原 Arc 存活（不被 unregister / HostTool 顶替）。
        runtime.set_exec_backends("native,termux");
        runtime.set_exec_backends("termux");
        runtime.set_exec_backends("native");
        let after = runtime.registry.get("termux_bash").expect("MCP 工具仍在场");
        assert!(
            Arc::ptr_eq(&mcp_tool, &after),
            "MCP 的 termux_bash 必须原 Arc 存活"
        );
        // 仅 native 切换后 native bash 正常注册。
        assert!(runtime.registry.contains("bash"));
        runtime.shutdown();
    }

    /// 选择点 2：HostTool 兜底在场时 reload 升级接管成功（让位，不撞
    /// 名回滚）。
    #[test]
    fn reload_mcp_takes_over_from_host_tool_without_rollback() {
        let (paths, _dir) = temp_paths();
        // 起始无 MCP：HostTool 兜底占位 termux_bash（termux 固定名）。
        let (collector, _rx) = Collector::new();
        let runtime =
            MobileRuntime::create_with(paths.clone(), collector, scripted_opts(vec![])).unwrap();
        runtime.set_exec_backends("native,termux");
        let host_tool = runtime.registry.get("termux_bash").expect("HostTool 兜底");
        assert!(runtime.mcp_tool_names.lock().unwrap().is_empty());

        // MCP host 上线（裸名 bash → 展开名 termux_bash）：reload 让位
        // HostTool 后接管成功，而非撞名整体回滚（旧实现的死锁）。
        let url = spawn_mock_mcp_host(&["bash"]);
        append_mcp_config(&paths, &url, None);
        let env = envelope(&runtime.reload_mcp());
        assert!(env["ok"].as_bool().unwrap(), "reload 应成功: {env}");
        assert_eq!(env["tools"].as_i64(), Some(1), "{env}");

        // 接管完成：termux_bash 换成 MCP 工具（名册内 + 非 HostTool Arc）。
        let roster = runtime.mcp_tool_names.lock().unwrap().clone();
        assert!(roster.iter().any(|n| n == "termux_bash"), "{roster:?}");
        let now = runtime.registry.get("termux_bash").expect("接管后在场");
        assert!(!Arc::ptr_eq(&host_tool, &now), "应已换成 MCP 工具");
        // native bash 不受牵连。
        assert!(runtime.registry.contains("bash"));
        runtime.shutdown();
    }

    /// reload 全败（全部 server 连不上且旧态为空）：不砸 HostTool 兜底。
    #[test]
    fn reload_mcp_all_failed_keeps_host_tool_fallback() {
        let (paths, _dir) = temp_paths();
        let (collector, _rx) = Collector::new();
        let runtime =
            MobileRuntime::create_with(paths.clone(), collector, scripted_opts(vec![])).unwrap();
        runtime.set_exec_backends("termux");
        assert!(
            runtime.registry.contains("termux_bash"),
            "HostTool 兜底（termux 固定名）"
        );

        // 死端口：bind 取端口后立刻 drop，连接必然被拒。
        let dead_url = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            drop(l);
            format!("http://127.0.0.1:{port}/mcp")
        };
        append_mcp_config(&paths, &dead_url, None);
        let env = envelope(&runtime.reload_mcp());
        // 全败 + 旧态空 → 空交换（ok:true / tools:0 / failed 非空），
        // 不走「冲突回滚」错误路径。
        assert!(env["ok"].as_bool().unwrap(), "{env}");
        assert_eq!(env["tools"].as_i64(), Some(0), "{env}");
        assert!(!env["failed"].as_array().unwrap().is_empty(), "{env}");
        // 兜底仍在场（重算后 HostTool 回装/保持）。
        assert!(
            runtime.registry.contains("termux_bash"),
            "全败后 HostTool 兜底仍在"
        );
        runtime.shutdown();
    }

    /// reload 撞真冲突（非 bash 域的 builtin 工具）→ 整体回滚，让位的
    /// HostTool 原样装回（回滚路径兜底在场）。
    #[test]
    fn reload_mcp_conflict_rollback_restores_yielded_host_tool() {
        let (paths, _dir) = temp_paths();
        let (collector, _rx) = Collector::new();
        let runtime =
            MobileRuntime::create_with(paths.clone(), collector, scripted_opts(vec![])).unwrap();
        runtime.set_exec_backends("termux");
        let host_tool = runtime
            .registry
            .get("termux_bash")
            .expect("HostTool 兜底（termux 固定名）");
        let ping_before = runtime.registry.get("mobile.ping").expect("extra ping");

        // 模板 `{tool}`：裸名原样注册 → termux_bash（让位）+ mobile.ping
        // （与 extra_tools 演示工具真冲突 → 回滚）。
        let url = spawn_mock_mcp_host(&["termux_bash", "mobile.ping"]);
        append_mcp_config(&paths, &url, Some("{tool}"));
        let env = envelope(&runtime.reload_mcp());
        assert!(!env["ok"].as_bool().unwrap(), "应因真冲突失败: {env}");
        assert!(
            env["error"].as_str().unwrap().contains("撞名"),
            "错误文案: {env}"
        );

        // （host_tool 变量保用于断言兜底语义：装回的 termux_bash 必须仍是
        // HostTool 而非 mock 工具，见下方 description 断言。）
        let _ = host_tool;
        // 回滚后：让位的 HostTool 装回（SharedTool 包同一 Arc，外层 Arc
        // 必然新——以描述语义判定是 HostTool 而非 mock），ping 未被顶替，
        // MCP 名册仍为空。
        let restored = runtime
            .registry
            .get("termux_bash")
            .expect("回滚后兜底装回");
        assert!(
            restored.description().contains("Termux environment"),
            "装回的应是 HostTool 兜底: {}",
            restored.description()
        );
        let ping_after = runtime.registry.get("mobile.ping").unwrap();
        assert!(Arc::ptr_eq(&ping_before, &ping_after), "ping 不被顶替");
        assert!(runtime.mcp_tool_names.lock().unwrap().is_empty());
        runtime.shutdown();
    }

    /// termux-only 命名矩阵：仅选 termux 后端 → HostTool 固定注册
    /// `termux_bash`（`bash` 不注册——名字与后端一一绑定，native 未选
    /// 不由 termux 顶替）；MCP 注册名 = builtin 模板展开（默认
    /// termux_bash；自定义 `{prefix}.{tool}` → termux.bash，证明不硬
    /// 编码）。
    #[test]
    fn termux_only_naming_matrix_follows_builtin_template() {
        // (a) 默认模板。
        let (paths, _dir) = temp_paths();
        paths.ensure_layout().unwrap();
        let url = spawn_mock_mcp_host(&["bash"]);
        append_mcp_config(&paths, &url, None);
        let (collector, _rx) = Collector::new();
        let runtime = MobileRuntime::create_with(paths, collector, scripted_opts(vec![])).unwrap();
        runtime.set_exec_backends("termux");
        let names = runtime.registry.tool_names();
        assert!(!names.contains(&"bash".to_owned()), "native 未选不注册: {names:?}");
        assert!(names.contains(&"termux_bash".to_owned()), "{names:?}");
        runtime.shutdown();

        // (b) 自定义模板 {prefix}.{tool}（展开 termux.bash；比照任务
        // 建议的字面 `{prefix}.bash`——后者缺 {tool} 占位符过不了 core
        // 校验，取等价合法形态）。
        let (paths, _dir) = temp_paths();
        paths.ensure_layout().unwrap();
        let url = spawn_mock_mcp_host(&["bash"]);
        append_mcp_config(&paths, &url, Some("{prefix}.{tool}"));
        let (collector, _rx) = Collector::new();
        let runtime = MobileRuntime::create_with(paths, collector, scripted_opts(vec![])).unwrap();
        runtime.set_exec_backends("termux");
        let names = runtime.registry.tool_names();
        assert!(!names.contains(&"bash".to_owned()), "native 未选不注册: {names:?}");
        assert!(
            names.contains(&"termux.bash".to_owned()),
            "MCP 注册名 = 模板展开: {names:?}"
        );
        assert!(
            names.contains(&"termux_bash".to_owned()),
            "HostTool 固定名不受模板影响: {names:?}"
        );
        runtime.shutdown();
    }

    /// C2：builtin 模板 setter（校验 → 落盘 → 空串删键回默认）。
    #[test]
    fn builtin_template_setter_validates_and_persists() {
        let (paths, _dir) = temp_paths();
        let (collector, _rx) = Collector::new();
        let runtime = MobileRuntime::create_with(paths, collector, scripted_opts(vec![])).unwrap();

        assert_eq!(
            runtime.get_mcp_builtin_tool_name_template(),
            "{prefix}_{tool}"
        );
        // 非法值全被拒（拒 {server} / 禁 __ / 缺 {tool} / 非法字符）。
        for bad in ["{server}_{tool}", "a__b_{tool}", "{prefix}", "a b_{tool}"] {
            let env = envelope(&runtime.set_mcp_builtin_tool_name_template(bad.to_owned()));
            assert!(!env["ok"].as_bool().unwrap(), "{bad} 应被拒: {env}");
        }
        // 合法值落盘 + 回读。
        let env =
            envelope(&runtime.set_mcp_builtin_tool_name_template("{prefix}.{tool}".to_owned()));
        assert!(env["ok"].as_bool().unwrap(), "{env}");
        assert_eq!(
            runtime.get_mcp_builtin_tool_name_template(),
            "{prefix}.{tool}"
        );
        // 空串删键回默认。
        let env = envelope(&runtime.set_mcp_builtin_tool_name_template(String::new()));
        assert!(env["ok"].as_bool().unwrap(), "{env}");
        assert_eq!(
            runtime.get_mcp_builtin_tool_name_template(),
            "{prefix}_{tool}"
        );
        runtime.shutdown();
    }

    // ── 8+. ask_user 工具（即时路径 / 重启恢复 / 首答者赢）────────────

    fn ask_user_tool_call_response() -> Vec<ModelStreamEvent> {
        vec![ModelStreamEvent::Done(ModelResponse {
            content: None,
            tool_calls: vec![openslate_core::types::ToolCall {
                id: openslate_core::types::ToolCallId("call-ask-1".into()),
                name: "ask_user".into(),
                arguments: serde_json::json!({
                    "questions": [{
                        "id": "db",
                        "header": "数据库",
                        "question": "本地存储用 SQLite 还是 sled？",
                        "type": "choice",
                        "options": [
                            {"label": "SQLite", "description": "零配置", "recommended": true},
                            {"label": "sled", "description": "纯 Rust"}
                        ]
                    }]
                }),
            }],
            reasoning_content: None,
            usage: None,
            finish_reason: Some("tool_calls".into()),
        })]
    }

    /// 即时路径全链路：模型发 ask_user → 下发信封（Android 契约形状）→
    /// resolve_host_call → 工具结果回填（自然语言 + 结构化）→ 续跑到
    /// turn_ok → 问卷行 answered 落库。
    #[test]
    fn ask_user_immediate_path_roundtrip() {
        let (paths, _dir) = temp_paths();
        let (collector, rx) = Collector::new();
        let runtime = MobileRuntime::create_with(
            paths.clone(),
            collector.clone(),
            scripted_opts(vec![
                ask_user_tool_call_response(),
                text_response("好，就用 SQLite。"),
            ]),
        )
        .unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

        runtime
            .send(r#"{"type":"submit","text":"帮我选本地存储"}"#)
            .unwrap();
        let events = collector.wait_for(&rx, "host_call_requested", Duration::from_secs(20));
        let req = events
            .iter()
            .rev()
            .find(|e| e["type"] == "host_call_requested" && e["tool"] == "ask_user")
            .expect("ask_user host call 信封");

        // 下发契约逐字段核对。
        assert_eq!(req["args"]["v"], 1, "payload 版本字段 v=1");
        assert_eq!(req["args"]["timeout"], -1, "默认永不超时（[ask_user] Never）");
        let q = &req["args"]["questions"][0];
        assert_eq!(q["id"], "db");
        assert_eq!(q["header"], "数据库");
        assert_eq!(q["question"], "本地存储用 SQLite 还是 sled？");
        assert_eq!(q["type"], "choice");
        assert_eq!(q["options"][0]["label"], "SQLite");
        assert_eq!(q["options"][0]["description"], "零配置");
        assert_eq!(q["options"][0]["recommended"], true);
        assert_eq!(q["options"].as_array().unwrap().len(), 2);
        assert_eq!(q["multi_select"], false);
        assert_eq!(q["optional"], false);
        let id = req["id"].as_u64().unwrap();

        // Android resolve 契约形状（ok=true 业务负载）。
        assert!(runtime.resolve_host_call(
            id,
            true,
            r#"{"outcome":"answered","answers":[{"id":"db","selected":["SQLite"]}]}"#.into()
        ));
        collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));

        // 行状态 answered；工具结果与迟to路径同形地进了消息历史。
        let run_id = runtime.current_run_id().expect("run 已打开");
        let (srt, store) = scratch_store(&paths);
        let (row, msgs) = srt.block_on(async {
            let row = store
                .get_pending_question(&id.to_string())
                .await
                .unwrap()
                .expect("问卷行存在");
            let msgs = openslate_store_sqlite::recorder::RunRecorder::load_messages(&store, &run_id)
                .await
                .unwrap();
            (row, msgs)
        });
        assert_eq!(row.status, "answered");
        assert_eq!(row.tool_call_id, "call-ask-1", "tool_call_id 关联悬挂调用");
        assert!(
            store_row_pending_empty(&srt, &store),
            "answered 行不再出现在 pending 清单"
        );
        let tool_msg = msgs
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("工具结果已入历史");
        assert!(
            tool_msg.content.contains("User answered your questions"),
            "自然语言段: {}",
            tool_msg.content
        );
        assert!(
            tool_msg.content.contains("- \"db\" → SQLite (Recommended)"),
            "逐题行: {}",
            tool_msg.content
        );
        assert!(
            tool_msg.content.contains(r#""outcome":"answered""#),
            "结构化段: {}",
            tool_msg.content
        );
        runtime.shutdown();
    }

    fn store_row_pending_empty(
        srt: &tokio::runtime::Runtime,
        store: &openslate_store_sqlite::store::SqliteStore,
    ) -> bool {
        srt.block_on(async { store.list_pending_questions().await.unwrap() })
            .is_empty()
    }

    /// secret 子系统全链路：模型问 secret（env 形态）→ 用户提交（记住）
    /// → 凭据入 vault/库 → 后续 bash 命令拿到注入的 env 值 → 工具输出
    /// 对已知密钥值打码（明文不进历史）。native bash 经 set_exec_backends
    /// 注册（默认装配不注册任何 bash 名）。
    #[test]
    fn ask_user_secret_env_injection_and_masking_roundtrip() {
        let ask_secret = || {
            vec![ModelStreamEvent::Done(ModelResponse {
                content: None,
                tool_calls: vec![openslate_core::types::ToolCall {
                    id: openslate_core::types::ToolCallId("call-sec-1".into()),
                    name: "ask_user".into(),
                    arguments: serde_json::json!({
                        "questions": [{
                            "id": "tok",
                            "header": "令牌",
                            "question": "GitHub token？",
                            "type": "secret",
                            "storage": {"kind": "env", "name": "GITHUB_TOKEN"}
                        }]
                    }),
                }],
                reasoning_content: None,
                usage: None,
                finish_reason: Some("tool_calls".into()),
            })]
        };
        let bash_echo = || {
            vec![ModelStreamEvent::Done(ModelResponse {
                content: None,
                tool_calls: vec![openslate_core::types::ToolCall {
                    id: openslate_core::types::ToolCallId("call-bash-1".into()),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "printf 'token=%s' \"$GITHUB_TOKEN\""}),
                }],
                reasoning_content: None,
                usage: None,
                finish_reason: Some("tool_calls".into()),
            })]
        };

        let (paths, _dir) = temp_paths();
        let (collector, rx) = Collector::new();
        let runtime = MobileRuntime::create_with(
            paths.clone(),
            collector.clone(),
            scripted_opts(vec![
                ask_secret(),
                bash_echo(),
                text_response("令牌可用。"),
            ]),
        )
        .unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
        // bash（启发式 High）在 P1b 的 Auto 缺省下会进审查链弹人；本
        // 测试聚焦 secret 注入/脱敏，先把审批缺省放开为 allow。
        runtime
            .send(r#"{"type":"set_approval_config","default_level":"allow","tool_levels":{},"server_levels":{}}"#)
            .unwrap();
        collector.wait_for(&rx, "config_changed", Duration::from_secs(10));
        runtime.set_exec_backends("native");

        runtime
            .send(r#"{"type":"submit","text":"需要 GitHub token"}"#)
            .unwrap();
        let events = collector.wait_for(&rx, "host_call_requested", Duration::from_secs(20));
        let req = events
            .iter()
            .rev()
            .find(|e| e["type"] == "host_call_requested" && e["tool"] == "ask_user")
            .expect("ask_user 信封");
        // secret 题型下发契约：storage 透传。
        assert_eq!(req["args"]["questions"][0]["type"], "secret");
        assert_eq!(req["args"]["questions"][0]["storage"]["kind"], "env");
        assert_eq!(req["args"]["questions"][0]["storage"]["name"], "GITHUB_TOKEN");
        let id = req["id"].as_u64().unwrap();

        // 用户提交明文（remember=true）。
        assert!(runtime.resolve_host_call(
            id,
            true,
            r#"{"outcome":"answered","answers":[{"id":"tok","value":"sk-e2e-canary-value","remember":true}]}"#.into()
        ));
        collector.wait_for(&rx, "turn_ok", Duration::from_secs(30));

        let run_id = runtime.current_run_id().expect("run 已打开");
        let (srt, store) = scratch_store(&paths);
        let (creds, msgs) = srt.block_on(async {
            let creds = store.list_credentials().await.unwrap();
            let msgs =
                openslate_store_sqlite::recorder::RunRecorder::load_messages(&store, &run_id)
                    .await
                    .unwrap();
            (creds, msgs)
        });

        // 1) 持久凭据行（remember=true）。
        assert_eq!(creds.len(), 1, "{creds:?}");
        assert_eq!(creds[0].name, "GITHUB_TOKEN");
        assert_eq!(creds[0].kind, "env");

        // 2) ask_user 工具结果只含引用，不含明文（引用回填无替换 →
        //    也不追加脱敏提示）。
        let ask_msg = msgs
            .iter()
            .find(|m| m.role == MessageRole::Tool && m.tool_call_id.as_ref().unwrap().0 == "call-sec-1")
            .expect("ask_user 结果");
        assert!(ask_msg.content.contains("已存入凭据：环境变量 GITHUB_TOKEN"), "{}", ask_msg.content);
        assert!(!ask_msg.content.contains("sk-e2e-canary-value"), "{}", ask_msg.content);
        assert!(
            !ask_msg.content.contains("<system-note>"),
            "无替换的输出不追加提示（避免噪音）: {}",
            ask_msg.content
        );

        // 3) bash 输出：env 注入生效（子进程可见）+ 历史中已打码 + 实际
        //    发生替换 → 末尾追加内部提示（tool result 内容的一部分）。
        let bash_msg = msgs
            .iter()
            .find(|m| m.role == MessageRole::Tool && m.tool_call_id.as_ref().unwrap().0 == "call-bash-1")
            .expect("bash 结果");
        assert!(
            bash_msg.content.contains("token=***"),
            "输出脱敏应把已知密钥值替换为 ***: {}",
            bash_msg.content
        );
        assert!(!bash_msg.content.contains("sk-e2e-canary-value"), "{}", bash_msg.content);
        assert!(
            bash_msg.content.contains("<system-note>Output masked:"),
            "发生替换 → 追加内部提示: {}",
            bash_msg.content
        );
        assert!(
            bash_msg.content.ends_with("</system-note>"),
            "提示位于内容末尾: {}",
            bash_msg.content
        );

        runtime.shutdown();
    }

    /// `tool_output_by_call_id`（懒加载全量显示的 FFI 后端）：跑一个
    /// native bash 回合 → 内存 history 命中返回完整输出；未知 id / 空
    /// id 返回 None（调用方回退 preview）。
    #[test]
    fn tool_output_by_call_id_hit_and_miss() {
        let bash = || {
            vec![ModelStreamEvent::Done(ModelResponse {
                content: None,
                tool_calls: vec![openslate_core::types::ToolCall {
                    id: openslate_core::types::ToolCallId("call-lazy-1".into()),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "printf 'lazy-full-output'"}),
                }],
                reasoning_content: None,
                usage: None,
                finish_reason: Some("tool_calls".into()),
            })]
        };
        let (paths, _dir) = temp_paths();
        let (collector, rx) = Collector::new();
        let runtime = MobileRuntime::create_with(
            paths.clone(),
            collector.clone(),
            scripted_opts(vec![bash(), text_response("完成")]),
        )
        .unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
        // 同上：bash 直跑场景放开审批缺省（本测试聚焦懒加载 FFI 后端）。
        runtime
            .send(r#"{"type":"set_approval_config","default_level":"allow","tool_levels":{},"server_levels":{}}"#)
            .unwrap();
        collector.wait_for(&rx, "config_changed", Duration::from_secs(10));
        runtime.set_exec_backends("native");

        runtime
            .send(r#"{"type":"submit","text":"跑一下"}"#)
            .unwrap();
        collector.wait_for(&rx, "turn_ok", Duration::from_secs(30));

        // 命中：内存 history 直读完整输出（tool_end preview 只有 512
        // 字符截断版，此处应拿到全文格式化输出）。
        let out = runtime
            .tool_output_by_call_id("call-lazy-1".into())
            .expect("命中已知 call_id");
        assert!(out.contains("lazy-full-output"), "输出: {out}");
        // 未命中：未知 id / 空串 → None（UI 回退条目自带 preview）。
        assert!(runtime.tool_output_by_call_id("call-unknown".into()).is_none());
        assert!(runtime.tool_output_by_call_id(String::new()).is_none());
        runtime.shutdown();
    }

    // ── 长输出全文闭环（C6）──────────────────────────────────────────

    /// C6 测试脚手架：真装配 runtime（文件库）→ 直插 Tool 消息进内存
    /// history + tool_outputs 行/文件进库（经 runtime 自身的 tokio handle
    /// 驱动——`create_with` 内部也 block_on，不能再包一层外层 runtime）。
    /// `delete_file = true` 模拟文件缺失（行在、文件被清）。返回
    /// (runtime, sha_hex, 预览 content, TempDir)——**TempDir 必须由调用方
    /// 持有到测试结束**（提前 drop = 整个库目录被删，SQLite 行读靠 fd
    /// 仍活着、按路径的文件读全挂）。
    #[allow(clippy::type_complexity)]
    fn seed_stored_output_runtime(
        full_text: &str,
        delete_file: bool,
    ) -> (
        Arc<MobileRuntime>,
        String,
        String,
        tempfile::TempDir,
    ) {
        let (paths, dir) = temp_paths();
        let (collector, _rx) = Collector::new();
        let runtime = MobileRuntime::create_with(paths.clone(), collector, scripted_opts(vec![]))
            .expect("runtime");
        let store = runtime
            .state
            .core
            .lock()
            .store
            .clone()
            .expect("文件库 store 应已装配");
        let handle = {
            let guard = runtime.rt.lock().expect("mobile runtime lock poisoned");
            guard.as_ref().expect("runtime alive").handle().clone()
        };

        // sha + 行/文件（C1 API：zst 由调用方压好）。ref 用数字短 id
        // （会话内自增序号形态；该 run 首条 → out-1）。legacy 单帧文件
        // （frame_index 空 BLOB）——C6 全文闭环走整篇解压，与格式无关。
        use sha2::{Digest, Sha256};
        let sha_hex = format!("{:x}", Sha256::digest(full_text.as_bytes()));
        let ref_id = "out-1".to_owned();
        let data_zst = zstd::encode_all(full_text.as_bytes(), 3).expect("zst");
        handle.block_on(async {
            store
                .insert_run("run-c6", None, "root", "completed", "{}", 1000)
                .await
                .expect("run");
            store
                .insert_tool_output(
                    "out-c6",
                    "run-c6",
                    &ref_id,
                    &sha_hex,
                    "bash",
                    full_text.len() as i64,
                    full_text.lines().count() as i64,
                    &[],
                    &data_zst,
                    2000,
                )
                .await
                .expect("tool output row");
            // 会话身份：严格会话隔离下查找按 (run, ref) 精确匹配，
            // session_run 必须指向落盘行所在的 run-c6。
            let recorder = openslate_store_sqlite::recorder::RunRecorder::resume(
                store.clone(),
                openslate_core::types::RunId("run-c6".into()),
                "root",
            )
            .await
            .expect("recorder");
            let mut inner = runtime.state.core.lock();
            inner.session_run = Some(openslate_session::state::SessionRun {
                run_id: openslate_core::types::RunId("run-c6".into()),
                recorder: Arc::new(recorder),
            });
        });
        if delete_file {
            std::fs::remove_file(
                store
                    .files_dir()
                    .expect("dir")
                    .join("outputs")
                    .join(format!("{sha_hex}.zst")),
            )
            .expect("remove file");
        }

        // Tool 消息直插内存 history（core build_preview 冻结格式的头行 +
        // 省略行 + 尾巴——解析只依赖头行；head/tail 切点走字符边界，
        // 兼容多字节测试文本）
        let head = &full_text[..floor_char_boundary(full_text, 32)];
        let tail_start = ceil_char_boundary(full_text, full_text.len().saturating_sub(16));
        let tail = &full_text[tail_start..];
        let preview = format!(
            "[STORED OUTPUT ref=\"{}\" size={} lines={}]\n{}\n[... {} bytes omitted ({} lines) — use output_query to inspect ...]\n{}",
            ref_id,
            full_text.len(),
            full_text.lines().count(),
            head,
            full_text.len() - head.len() - tail.len(),
            full_text.lines().count(),
            tail,
        );
        {
            let mut inner = runtime.state.core.lock();
            inner.history.push(Message {
                attachments: None,
                role: MessageRole::Tool,
                content: preview.clone(),
                tool_call_id: Some(openslate_core::types::ToolCallId("call-c6-1".into())),
                name: Some("bash".into()),
                tool_calls: None,
                reasoning_content: None,
            });
        }
        (runtime, sha_hex, preview, dir)
    }

    /// 落盘标记 ref 解析（数字短 id / 16 位上界 / 非法形状）。
    #[test]
    fn parse_stored_output_ref_digit_forms() {
        let marker_of = |r: &str| format!("[STORED OUTPUT ref=\"{r}\" size=1 lines=1]\npreview");
        // 短 id（v7 会话内自增序号）
        assert_eq!(parse_stored_output_ref(&marker_of("out-3")), Some("out-3"));
        assert_eq!(parse_stored_output_ref(&marker_of("out-42")), Some("out-42"));
        // 16 位数字（格式上界）
        assert_eq!(
            parse_stored_output_ref(&marker_of("out-1234567890123456")),
            Some("out-1234567890123456")
        );
        // 标记不在行首（前面有任意文本）也能定位
        assert_eq!(
            parse_stored_output_ref(&format!("noise\n{}", marker_of("out-7"))),
            Some("out-7")
        );
        // 非法形状 → None（含旧 16hex 形态：字母不做兼容 → 回落预览）
        assert_eq!(parse_stored_output_ref(&marker_of("out-")), None, "空 ref");
        assert_eq!(
            parse_stored_output_ref(&marker_of(&format!("out-{}", "1".repeat(17)))),
            None,
            "超 16 位"
        );
        assert_eq!(parse_stored_output_ref(&marker_of("out-zzzz")), None, "非数字");
        assert_eq!(
            parse_stored_output_ref(&marker_of("out-ab12cd34ef56ab78")),
            None,
            "旧 16hex 形态不解析（回落预览）"
        );
        assert_eq!(parse_stored_output_ref("plain content"), None, "无标记");
    }

    /// C6：有标记 → 行+文件齐全时取回全文（== 落盘原文）。
    #[test]
    fn tool_output_by_call_id_expands_stored_output() {
        let full_text = format!("c6-head\n{}\nc6-tail", "m".repeat(20_000));
        let (runtime, _sha, _preview, _dir) = seed_stored_output_runtime(&full_text, false);

        let out = runtime
            .tool_output_by_call_id("call-c6-1".into())
            .expect("命中");
        assert_eq!(out, full_text, "应返回落盘全文而非预览");
        runtime.shutdown();
    }

    /// C6：无标记 / 未知 ref / 文件缺失 → 一律回落 content 原文（预览）。
    #[test]
    fn tool_output_by_call_id_stored_output_fallbacks() {
        let full_text = "z".repeat(9000);

        // ① 文件缺失（行在）：回落预览原文
        let (runtime, _sha, preview, _dir) = seed_stored_output_runtime(&full_text, true);
        let out = runtime
            .tool_output_by_call_id("call-c6-1".into())
            .expect("命中");
        assert_eq!(out, preview, "文件缺失 → content 原文（预览）");
        runtime.shutdown();

        // ② 无标记：普通截断输出原样返回
        let (runtime, _sha, _preview, _dir) = seed_stored_output_runtime(&full_text, false);
        {
            let mut inner = runtime.state.core.lock();
            inner.history.last_mut().expect("tool msg").content =
                "[TRUNCATED: original 9000 bytes, showing 16 bytes]\nzz…".into();
        }
        let out = runtime
            .tool_output_by_call_id("call-c6-1".into())
            .expect("命中");
        assert!(
            out.starts_with("[TRUNCATED:"),
            "无 STORED OUTPUT 标记 → 原文直通, got: {}",
            &out[..out.len().min(40)]
        );
        runtime.shutdown();

        // ③ 标记 ref 无行（伪造 ref）：回落预览原文
        let (runtime, _sha, _preview, _dir) = seed_stored_output_runtime(&full_text, false);
        let fake_preview = format!(
            "[STORED OUTPUT ref=\"out-0000000000000000\" size={} lines=1]\nzz…\n[... omitted — use output_query to inspect ...]",
            full_text.len()
        );
        {
            let mut inner = runtime.state.core.lock();
            inner.history.last_mut().expect("tool msg").content = fake_preview.clone();
        }
        let out = runtime
            .tool_output_by_call_id("call-c6-1".into())
            .expect("命中");
        assert_eq!(out, fake_preview, "未知 ref → content 原文（预览）");
        runtime.shutdown();
    }

    /// C6：超限全文封顶——头 448 KiB + 省略行 + 尾 64 KiB，多字节字符不
    /// 劈裂（「中」3B/字铺满，切点必然落在字符内部）。
    #[test]
    fn tool_output_by_call_id_caps_full_text_at_512k() {
        // 600 KiB 的「中」（1_800_000B，非 3 的倍数切点全部落字符内）
        let full_text = "中".repeat(600 * 1024 / 3);
        assert!(full_text.len() > STORED_FULL_TEXT_CAP);
        let (runtime, _sha, _preview, _dir) = seed_stored_output_runtime(&full_text, false);

        let out = runtime
            .tool_output_by_call_id("call-c6-1".into())
            .expect("命中");
        assert!(
            out.contains(STORED_FULL_TEXT_OMISSION),
            "超限必有省略行"
        );
        assert!(
            out.len() < full_text.len(),
            "封顶后必小于原文（{} < {}）",
            out.len(),
            full_text.len()
        );
        let mut lines = out.lines();
        let head = lines.next().expect("head 行");
        assert!(head.len() <= STORED_FULL_TEXT_HEAD, "头行不超 448 KiB 预算");
        assert!(head.chars().all(|c| c == '中'), "头部字符不被劈裂");
        assert_eq!(
            head.len(),
            floor_char_boundary(&full_text, STORED_FULL_TEXT_HEAD),
            "头部为 448 KiB 预算内最大完整字符段（floor 边界）"
        );
        let tail = out.lines().last().expect("tail 行");
        assert!(tail.len() <= STORED_FULL_TEXT_TAIL, "尾行不超 64 KiB 预算");
        assert!(tail.chars().all(|c| c == '中'), "尾部字符不被劈裂");
        assert_eq!(
            tail.len(),
            full_text.len()
                - ceil_char_boundary(&full_text, full_text.len() - STORED_FULL_TEXT_TAIL),
            "尾部为 64 KiB 预算内最大完整字符段（ceil 边界）"
        );
        runtime.shutdown();
    }

    /// 重启恢复全链路（预置崩溃现场）：悬挂 ask_user 的 run + pending
    /// 问卷行 → 快照 pending_questions 回放 → question_reply 迟到答案
    /// 补全 tool result 并续跑 → turn_ok；重复应答首答者赢。
    #[test]
    fn ask_user_restart_recovery_and_first_answer_wins() {
        let (paths, _dir) = temp_paths();
        paths.ensure_layout().unwrap();

        let run_id_str = "ask-recover-run";
        let tool_call_id = "call-ask-9";
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        {
            let (srt, store) = scratch_store(&paths);
            srt.block_on(async {
                store
                    .insert_run(run_id_str, Some("提问恢复"), "root", "interrupted", "{}", now_ms - 60_000)
                    .await
                    .unwrap();
                store
                    .insert_execution_node(
                        &format!("{run_id_str}-persist"),
                        run_id_str,
                        "root",
                        None,
                        None,
                        "running",
                        "{}",
                        now_ms - 60_000,
                    )
                    .await
                    .unwrap();
                let user_msg = Message {
                    attachments: None,
                    role: MessageRole::User,
                    content: "选个数据库".into(),
                    tool_call_id: None,
                    name: None,
                    tool_calls: None,
                    reasoning_content: None,
                };
                let assistant_msg = Message {
                    attachments: None,
                    role: MessageRole::Assistant,
                    content: String::new(),
                    tool_call_id: None,
                    name: None,
                    tool_calls: Some(vec![openslate_core::types::ToolCall {
                        id: openslate_core::types::ToolCallId(tool_call_id.into()),
                        name: "ask_user".into(),
                        arguments: serde_json::json!({
                            "questions": [{
                                "id": "db",
                                "question": "数据库？",
                                "type": "choice",
                                "options": [
                                    {"label": "SQLite", "recommended": true},
                                    {"label": "sled"}
                                ]
                            }]
                        }),
                    }]),
                    reasoning_content: None,
                };
                store
                    .insert_message(
                        &format!("msg-{run_id_str}-1"),
                        run_id_str,
                        &format!("{run_id_str}-persist"),
                        Some("root"),
                        "user",
                        &openslate_store_sqlite::codec::encode_content(
                            &serde_json::to_string(&user_msg).unwrap(),
                        ),
                        None,
                        1,
                        now_ms - 59_000,
                    )
                    .await
                    .unwrap();
                store
                    .insert_message(
                        &format!("msg-{run_id_str}-2"),
                        run_id_str,
                        &format!("{run_id_str}-persist"),
                        Some("root"),
                        "assistant",
                        &openslate_store_sqlite::codec::encode_content(
                            &serde_json::to_string(&assistant_msg).unwrap(),
                        ),
                        None,
                        2,
                        now_ms - 58_000,
                    )
                    .await
                    .unwrap();
                store
                    .insert_pending_question(
                        "42",
                        tool_call_id,
                        r#"[{"id":"db","question":"数据库？","type":"choice","options":[{"label":"SQLite","recommended":true},{"label":"sled"}],"optional":false}]"#,
                        1,
                        now_ms - 58_000,
                    )
                    .await
                    .unwrap();
            });
        }

        let (collector, rx) = Collector::new();
        let runtime = MobileRuntime::create_with(
            paths.clone(),
            collector.clone(),
            scripted_opts(vec![text_response("收到，就用 SQLite。")]),
        )
        .unwrap();

        // 快照回放：pending_questions 从问卷表填充（重启恢复数据源）。
        let events = collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
        let snap = events
            .iter()
            .rev()
            .find(|e| e["type"] == "snapshot")
            .expect("首份快照");
        let pq = snap["session"]["pending_questions"]
            .as_array()
            .expect("pending_questions 数组");
        assert_eq!(pq.len(), 1, "悬挂问卷回放: {snap}");
        assert_eq!(pq[0]["request_id"], "42");
        assert_eq!(pq[0]["tool_call_id"], tool_call_id);
        assert_eq!(pq[0]["status"], "pending");
        assert_eq!(pq[0]["version"], 1);
        assert_eq!(pq[0]["questions"][0]["id"], "db", "问卷负载透传");

        // 迟到答案：question_reply → 补全原 tool result → 续跑 → turn_ok。
        runtime
            .send(r#"{"type":"question_reply","request_id":"42","action":"accept","answers":[{"id":"db","selected":["SQLite"]}]}"#)
            .unwrap();
        collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));

        // 首答者赢：重复应答 → 定向 notice，不二次续跑。
        runtime
            .send(r#"{"type":"question_reply","request_id":"42","action":"accept","answers":[{"id":"db","selected":["sled"]}]}"#)
            .unwrap();
        let events = collector.wait_for(&rx, "notice", Duration::from_secs(10));
        let notice = events
            .iter()
            .rev()
            .find(|e| e["type"] == "notice")
            .expect("notice 已发出");
        assert!(
            notice["text"].as_str().unwrap().contains("已被应答或已关闭"),
            "首答者赢语义: {notice}"
        );

        // 行 answered；消息历史里迟到补全的 tool result 与即时路径同形。
        let (srt, store) = scratch_store(&paths);
        let (row, msgs) = srt.block_on(async {
            let row = store
                .get_pending_question("42")
                .await
                .unwrap()
                .expect("问卷行存在");
            let msgs = openslate_store_sqlite::recorder::RunRecorder::load_messages(
                &store,
                run_id_str,
            )
            .await
            .unwrap();
            (row, msgs)
        });
        assert_eq!(row.status, "answered");
        let tool_msg = msgs
            .iter()
            .find(|m| m.role == MessageRole::Tool
                && m.tool_call_id
                    .as_ref()
                    .map(|i| i.0.as_str())
                    == Some(tool_call_id))
            .expect("迟到答案已补全 tool result");
        assert!(
            tool_msg.content.contains("User answered your questions")
                && tool_msg.content.contains("- \"db\" → SQLite (Recommended)"),
            "与即时路径完全同形（迟到等价不变量）: {}",
            tool_msg.content
        );
        assert!(
            msgs.iter().any(|m| m.role == MessageRole::Assistant
                && m.content.contains("收到，就用 SQLite。")),
            "续跑后的最终回复已入库"
        );
        runtime.shutdown();
    }

    // ── 附件（多模态输入，存储 v5 §3/§6/§7）───────────────────────────

    /// 捕获型 scripted provider：记录每次请求的 messages 快照（附件
    /// data 已由 AttachmentDataProxy 装载）+ 流式回脚本回复。
    struct CapturingProvider {
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
        scripts: Arc<Mutex<VecDeque<Vec<ModelStreamEvent>>>>,
    }

    impl Clone for CapturingProvider {
        fn clone(&self) -> Self {
            Self {
                requests: self.requests.clone(),
                scripts: self.scripts.clone(),
            }
        }
    }

    impl CapturingProvider {
        fn new(scripts: Vec<Vec<ModelStreamEvent>>) -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                scripts: Arc::new(Mutex::new(scripts.into())),
            }
        }
    }

    #[async_trait]
    impl openslate_core::provider::ModelProvider for CapturingProvider {
        async fn generate(
            &self,
            _request: GenerateRequest,
        ) -> Result<ModelResponse, ProviderError> {
            Err(ProviderError::ServerError(500))
        }

        async fn generate_stream(
            &self,
            request: GenerateRequest,
        ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
            self.requests.lock().unwrap().push(request.messages);
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
            tokio::spawn(async move {
                for event in script {
                    let _ = tx.send(Ok(event)).await;
                }
            });
            rx
        }

        fn provider_name(&self) -> &str {
            "capturing"
        }
    }

    /// 写一张小 PNG 到临时文件，返回路径与字节。
    fn write_tiny_png(dir: &std::path::Path) -> (std::path::PathBuf, Vec<u8>) {
        let mut png = Vec::new();
        let img = image::RgbImage::from_pixel(48, 32, image::Rgb([10, 200, 30]));
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let path = dir.join("shot.png");
        std::fs::write(&path, &png).unwrap();
        (path, png)
    }

    /// 伪 MP4（ftyp/isom 头 + 填充）。
    fn write_fake_mp4(dir: &std::path::Path) -> std::path::PathBuf {
        let mut mp4 = vec![0, 0, 0, 0];
        mp4.extend_from_slice(b"ftypisom");
        mp4.extend(std::iter::repeat(7u8).take(4096));
        let path = dir.join("clip.mp4");
        std::fs::write(&path, &mp4).unwrap();
        path
    }

    /// 预置本地 models_dev registry 数据（门控 caps 数据源）。
    fn seed_registry_models_dev(paths: &MobilePaths, json: serde_json::Value) {
        let dir = paths.config_dir.join("registry");
        std::fs::create_dir_all(&dir).unwrap();
        let raw = serde_json::to_vec(&json).unwrap();
        let zst = zstd::encode_all(raw.as_slice(), 3).unwrap();
        std::fs::write(dir.join("models_dev.json.zst"), zst).unwrap();
    }

    /// models.dev 条目样例（默认会话模型 glm-5.3-flash）。
    fn models_dev_entry(input: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "zhipuai/glm-5.3-flash": {
                "name": "GLM-5.3-Flash",
                "limit": {"context": 200000, "output": 8192},
                "modalities": {"input": input, "output": ["text"]},
                "reasoning": true,
                "tool_call": true
            }
        })
    }

    /// e2e：import（FFI JSON）→ Submit 带附件 → 捕获 provider 收到
    /// data=Some 的附件（代理装载）→ 落库绑定 + 消息持久化带引用 →
    /// transcript User 条目带引用（快照 + 持久化行）。
    #[test]
    fn attachments_submit_e2e_delivers_data_parts() {
        let (paths, _dir) = temp_paths();
        let provider = CapturingProvider::new(vec![text_response("看到了，图上是绿色方块。")]);
        let probe = provider.clone();
        let opts = RuntimeOptions {
            provider_factory: Some(Arc::new(move |_c, _a| {
                Ok(Box::new(provider.clone()) as Box<dyn openslate_core::provider::ModelProvider>)
            })),
            ..Default::default()
        };
        let (collector, rx) = Collector::new();
        let runtime = MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

        // 1) import（同 FFI 后端）：小 PNG → passthrough png。
        let (png_path, png_bytes) = write_tiny_png(_dir.path());
        let env: serde_json::Value = serde_json::from_str(
            &runtime.import_attachment_json(png_path.to_str().unwrap(), "image/png", "shot.png"),
        )
        .unwrap();
        assert!(env["ok"].as_bool().unwrap(), "{env}");
        assert_eq!(env["mime"].as_str().unwrap(), "image/png");
        assert_eq!(env["filename"].as_str().unwrap(), "shot.png");
        assert_eq!(env["size_bytes"].as_u64().unwrap(), png_bytes.len() as u64);
        let att_id = env["id"].as_str().unwrap().to_owned();

        // 2) Submit 带附件（caps 未知 → 放行）→ turn_ok。
        runtime
            .send(&format!(
                r#"{{"type":"submit","text":"看图","attachments":[{{"id":"{att_id}","mime":"image/png","filename":"shot.png"}}]}}"#
            ))
            .unwrap();
        collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));

        // 3) 捕获的请求：最后一条 user 消息带 1 个附件且 data 已装载
        //    （字节与文件一致）——AttachmentDataProxy 生效。
        {
            let reqs = probe.requests.lock().unwrap();
            let last = reqs.last().expect("至少一次模型请求");
            let user = last
                .iter()
                .rev()
                .find(|m| m.role == MessageRole::User)
                .expect("请求里有 user 消息");
            let atts = user.attachments.as_ref().expect("user 消息带附件引用");
            assert_eq!(atts.len(), 1);
            assert_eq!(atts[0].id, att_id);
            let data = atts[0].data.as_ref().expect("data 已由代理装载");
            assert_eq!(&**data, &png_bytes[..], "装载字节与文件一致");
        }

        // 4) 落库：附件行绑定到 run；user 消息持久化带引用（data 不进库）。
        let run_id = runtime.current_run_id().expect("run 已打开");
        let (srt, store) = scratch_store(&paths);
        srt.block_on(async {
            let row = store.get_attachment(&att_id).await.unwrap().unwrap();
            assert_eq!(row.run_id.as_deref(), Some(run_id.as_str()), "Submit 后绑定");
            let msgs =
                openslate_store_sqlite::recorder::RunRecorder::load_messages(&store, &run_id)
                    .await
                    .unwrap();
            let user = msgs
                .iter()
                .find(|m| m.role == MessageRole::User)
                .expect("user 消息已入库");
            let atts = user.attachments.as_ref().expect("持久化带附件引用");
            assert_eq!(atts.len(), 1);
            assert_eq!(atts[0].id, att_id);
            assert!(atts[0].data.is_none(), "data 不进持久化");
        });

        // 5) transcript 持久化：同步任务把 User 条目（带引用）落行。
        {
            let start = std::time::Instant::now();
            loop {
                let n = srt.block_on(async {
                    store
                        .list_transcript_entries(&run_id)
                        .await
                        .map(|v| v.len())
                        .unwrap_or(0)
                });
                if n >= 2 {
                    break;
                }
                assert!(start.elapsed() < Duration::from_secs(5), "transcript 行未落库");
                std::thread::sleep(Duration::from_millis(150));
            }
        }
        let (srt3, store3) = scratch_store(&paths);
        let (user_entry_json, kinds) = srt3.block_on(async {
            let rows = store3.list_transcript_entries(&run_id).await.unwrap();
            let kinds: Vec<String> = rows
                .iter()
                .filter_map(|r| serde_json::from_str::<EntryDto>(&r.entry_json).ok())
                .map(|e| match e {
                    EntryDto::User { .. } => "user".into(),
                    EntryDto::Assistant { .. } => "assistant".into(),
                    EntryDto::Meta { .. } => "meta".into(),
                    _ => "other".into(),
                })
                .collect();
            let user_json = rows
                .iter()
                .map(|r| r.entry_json.clone())
                .find(|j| j.contains("\"kind\":\"user\""))
                .expect("User 条目已持久化");
            (user_json, kinds)
        });
        assert!(kinds.contains(&"user".to_owned()), "{kinds:?}");
        assert!(kinds.contains(&"assistant".to_owned()), "{kinds:?}");
        assert!(
            user_entry_json.contains(&format!("\"id\":\"{att_id}\"")),
            "User 条目带附件引用: {user_entry_json}"
        );

        runtime.shutdown();
    }

    /// 门控矩阵（notice 通道拒绝；引擎零调用）：
    /// 纯文本模型发图拒 / 图视频混发拒 / 实验视频关拒 / 未知 id 拒 /
    /// 空文本拒；多模态（image+video caps）+ 开关开 + mp4 放行。
    #[test]
    fn attachments_gate_rejections_via_notice() {
        // (a) registry 声明纯文本 → 图像被拒。
        {
            let (paths, _dir) = temp_paths();
            paths.ensure_layout().unwrap();
            seed_registry_models_dev(&paths, models_dev_entry(&["text"]));
            let provider = CapturingProvider::new(vec![text_response("ok")]);
            let probe = provider.clone();
            let opts = RuntimeOptions {
                provider_factory: Some(Arc::new(move |_c, _a| {
                    Ok(Box::new(provider.clone())
                        as Box<dyn openslate_core::provider::ModelProvider>)
                })),
                ..Default::default()
            };
            let (collector, rx) = Collector::new();
            let runtime = MobileRuntime::create_with(paths, collector.clone(), opts).unwrap();
            collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
            let (png_path, _) = write_tiny_png(_dir.path());
            let env: serde_json::Value = serde_json::from_str(
                &runtime.import_attachment_json(png_path.to_str().unwrap(), "image/png", "a.png"),
            )
            .unwrap();
            let att_id = env["id"].as_str().unwrap();
            runtime
                .send(&format!(
                    r#"{{"type":"submit","text":"看图","attachments":[{{"id":"{att_id}","mime":"image/png"}}]}}"#
                ))
                .unwrap();
            let events = collector.wait_for(&rx, "notice", Duration::from_secs(10));
            let notice = events.iter().rev().find(|e| e["type"] == "notice").unwrap();
            assert!(notice["text"].as_str().unwrap().contains("不支持图像输入"), "{notice}");
            assert!(
                probe.requests.lock().unwrap().is_empty(),
                "拒绝后引擎零调用"
            );
            runtime.shutdown();
        }

        // (b) caps 未知：混发拒 / 实验视频关拒 / 空文本拒 / 未知 id 拒。
        {
            let (paths, _dir) = temp_paths();
            let provider = CapturingProvider::new(vec![text_response("ok")]);
            let opts = RuntimeOptions {
                provider_factory: Some(Arc::new(move |_c, _a| {
                    Ok(Box::new(provider.clone())
                        as Box<dyn openslate_core::provider::ModelProvider>)
                })),
                ..Default::default()
            };
            let (collector, rx) = Collector::new();
            let runtime = MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();
            collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
            let (png_path, _) = write_tiny_png(_dir.path());
            let mp4_path = write_fake_mp4(_dir.path());
            let img_id: String = {
                let env: serde_json::Value = serde_json::from_str(
                    &runtime.import_attachment_json(png_path.to_str().unwrap(), "image/png", "a.png"),
                )
                .unwrap();
                env["id"].as_str().unwrap().to_owned()
            };
            let vid_id: String = {
                let env: serde_json::Value = serde_json::from_str(
                    &runtime.import_attachment_json(mp4_path.to_str().unwrap(), "video/mp4", "c.mp4"),
                )
                .unwrap();
                env["id"].as_str().unwrap().to_owned()
            };
            // notice 逐条递增等待（wait_for 只等"≥1 条"，会拿到旧的）。
            let mut notice_count = 0usize;
            let expect_notice = |notice_count: &mut usize, substr: &str| {
                *notice_count += 1;
                let events =
                    collector.wait_for_count(&rx, "notice", *notice_count, Duration::from_secs(10));
                let n = events
                    .iter()
                    .filter(|e| e["type"] == "notice")
                    .nth(*notice_count - 1)
                    .unwrap();
                assert!(n["text"].as_str().unwrap().contains(substr), "{n}");
            };
            // 混发。
            runtime
                .send(&format!(
                    r#"{{"type":"submit","text":"混","attachments":[{{"id":"{img_id}","mime":"image/png"}},{{"id":"{vid_id}","mime":"video/mp4"}}]}}"#
                ))
                .unwrap();
            expect_notice(&mut notice_count, "图像与视频请分开发送");
            // 实验视频关（默认）。
            runtime
                .send(&format!(
                    r#"{{"type":"submit","text":"视频","attachments":[{{"id":"{vid_id}","mime":"video/mp4"}}]}}"#
                ))
                .unwrap();
            expect_notice(&mut notice_count, "实验性功能");
            // 空文本。
            runtime
                .send(&format!(
                    r#"{{"type":"submit","text":"  ","attachments":[{{"id":"{img_id}","mime":"image/png"}}]}}"#
                ))
                .unwrap();
            expect_notice(&mut notice_count, "请为附件输入文字说明");
            // 未知 id。
            runtime
                .send(r#"{"type":"submit","text":"图","attachments":[{"id":"att-nope","mime":"image/png"}]}"#)
                .unwrap();
            expect_notice(&mut notice_count, "附件不存在或已过期");
            runtime.shutdown();
        }

        // (c) 多模态 + 实验开关开（改 toml）→ 视频放行到引擎。
        {
            let (paths, _dir) = temp_paths();
            paths.ensure_layout().unwrap();
            seed_registry_models_dev(&paths, models_dev_entry(&["text", "image", "video"]));
            // 开实验视频开关（追加 [attachments] 节）。
            let cfg_path = paths.config_file();
            let mut body = std::fs::read_to_string(&cfg_path).unwrap();
            body.push_str("\n[attachments]\nexperimental_video = true\n");
            std::fs::write(&cfg_path, body).unwrap();
            let provider = CapturingProvider::new(vec![text_response("收到视频。")]);
            let probe = provider.clone();
            let opts = RuntimeOptions {
                provider_factory: Some(Arc::new(move |_c, _a| {
                    Ok(Box::new(provider.clone())
                        as Box<dyn openslate_core::provider::ModelProvider>)
                })),
                ..Default::default()
            };
            let (collector, rx) = Collector::new();
            let runtime = MobileRuntime::create_with(paths, collector.clone(), opts).unwrap();
            collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
            let mp4_path = write_fake_mp4(_dir.path());
            let env: serde_json::Value = serde_json::from_str(
                &runtime.import_attachment_json(mp4_path.to_str().unwrap(), "video/mp4", "c.mp4"),
            )
            .unwrap();
            let vid_id = env["id"].as_str().unwrap();
            runtime
                .send(&format!(
                    r#"{{"type":"submit","text":"看视频","attachments":[{{"id":"{vid_id}","mime":"video/mp4"}}]}}"#
                ))
                .unwrap();
            collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));
            let reqs = probe.requests.lock().unwrap();
            let user = reqs
                .last()
                .unwrap()
                .iter()
                .rev()
                .find(|m| m.role == MessageRole::User)
                .unwrap();
            assert_eq!(
                user.attachments.as_ref().unwrap()[0].mime, "video/mp4",
                "视频过门控且字节已装载"
            );
            assert!(user.attachments.as_ref().unwrap()[0].data.is_some());
        }
    }

    /// 实验视频开关（L4→L6 交接，FFI set/get 的 mobile 侧语义）：
    /// set 落盘（toml 其余内容零扰动）+ 内存即时生效 + 重启后从落盘
    /// 配置装载；反复切换改值不产生重复键。
    #[test]
    fn experimental_video_switch_persists_and_takes_effect() {
        let (paths, _dir) = temp_paths();
        paths.ensure_layout().unwrap();
        // 手工塞一行注释进配置：set 的写回必须零扰动保留它。
        let cfg_path = paths.config_file();
        let mut body = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(!body.contains("experimental_video"), "{body}");
        body.push_str("# marker comment\n");
        std::fs::write(&cfg_path, &body).unwrap();

        {
            let (collector, rx) = Collector::new();
            let runtime =
                MobileRuntime::create_with(paths.clone(), collector.clone(), Default::default())
                    .unwrap();
            collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
            assert!(!runtime.get_experimental_video(), "默认关");
            let resp: serde_json::Value =
                serde_json::from_str(&runtime.set_experimental_video(true)).unwrap();
            assert_eq!(resp["ok"], serde_json::json!(true), "{resp}");
            assert!(runtime.get_experimental_video(), "内存即时生效");
            runtime.shutdown();
        }
        // 落盘断言：注释保留 + 节就位 + 值正确。
        let body = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(body.contains("# marker comment"), "{body}");
        assert!(body.contains("experimental_video = true"), "{body}");
        // 重启后从落盘配置装载；关回 false 是改值而非追加。
        {
            let (collector, rx) = Collector::new();
            let runtime =
                MobileRuntime::create_with(paths.clone(), collector.clone(), Default::default())
                    .unwrap();
            collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
            assert!(runtime.get_experimental_video(), "重启后仍开");
            let resp: serde_json::Value =
                serde_json::from_str(&runtime.set_experimental_video(false)).unwrap();
            assert_eq!(resp["ok"], serde_json::json!(true), "{resp}");
            assert!(!runtime.get_experimental_video());
            runtime.shutdown();
        }
        let body = std::fs::read_to_string(&cfg_path).unwrap();
        assert_eq!(
            body.matches("experimental_video").count(),
            1,
            "改值不追加：{body}"
        );
        assert!(body.contains("experimental_video = false"), "{body}");
        assert!(body.contains("# marker comment"), "{body}");
    }

    /// 确定性噪点 JPEG（同 attachments.rs 测试的 LCG 像素源）。
    fn write_noise_jpeg(dir: &std::path::Path, w: u32, h: u32, q: u8) -> std::path::PathBuf {
        let mut px = image::RgbImage::new(w, h);
        let mut state = 0x2545F4914F6CDD1Du64;
        for y in 0..h {
            for x in 0..w {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let v = (state >> 33) as u8;
                px.put_pixel(x, y, image::Rgb([v, (v >> 1) as u8 | 0x80, 255 - v]));
            }
        }
        let mut jpeg = Vec::new();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, q);
        image::DynamicImage::ImageRgb8(px)
            .write_with_encoder(enc)
            .unwrap();
        let path = dir.join(format!("noise-{w}x{h}-q{q}.jpg"));
        std::fs::write(&path, &jpeg).unwrap();
        path
    }

    /// 图片处理双旋钮（FFI set/get_image_processing 的 mobile 侧语义）：
    /// 非法值拒（core validation 同源：dim 512–6000、mb 1–5）、合法值
    /// 落盘（toml 其余内容零扰动）+ 内存即时生效 + 重启装载 + **下一次
    /// 导入即按新旋钮走**（快速通道边界随用户 dim）。
    #[test]
    fn image_processing_knobs_persist_and_take_effect_on_next_import() {
        let (paths, _dir) = temp_paths();
        paths.ensure_layout().unwrap();
        // 手工塞一行注释进配置：set 的写回必须零扰动保留它。
        let cfg_path = paths.config_file();
        let mut body = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(!body.contains("max_image_dim"), "{body}");
        body.push_str("# marker comment\n");
        std::fs::write(&cfg_path, &body).unwrap();

        // 1600×1200 噪点 JPEG（~300KB，远低于大小上限）：默认旋钮下只看
        // dim 维——passthrough；旋钮 dim=1024 后 → transcode。
        let src = write_noise_jpeg(_dir.path(), 1600, 1200, 60);

        {
            let (collector, rx) = Collector::new();
            let runtime =
                MobileRuntime::create_with(paths.clone(), collector.clone(), scripted_opts(vec![]))
                    .unwrap();
            collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

            // 默认值回显（2048 / 5）。
            let get: serde_json::Value =
                serde_json::from_str(&runtime.get_image_processing()).unwrap();
            assert_eq!(get["max_image_dim"], serde_json::json!(2048), "{get}");
            assert_eq!(get["max_image_mb"], serde_json::json!(5), "{get}");

            // 默认旋钮：1600px 小文件 → passthrough。
            let env: serde_json::Value = serde_json::from_str(
                &runtime.import_attachment_json(src.to_str().unwrap(), "image/jpeg", "knob.jpg"),
            )
            .unwrap();
            assert_eq!(env["processed"], serde_json::json!("passthrough"), "{env}");

            // 非法值矩阵：dim 100/7000、mb 0/6 → ok:false 且值不动内存。
            for (d, m) in [(100i64, 5i64), (7000, 5), (2048, 0), (2048, 6)] {
                let resp: serde_json::Value =
                    serde_json::from_str(&runtime.set_image_processing(d, m)).unwrap();
                assert_eq!(resp["ok"], serde_json::json!(false), "{d}/{m}: {resp}");
                assert!(
                    resp["error"].as_str().is_some_and(|e| !e.is_empty()),
                    "{d}/{m}: {resp}"
                );
            }
            let get: serde_json::Value =
                serde_json::from_str(&runtime.get_image_processing()).unwrap();
            assert_eq!(get["max_image_dim"], serde_json::json!(2048), "非法值不动内存");
            assert_eq!(get["max_image_mb"], serde_json::json!(5));

            // 合法旋钮 dim=1024：同一文件下一次导入 → transcode（缩到
            // ≤1024；1600×1200 → 1024×768）。
            let resp: serde_json::Value =
                serde_json::from_str(&runtime.set_image_processing(1024, 5)).unwrap();
            assert_eq!(resp["ok"], serde_json::json!(true), "{resp}");
            let env: serde_json::Value = serde_json::from_str(
                &runtime.import_attachment_json(src.to_str().unwrap(), "image/jpeg", "knob2.jpg"),
            )
            .unwrap();
            assert!(env["ok"].as_bool().unwrap(), "{env}");
            assert_eq!(env["processed"], serde_json::json!("transcode"), "{env}");
            assert_eq!(env["width"], serde_json::json!(1024), "{env}");
            assert_eq!(env["height"], serde_json::json!(768), "{env}");
            runtime.shutdown();
        }
        // 落盘断言：注释保留 + 两键就位 + 值正确。
        let body = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(body.contains("# marker comment"), "{body}");
        assert!(body.contains("max_image_dim = 1024"), "{body}");
        assert!(body.contains("max_image_mb = 5"), "{body}");
        // 重启后从落盘装载。
        {
            let (collector, rx) = Collector::new();
            let runtime =
                MobileRuntime::create_with(paths.clone(), collector.clone(), scripted_opts(vec![]))
                    .unwrap();
            collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
            let get: serde_json::Value =
                serde_json::from_str(&runtime.get_image_processing()).unwrap();
            assert_eq!(get["max_image_dim"], serde_json::json!(1024), "重启后仍生效");
            assert_eq!(get["max_image_mb"], serde_json::json!(5));
            runtime.shutdown();
        }
    }

    /// resume 回填：带图 run 重启恢复（assemble_state restore 路径）后
    /// history 的附件 data 已回填（下轮请求直接带图）。
    #[test]
    fn attachments_resume_refills_data_on_restart() {
        let (paths, _dir) = temp_paths();
        {
            let provider = CapturingProvider::new(vec![text_response("第一轮。")]);
            let _probe = provider.clone();
            let opts = RuntimeOptions {
                provider_factory: Some(Arc::new(move |_c, _a| {
                    Ok(Box::new(provider.clone())
                        as Box<dyn openslate_core::provider::ModelProvider>)
                })),
                ..Default::default()
            };
            let (collector, rx) = Collector::new();
            let runtime =
                MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();
            collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
            let (png_path, _png_bytes) = write_tiny_png(_dir.path());
            let env: serde_json::Value = serde_json::from_str(
                &runtime.import_attachment_json(png_path.to_str().unwrap(), "image/png", "s.png"),
            )
            .unwrap();
            let att_id = env["id"].as_str().unwrap().to_owned();
            runtime
                .send(&format!(
                    r#"{{"type":"submit","text":"看图","attachments":[{{"id":"{att_id}","mime":"image/png"}}]}}"#
                ))
                .unwrap();
            collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));
            runtime.shutdown();
        }
        // 重启：assemble_state 恢复最近 run，回填附件 data；第二轮请求
        // （无新附件提交）仍带图。
        {
            let provider = CapturingProvider::new(vec![text_response("第二轮。")]);
            let probe = provider.clone();
            let opts = RuntimeOptions {
                provider_factory: Some(Arc::new(move |_c, _a| {
                    Ok(Box::new(provider.clone())
                        as Box<dyn openslate_core::provider::ModelProvider>)
                })),
                ..Default::default()
            };
            let (collector, rx) = Collector::new();
            let runtime =
                MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();
            collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
            runtime.send(r#"{"type":"submit","text":"刚才那张图是什么颜色？"}"#).unwrap();
            collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));
            let reqs = probe.requests.lock().unwrap();
            let user = reqs
                .last()
                .unwrap()
                .iter()
                .rev()
                .find(|m| m.role == MessageRole::User && m.attachments.is_some())
                .expect("恢复后的历史 user 消息带附件");
            let att = &user.attachments.as_ref().unwrap()[0];
            assert!(att.data.is_some(), "resume 回填后 data 在场（历史图随轮重发）");
            runtime.shutdown();
        }
    }

    /// 启动 GC：7 天前导入未发送的暂存附件被清（行 + 文件）。
    #[test]
    fn startup_gc_clears_stale_unbound_attachments() {
        let (paths, _dir) = temp_paths();
        paths.ensure_layout().unwrap();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let stale_sha;
        {
            let (srt, store) = scratch_store(&paths);
            srt.block_on(async {
                store
                    .insert_attachment(
                        "att-stale",
                        None,
                        "deadbeef",
                        "image/png",
                        None,
                        8,
                        b"pngbytes",
                        now_ms - 8 * 24 * 3600 * 1000,
                    )
                    .await
                    .unwrap();
                store
                    .insert_attachment(
                        "att-fresh",
                        None,
                        "cafef00d",
                        "image/png",
                        None,
                        8,
                        b"pngbytes",
                        now_ms,
                    )
                    .await
                    .unwrap();
            });
            stale_sha = "deadbeef".to_owned();
        }

        let (collector, _rx) = Collector::new();
        let runtime =
            MobileRuntime::create_with(paths.clone(), collector, scripted_opts(vec![])).unwrap();

        let (srt, store) = scratch_store(&paths);
        let (stale, fresh) = srt.block_on(async {
            (
                store.get_attachment("att-stale").await.unwrap(),
                store.get_attachment("att-fresh").await.unwrap(),
            )
        });
        assert!(stale.is_none(), "8 天前暂存被 GC");
        assert!(fresh.is_some(), "新暂存保留");
        let file_gone = store
            .files_dir()
            .map(|d| !d.join(format!("{stale_sha}.png")).exists())
            .unwrap_or(true);
        assert!(file_gone, "对应文件已清扫");

        runtime.shutdown();
    }

    /// transcript 持久化往返：完成一个带附件回合 → 行落库 → open_session
    /// 从行重建（快照含 User 附件引用）；旧 run（无行）走投影回退不回归。
    #[test]
    fn transcript_entries_roundtrip_and_old_run_fallback() {
        let (paths, _dir) = temp_paths();
        let provider = CapturingProvider::new(vec![text_response("回答")]);
        let opts = RuntimeOptions {
            provider_factory: Some(Arc::new(move |_c, _a| {
                Ok(Box::new(provider.clone()) as Box<dyn openslate_core::provider::ModelProvider>)
            })),
            ..Default::default()
        };
        let (collector, rx) = Collector::new();
        let runtime =
            MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

        let (png_path, _) = write_tiny_png(_dir.path());
        let env: serde_json::Value = serde_json::from_str(
            &runtime.import_attachment_json(png_path.to_str().unwrap(), "image/png", "s.png"),
        )
        .unwrap();
        let att_id = env["id"].as_str().unwrap().to_owned();
        runtime
            .send(&format!(
                r#"{{"type":"submit","text":"看图","attachments":[{{"id":"{att_id}","mime":"image/png"}}]}}"#
            ))
            .unwrap();
        collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));
        let run_id = runtime.current_run_id().unwrap();

        // 等同步任务落库（活跃期 300ms 周期）。
        {
            let (srt, store) = scratch_store(&paths);
            let start = std::time::Instant::now();
            loop {
                let n = srt.block_on(async {
                    store
                        .list_transcript_entries(&run_id)
                        .await
                        .map(|v| v.len())
                        .unwrap_or(0)
                });
                if n >= 2 {
                    break;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "transcript_entries 未落库"
                );
                std::thread::sleep(Duration::from_millis(150));
            }
        }

        // 新会话切走再 open_session 回来：transcript 从 entries 行重建
        // （User 条目带附件引用——这是投影回退拿不到的字段一致性验证）。
        runtime.send(r#"{"type":"new_session"}"#).unwrap();
        collector.wait_for(&rx, "session_reset", Duration::from_secs(5));
        runtime.open_session(run_id.clone()).unwrap();
        // 快照序：create(1) → new_session 补发(2) → open_session(3)。
        let events = collector.wait_for_count(&rx, "snapshot", 3, Duration::from_secs(10));
        let snaps: Vec<&serde_json::Value> = events
            .iter()
            .filter(|e| e["type"] == "snapshot")
            .collect();
        assert_eq!(snaps.len(), 3);
        let snap = snaps[2];
        // C2：!same_run 重开轮换 session_id（ChatScreen 会话揭示依赖它
        // 变化；同一 run 重开则保持——另见专项测试）。
        assert_ne!(
            snap["session"]["session_id"], snaps[1]["session"]["session_id"],
            "切换到另一 run 应轮换 session_id"
        );
        let transcript = snap["session"]["transcript"].as_array().unwrap();
        let user_entry = transcript
            .iter()
            .find(|e| e["kind"] == "user")
            .expect("User 条目在场");
        assert!(
            user_entry["attachments"][0]["id"].as_str() == Some(att_id.as_str()),
            "entries 行重建的 User 条目带附件引用: {user_entry}"
        );

        // 旧 run（v5 前，无 entries 行）→ 投影回退不回归。
        let old_run = "legacy-run-no-entries";
        {
            let (srt, store) = scratch_store(&paths);
            srt.block_on(async {
                store
                    .insert_run(old_run, Some("旧会话"), "root", "completed", "{}", 1000)
                    .await
                    .unwrap();
                store
                    .insert_execution_node("legacy-persist", old_run, "root", None, None, "completed", "{}", 1000)
                    .await
                    .unwrap();
                let user_msg = Message {
                    attachments: None,
                    role: MessageRole::User,
                    content: "旧消息".into(),
                    tool_call_id: None,
                    name: None,
                    tool_calls: None,
                    reasoning_content: None,
                };
                store
                    .insert_message(
                        "legacy-msg-1",
                        old_run,
                        "legacy-persist",
                        Some("root"),
                        "user",
                        &openslate_store_sqlite::codec::encode_content(
                            &serde_json::to_string(&user_msg).unwrap(),
                        ),
                        None,
                        1,
                        1001,
                    )
                    .await
                    .unwrap();
            });
        }
        runtime.open_session(old_run.to_owned()).unwrap();
        let events = collector.wait_for_count(&rx, "snapshot", 4, Duration::from_secs(10));
        let snap = events
            .iter()
            .rev()
            .find(|e| e["type"] == "snapshot")
            .unwrap();
        let transcript = snap["session"]["transcript"].as_array().unwrap();
        assert!(
            transcript.iter().any(|e| e["kind"] == "user" && e["text"] == "旧消息"),
            "无 entries 行的旧 run 走消息投影回退: {transcript:?}"
        );

        runtime.shutdown();
    }

    // ── 历史转写丢失修复（C1 前置冲刷 / C2 pop 回写 + 会话轮换 / C3 自愈）──

    /// 等待 sync_loop 把 run 的 entries 行数写到 ≥ want（超时 panic；
    /// scratch 连接独立于 runtime 私有池）。
    fn wait_entry_count(paths: &MobilePaths, run_id: &str, want: usize) {
        let (srt, store) = scratch_store(paths);
        let start = std::time::Instant::now();
        loop {
            let n = srt.block_on(async {
                store
                    .list_transcript_entries(run_id)
                    .await
                    .map(|v| v.len())
                    .unwrap_or(0)
            });
            if n >= want {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "entries 未落库（有 {n}，期望 ≥ {want}）"
            );
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// 种子一条消息行（content = Message JSON 压缩，与 recorder 同口径）。
    async fn seed_message(
        store: &openslate_store_sqlite::store::SqliteStore,
        run: &str,
        node: &str,
        id: &str,
        seq: i64,
        msg: &Message,
    ) {
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
            MessageRole::System => "system",
        };
        store
            .insert_message(
                id,
                run,
                node,
                Some("root"),
                role,
                &openslate_store_sqlite::codec::encode_content(
                    &serde_json::to_string(msg).unwrap(),
                ),
                None,
                seq,
                1000 + seq,
            )
            .await
            .unwrap();
    }

    /// 悬挂形态的 assistant 消息（带未答 tool_calls）。
    fn dangling_assistant(call_id: &str) -> Message {
        Message {
            attachments: None,
            role: MessageRole::Assistant,
            content: String::new(),
            tool_call_id: None,
            name: None,
            tool_calls: Some(vec![openslate_core::types::ToolCall {
                id: openslate_core::types::ToolCallId(call_id.into()),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "ls"}),
            }]),
            reasoning_content: None,
        }
    }

    /// T3（必做）：悬挂尾巴 pop 回写 DB——第一次 open 删行（messages 尾行
    /// + 全部 entries）+ 投影重建；第二次 open 不再命中 pop 分支（G-1）、
    /// 无新增 Meta 行。
    #[test]
    fn dangling_pop_persists_to_db_second_open_is_clean() {
        let (paths, _dir) = temp_paths();
        let (collector, rx) = Collector::new();
        let runtime =
            MobileRuntime::create_with(paths.clone(), collector.clone(), scripted_opts(vec![]))
                .unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

        // 种一个尾部悬挂的 run：messages = [user, assistant(dangling)]，
        // entries = [User, ToolCall(未折叠终值)]——正是会被反复 pop 的形状。
        let seed_run = "dangling-run";
        {
            let (srt, store) = scratch_store(&paths);
            srt.block_on(async {
                store
                    .insert_run(seed_run, Some("悬挂会话"), "root", "interrupted", "{}", 1000)
                    .await
                    .unwrap();
                store
                    .insert_execution_node(
                        "dangling-node", seed_run, "root", None, None, "completed", "{}", 1000,
                    )
                    .await
                    .unwrap();
                seed_message(
                    &store,
                    seed_run,
                    "dangling-node",
                    "dangling-m1",
                    1,
                    &Message {
                        attachments: None,
                        role: MessageRole::User,
                        content: "帮我查目录".into(),
                        tool_call_id: None,
                        name: None,
                        tool_calls: None,
                        reasoning_content: None,
                    },
                )
                .await;
                seed_message(
                    &store, seed_run, "dangling-node", "dangling-m2", 2,
                    &dangling_assistant("tc-dangling"),
                )
                .await;
                store
                    .insert_transcript_entry(
                        "te-d0",
                        seed_run,
                        0,
                        &serde_json::to_string(&EntryDto::User {
                            text: "帮我查目录".into(),
                            attachments: None,
                        })
                        .unwrap(),
                        1200,
                    )
                    .await
                    .unwrap();
                let tc = EntryDto::ToolCall {
                    name: "bash".into(),
                    args: r#"{"command":"ls"}"#.into(),
                    call_id: Some("tc-dangling".into()),
                    status: openslate_protocol::ToolStatusDto::Running,
                    detail: openslate_protocol::ToolEntryDetailDto {
                        args: r#"{"command":"ls"}"#.into(),
                        output: None,
                    },
                };
                store
                    .insert_transcript_entry(
                        "te-d1",
                        seed_run,
                        1,
                        &serde_json::to_string(&tc).unwrap(),
                        1201,
                    )
                    .await
                    .unwrap();
            });
        }

        // 第一次 open：pop 发生 → DB 回写（messages 尾行删除、entries 全
        // 删）→ 转写走投影重建 + 恰一条 DANGLING Meta。
        runtime.open_session(seed_run.to_owned()).unwrap();
        let events = collector.wait_for_count(&rx, "snapshot", 2, Duration::from_secs(10));
        let snap = events.iter().rev().find(|e| e["type"] == "snapshot").unwrap();
        let transcript = snap["session"]["transcript"].as_array().unwrap();
        let metas: Vec<_> = transcript
            .iter()
            .filter(|e| e["kind"] == "meta")
            .collect();
        assert_eq!(metas.len(), 1, "恰一条 Meta: {transcript:?}");
        assert_eq!(
            metas[0]["text"].as_str().unwrap(),
            crate::transcripts::DANGLING_TAIL_META
        );

        // 等 sync_loop 把重建转写全量写回（游标已失效 → 自举 0）。
        wait_entry_count(&paths, seed_run, 2);

        // G-1 + T3 断言：第二次 open 不再命中 pop——无新增 Meta、
        // messages 尾行（悬挂 assistant）确已删除。
        runtime.open_session(seed_run.to_owned()).unwrap();
        let events = collector.wait_for_count(&rx, "snapshot", 3, Duration::from_secs(10));
        let snap = events.iter().rev().find(|e| e["type"] == "snapshot").unwrap();
        let transcript = snap["session"]["transcript"].as_array().unwrap();
        assert_eq!(
            transcript.iter().filter(|e| e["kind"] == "meta").count(),
            1,
            "第二次 open 无新增 Meta（DB 已删干净，不再 pop）: {transcript:?}"
        );
        {
            let (srt, store) = scratch_store(&paths);
            let (msgs, entries) = srt.block_on(async {
                (
                    store.list_messages_by_run(seed_run).await.unwrap().len(),
                    store.list_transcript_entries(seed_run).await.unwrap().len(),
                )
            });
            assert_eq!(msgs, 1, "被 pop 的悬挂 assistant 尾行已从 DB 删除");
            assert_eq!(entries, 2, "重建转写已全量写回（User + Meta）");
        }

        runtime.shutdown();
    }

    /// T2（必做）：entries 截断自愈（C3）幂等——messages=5 行 / entries=2
    /// 行的 run → open 转写 = 2 前缀 + HEAL Meta + 投影尾 3；再 open 结果
    /// 一致（不重复 heal、不丢内容）。
    #[test]
    fn heal_truncated_entries_idempotent_on_reopen() {
        let (paths, _dir) = temp_paths();
        let (collector, rx) = Collector::new();
        let runtime =
            MobileRuntime::create_with(paths.clone(), collector.clone(), scripted_opts(vec![]))
                .unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

        let plain = |role: MessageRole, text: &str| Message {
            attachments: None,
            role,
            content: text.into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        };
        // 5 行消息（尾是 user → 无 pop 干扰），entries 只落前 2 条投影。
        let messages = vec![
            plain(MessageRole::User, "问1"),
            plain(MessageRole::Assistant, "答1"),
            plain(MessageRole::User, "问2"),
            plain(MessageRole::Assistant, "答2"),
            plain(MessageRole::User, "问3"),
        ];
        let seed_run = "truncated-run";
        {
            let (srt, store) = scratch_store(&paths);
            srt.block_on(async {
                store
                    .insert_run(seed_run, Some("截断会话"), "root", "completed", "{}", 1000)
                    .await
                    .unwrap();
                store
                    .insert_execution_node(
                        "trunc-node", seed_run, "root", None, None, "completed", "{}", 1000,
                    )
                    .await
                    .unwrap();
                for (i, msg) in messages.iter().enumerate() {
                    seed_message(
                        &store,
                        seed_run,
                        "trunc-node",
                        &format!("trunc-m{i}"),
                        i as i64 + 1,
                        msg,
                    )
                    .await;
                }
                store
                    .insert_transcript_entry(
                        "te-t0",
                        seed_run,
                        0,
                        &serde_json::to_string(&EntryDto::User {
                            text: "问1".into(),
                            attachments: None,
                        })
                        .unwrap(),
                        1200,
                    )
                    .await
                    .unwrap();
                store
                    .insert_transcript_entry(
                        "te-t1",
                        seed_run,
                        1,
                        &serde_json::to_string(&EntryDto::Assistant { text: "答1".into() })
                            .unwrap(),
                        1201,
                    )
                    .await
                    .unwrap();
            });
        }

        let assert_transcript = |snap: &serde_json::Value| {
            let transcript = snap["session"]["transcript"].as_array().unwrap();
            let texts: Vec<&str> = transcript
                .iter()
                .map(|e| e.get("text").and_then(|t| t.as_str()).unwrap_or(""))
                .collect();
            assert_eq!(
                texts,
                vec!["问1", "答1", crate::transcripts::HEAL_META, "问2", "答2", "问3"],
                "2 前缀 + HEAL Meta + 投影尾 3: {transcript:?}"
            );
        };

        // 第一次 open：heal 触发。
        runtime.open_session(seed_run.to_owned()).unwrap();
        let events = collector.wait_for_count(&rx, "snapshot", 2, Duration::from_secs(10));
        let snap = events.iter().rev().find(|e| e["type"] == "snapshot").unwrap();
        assert_transcript(snap);

        // 等 sync_loop 把补齐尾部按接续 seq 写回（行数 2 → 6）。
        wait_entry_count(&paths, seed_run, 6);

        // 第二次 open：从 6 行重建 → 无缺尾 → 不再 heal，结果一致。
        runtime.open_session(seed_run.to_owned()).unwrap();
        let events = collector.wait_for_count(&rx, "snapshot", 3, Duration::from_secs(10));
        let snap = events.iter().rev().find(|e| e["type"] == "snapshot").unwrap();
        assert_transcript(snap);

        runtime.shutdown();
    }

    /// 立即成功的测试工具（T1：模型点名的工具真实执行、Tool 消息落库）。
    struct FastProbeTool;

    #[async_trait]
    impl openslate_core::tool::Tool for FastProbeTool {
        fn name(&self) -> &str {
            "fast_probe"
        }
        fn namespace(&self) -> Option<String> {
            None
        }
        fn description(&self) -> &str {
            "Returns immediately (test fixture)."
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{},"additionalProperties":false})
        }
        async fn execute(
            &self,
            _args: &serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                content: "ok".into(),
                bytes: 2,
                duration_ms: 1,
                status: openslate_core::types::ToolOutputStatus::Success,
                attachments: None,
            })
        }
    }

    /// 永不返回的工具（T4/T5：把回合钉在 running=true）。
    struct HangingTool;

    #[async_trait]
    impl openslate_core::tool::Tool for HangingTool {
        fn name(&self) -> &str {
            "hang_forever"
        }
        fn namespace(&self) -> Option<String> {
            None
        }
        fn description(&self) -> &str {
            "Never returns (test fixture; simulates a stuck tool)."
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{},"additionalProperties":false})
        }
        async fn execute(
            &self,
            _args: &serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            std::future::pending().await
        }
    }

    /// 点名工具的流脚本（无正文——工具调用回合的标准形状）。
    fn tool_call_step(name: &str, call_id: &str) -> Vec<ModelStreamEvent> {
        vec![ModelStreamEvent::Done(ModelResponse {
            reasoning_content: None,
            content: None,
            tool_calls: vec![openslate_core::types::ToolCall {
                id: openslate_core::types::ToolCallId(call_id.into()),
                name: name.into(),
                arguments: serde_json::json!({}),
            }],
            usage: None,
            finish_reason: Some("tool_calls".into()),
        })]
    }

    /// T1 用 provider：首次调用回放脚本，之后的调用立刻吐流错误（回合
    /// Err 收尾——折叠不运行，ToolCall 条目保持未定稿形态）。
    #[derive(Clone)]
    struct ToolThenErrorProvider {
        scripts: Arc<Mutex<VecDeque<Vec<ModelStreamEvent>>>>,
    }

    #[async_trait]
    impl ModelProvider for ToolThenErrorProvider {
        async fn generate(
            &self,
            _request: GenerateRequest,
        ) -> Result<ModelResponse, ProviderError> {
            Err(ProviderError::ServerError(500))
        }

        async fn generate_stream(
            &self,
            _request: GenerateRequest,
        ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let script = self.scripts.lock().unwrap().pop_front();
            tokio::spawn(async move {
                match script {
                    Some(events) => {
                        for event in events {
                            let _ = tx.send(Ok(event)).await;
                        }
                    }
                    None => {
                        let _ = tx
                            .send(Err(ProviderError::ServerError(503)))
                            .await;
                    }
                }
            });
            rx
        }

        fn provider_name(&self) -> &str {
            "tool-then-error"
        }
    }

    /// 审批缺省放开为 allow（恢复 gating 前的工具直通；host_call 测试同款）。
    fn relax_approval(runtime: &MobileRuntime, collector: &Collector, rx: &std_mpsc::Receiver<()>) {
        runtime
            .send(r#"{"type":"set_approval_config","default_level":"allow","tool_levels":{},"server_levels":{}}"#)
            .unwrap();
        collector.wait_for(rx, "config_changed", Duration::from_secs(10));
    }

    /// T1（尽量做）：回合 Err 收尾（未折叠终值的 ToolCall 留在内存转写）
    /// → NewSession 前置冲刷 → DB 含 as-is ToolCall + INCOMPLETE_TURN_
    /// META；重开转写完整。
    #[test]
    fn interrupted_turn_new_session_flushes_snapshot_meta() {
        let (paths, _dir) = temp_paths();
        let provider = ToolThenErrorProvider {
            scripts: Arc::new(Mutex::new(VecDeque::from(vec![tool_call_step(
                "fast_probe",
                "tc-t1",
            )]))),
        };
        let opts = RuntimeOptions {
            provider_factory: Some(Arc::new(move |_c, _a| {
                Ok(Box::new(provider.clone()) as Box<dyn ModelProvider>)
            })),
            extra_tools: vec![Box::new(FastProbeTool)],
            ..Default::default()
        };
        let (collector, rx) = Collector::new();
        let runtime = MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
        relax_approval(&runtime, &collector, &rx);

        // 步骤 1 工具执行成功（ToolCall 条目 live Done、output 未折叠），
        // 步骤 2 模型流错误 → turn_error（Err 路径不折叠）。
        runtime.send(r#"{"type":"submit","text":"跑探针"}"#).unwrap();
        collector.wait_for(&rx, "turn_error", Duration::from_secs(20));
        let run_id = runtime.current_run_id().expect("run 应已打开");

        // NewSession：前置冲刷把推迟的 ToolCall 尾部 as-is 落库 + 中断
        // 快照 Meta 行。
        runtime.send(r#"{"type":"new_session"}"#).unwrap();
        collector.wait_for(&rx, "session_reset", Duration::from_secs(5));

        {
            let (srt, store) = scratch_store(&paths);
            let rows = srt
                .block_on(async { store.list_transcript_entries(&run_id).await.unwrap() });
            let jsons: Vec<&str> = rows.iter().map(|r| r.entry_json.as_str()).collect();
            assert!(
                jsons.iter().any(|j| j.contains(r#""kind":"tool_call""#) && j.contains(r#""output":null"#)),
                "DB 应含 as-is ToolCall（未折叠终值）: {jsons:?}"
            );
            assert!(
                jsons
                    .iter()
                    .any(|j| j.contains(crate::transcripts::INCOMPLETE_TURN_META)),
                "DB 应含 INCOMPLETE_TURN_META 行: {jsons:?}"
            );
        }

        // 重开：转写完整（用户消息在场——不再只剩截断前缀）。
        runtime.open_session(run_id.clone()).unwrap();
        let events = collector.wait_for_count(&rx, "snapshot", 3, Duration::from_secs(10));
        let snap = events.iter().rev().find(|e| e["type"] == "snapshot").unwrap();
        let transcript = snap["session"]["transcript"].as_array().unwrap();
        assert!(
            transcript.iter().any(|e| e["kind"] == "user" && e["text"] == "跑探针"),
            "重开转写完整: {transcript:?}"
        );

        runtime.shutdown();
    }

    /// T4（尽量做）：NewSession 二次 busy 守卫——前置冲刷期间被注入的
    /// submit 起了回合 → NewSession 被拒（Warn notice），转写未清、
    /// session_id 未轮换。
    struct SubmitInjectingFlusher;

    impl openslate_session::state::TranscriptFlusher for SubmitInjectingFlusher {
        fn flush(&self, state: Arc<AppState>) -> futures::future::BoxFuture<'static, ()> {
            Box::pin(async move {
                // 模拟冲刷窗口内的竞态 submit（挂死工具钉住 running=true）。
                session_dispatch::dispatch(
                    &state,
                    crate::events::MOBILE_CONN_ID,
                    ClientMsg::Submit {
                        text: "抢跑提交".into(),
                        attachments: None,
                    },
                )
                .await;
            })
        }
    }

    #[test]
    fn new_session_second_busy_guard_rejects_when_submit_lands_in_flush() {
        let (paths, _dir) = temp_paths();
        let provider = ToolThenErrorProvider {
            scripts: Arc::new(Mutex::new(VecDeque::from(vec![tool_call_step(
                "hang_forever",
                "tc-t4",
            )]))),
        };
        let opts = RuntimeOptions {
            provider_factory: Some(Arc::new(move |_c, _a| {
                Ok(Box::new(provider.clone()) as Box<dyn ModelProvider>)
            })),
            extra_tools: vec![Box::new(HangingTool)],
            ..Default::default()
        };
        let (collector, rx) = Collector::new();
        let runtime = MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
        relax_approval(&runtime, &collector, &rx);

        // 预置转写标记 + 记录当前会话身份。
        let sid_before = {
            let mut inner = runtime.state.core.lock();
            inner.transcript.push(EntryDto::Meta {
                text: "t4-marker".into(),
            });
            inner.session_id.clone()
        };

        // 覆盖冲刷钩子为「注入 submit」的实现，再发 NewSession。
        runtime
            .state
            .core
            .set_transcript_flusher(Arc::new(SubmitInjectingFlusher));
        runtime.send(r#"{"type":"new_session"}"#).unwrap();

        // 二次守卫拒绝：Warn notice（回合进行中）；无 session_reset。
        let events = collector.wait_for(&rx, "notice", Duration::from_secs(10));
        let notice = events
            .iter()
            .rev()
            .find(|e| e["type"] == "notice" && e["text"].as_str().unwrap().contains("回合进行中"))
            .expect("二次 busy 守卫应定向 Warn notice");
        assert_eq!(notice["level"].as_str().unwrap(), "warn");

        // 转写未清（标记仍在 + 抢跑 user 条目）、session_id 未轮换。
        {
            let inner = runtime.state.core.lock();
            assert!(
                inner
                    .transcript
                    .iter()
                    .any(|e| matches!(e, EntryDto::Meta { text } if text == "t4-marker")),
                "转写未被清空"
            );
            assert_eq!(inner.session_id, sid_before, "被拒路径不轮换 session_id");
        }

        runtime.shutdown();
    }

    /// T5（尽量做）：回合中（工具挂死）shutdown → 关停冲刷把 ToolCall
    /// 中断快照落库（entries 含尾部 + INCOMPLETE_TURN_META）。
    #[test]
    fn shutdown_flushes_transcript_tail_mid_turn() {
        let (paths, _dir) = temp_paths();
        let provider = ToolThenErrorProvider {
            scripts: Arc::new(Mutex::new(VecDeque::from(vec![tool_call_step(
                "hang_forever",
                "tc-t5",
            )]))),
        };
        let opts = RuntimeOptions {
            provider_factory: Some(Arc::new(move |_c, _a| {
                Ok(Box::new(provider.clone()) as Box<dyn ModelProvider>)
            })),
            extra_tools: vec![Box::new(HangingTool)],
            ..Default::default()
        };
        let (collector, rx) = Collector::new();
        let runtime = MobileRuntime::create_with(paths.clone(), collector.clone(), opts).unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
        relax_approval(&runtime, &collector, &rx);

        // 回合进行中（工具挂死）：tool_start 事件 = ToolCall 条目已在
        // 内存转写（推迟落库）。
        runtime.send(r#"{"type":"submit","text":"挂住别动"}"#).unwrap();
        collector.wait_for(&rx, "tool_start", Duration::from_secs(20));
        let run_id = runtime.current_run_id().expect("run 应已打开");

        runtime.shutdown();

        // 关停冲刷已把 User + as-is ToolCall + 中断快照 Meta 落库。
        let (srt, store) = scratch_store(&paths);
        let rows = srt.block_on(async { store.list_transcript_entries(&run_id).await.unwrap() });
        let jsons: Vec<&str> = rows.iter().map(|r| r.entry_json.as_str()).collect();
        assert!(
            jsons.iter().any(|j| j.contains(r#""kind":"user""#) && j.contains("挂住别动")),
            "entries 应含 User 尾部: {jsons:?}"
        );
        assert!(
            jsons
                .iter()
                .any(|j| j.contains(r#""kind":"tool_call""#) && j.contains(r#""output":null"#)),
            "entries 应含 as-is ToolCall: {jsons:?}"
        );
        assert!(
            jsons
                .iter()
                .any(|j| j.contains(crate::transcripts::INCOMPLETE_TURN_META)),
            "entries 应含 INCOMPLETE_TURN_META: {jsons:?}"
        );
    }

    // ── 重启恢复 pop 兜底守卫（may_pop_dangling_tail）────────────────

    #[test]
    fn may_pop_dangling_tail_guard_semantics() {
        fn tail(ids: &[&str]) -> Message {
            Message {
                attachments: None,
                role: MessageRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                name: None,
                tool_calls: Some(
                    ids.iter()
                        .map(|i| openslate_core::types::ToolCall {
                            id: openslate_core::types::ToolCallId((*i).into()),
                            name: "ask_user".into(),
                            arguments: serde_json::json!({}),
                        })
                        .collect(),
                ),
                reasoning_content: None,
            }
        }
        fn user() -> Message {
            Message {
                attachments: None,
                role: MessageRole::User,
                content: "go".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            }
        }
        // 空历史 / 尾部非悬挂 assistant → 无事可 pop。
        assert!(!may_pop_dangling_tail(&[], &[]));
        assert!(!may_pop_dangling_tail(&[user()], &[]));
        // 尾部悬挂 assistant + 无任何 pending 引用 → 允许 pop（经典
        // 「模型重新决策」语义回归）。
        assert!(may_pop_dangling_tail(&[user(), tail(&["tc-1"])], &[]));
        assert!(may_pop_dangling_tail(
            &[user(), tail(&["tc-1"])],
            &["tc-other".to_owned()]
        ));
        // 尾部声明的 tool_call 被 pending 行引用（哪怕只引用其一）→
        // 禁止 pop：迟到答案 / TTL 过期的补全点不可逆。
        assert!(!may_pop_dangling_tail(
            &[user(), tail(&["tc-1"])],
            &["tc-1".to_owned()]
        ));
        assert!(!may_pop_dangling_tail(
            &[user(), tail(&["tc-1", "tc-2"])],
            &["tc-2".to_owned()]
        ));
    }

    /// 种子数据：interrupted run，消息 = user + 尾部悬挂 assistant
    /// （ask_user 声明）；可选插入引用该 tool_call 的 pending 问卷行。
    fn seed_run_with_dangling_tail(
        paths: &MobilePaths,
        run_id: &str,
        tool_call_id: &str,
        with_row: bool,
    ) {
        let (srt, store) = scratch_store(paths);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        srt.block_on(async {
            store
                .insert_run(run_id, Some("pop 守卫"), "root", "interrupted", "{}", now_ms - 60_000)
                .await
                .unwrap();
            store
                .insert_execution_node(
                    &format!("{run_id}-persist"),
                    run_id,
                    "root",
                    None,
                    None,
                    "running",
                    "{}",
                    now_ms - 60_000,
                )
                .await
                .unwrap();
            let user_msg = Message {
                attachments: None,
                role: MessageRole::User,
                content: "选个数据库".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            };
            let assistant_msg = Message {
                attachments: None,
                role: MessageRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                name: None,
                tool_calls: Some(vec![openslate_core::types::ToolCall {
                    id: openslate_core::types::ToolCallId(tool_call_id.into()),
                    name: "ask_user".into(),
                    arguments: serde_json::json!({
                        "questions": [{
                            "id": "db",
                            "question": "数据库？",
                            "type": "choice",
                            "options": [{"label": "SQLite"}, {"label": "sled"}]
                        }]
                    }),
                }]),
                reasoning_content: None,
            };
            store
                .insert_message(
                    &format!("msg-{run_id}-1"),
                    run_id,
                    &format!("{run_id}-persist"),
                    Some("root"),
                    "user",
                    &openslate_store_sqlite::codec::encode_content(
                        &serde_json::to_string(&user_msg).unwrap(),
                    ),
                    None,
                    1,
                    now_ms - 59_000,
                )
                .await
                .unwrap();
            store
                .insert_message(
                    &format!("msg-{run_id}-2"),
                    run_id,
                    &format!("{run_id}-persist"),
                    Some("root"),
                    "assistant",
                    &openslate_store_sqlite::codec::encode_content(
                        &serde_json::to_string(&assistant_msg).unwrap(),
                    ),
                    None,
                    2,
                    now_ms - 58_000,
                )
                .await
                .unwrap();
            if with_row {
                store
                    .insert_pending_question(
                        "77",
                        tool_call_id,
                        r#"[{"id":"db","question":"数据库？","type":"choice","options":[{"label":"SQLite"},{"label":"sled"}],"optional":false}]"#,
                        1,
                        now_ms - 58_000,
                    )
                    .await
                    .unwrap();
            }
        });
    }

    fn transcript_has_tool_call(snap: &serde_json::Value, call_id: &str) -> bool {
        snap["session"]["transcript"]
            .as_array()
            .map(|ts| {
                ts.iter().any(|e| {
                    e["kind"] == "tool_call"
                        && e["call_id"].as_str() == Some(call_id)
                })
            })
            .unwrap_or(false)
    }

    /// 本 run 尾部悬挂被 pending 行引用 → 重启恢复不得 pop（补全点
    /// 保留，迟到答案 / TTL 过期收尾依赖它）。
    #[test]
    fn restart_keeps_dangling_tail_when_pending_row_references_it() {
        let (paths, _dir) = temp_paths();
        paths.ensure_layout().unwrap();
        seed_run_with_dangling_tail(&paths, "pop-guard-keep", "call-guard-keep", true);

        let (collector, rx) = Collector::new();
        let runtime = MobileRuntime::create_with(
            paths.clone(),
            collector.clone(),
            scripted_opts(vec![text_response("收到。")]),
        )
        .unwrap();
        let events = collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
        let snap = events
            .iter()
            .rev()
            .find(|e| e["type"] == "snapshot")
            .expect("首份快照");
        assert!(
            transcript_has_tool_call(snap, "call-guard-keep"),
            "pending 行引用尾部 tool_call → 悬挂 assistant 不被 pop: {snap}"
        );
        runtime.shutdown();
    }

    /// 无 pending 引用 → 经典丢弃语义照常（尾部悬挂 assistant 被 pop，
    /// 模型重新决策）。
    #[test]
    fn restart_pops_dangling_tail_without_pending_reference() {
        let (paths, _dir) = temp_paths();
        paths.ensure_layout().unwrap();
        seed_run_with_dangling_tail(&paths, "pop-guard-drop", "call-guard-drop", false);

        let (collector, rx) = Collector::new();
        let runtime = MobileRuntime::create_with(
            paths.clone(),
            collector.clone(),
            scripted_opts(vec![text_response("收到。")]),
        )
        .unwrap();
        let events = collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
        let snap = events
            .iter()
            .rev()
            .find(|e| e["type"] == "snapshot")
            .expect("首份快照");
        assert!(
            !transcript_has_tool_call(snap, "call-guard-drop"),
            "无 pending 引用 → 尾部悬挂 assistant 照常 pop（经典语义）: {snap}"
        );
        runtime.shutdown();
    }

    // ── read_file 读图链路 + 附件缩略图 FFI（2026-10-21）─────────────

    /// 端到端（core 侧链路，mobile 进程内验证）：builtin read_file 读图
    /// → fs.rs 嗅探/归一化 → Image content block → core downgrade 收集
    /// → ToolOutput.attachments → Tool Message 组装（runtime 同款）→
    /// 持久化仅引用形状（data serde-skip）。
    #[test]
    fn read_file_image_flows_to_tool_message_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let (_, png) = write_tiny_png(dir.path());
        std::fs::write(dir.path().join("cam.png"), &png).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (tools, _services) = openslate_core::mcp::connect_builtin_servers(
                dir.path(),
                &openslate_core::config::BuiltinToolsConfig::default(),
                Vec::new(),
            )
            .await
            .unwrap();
            let read = tools
                .iter()
                .find(|t| t.name() == "read_file")
                .expect("read_file 注册");

            let out = read
                .execute(&serde_json::json!({"path": "cam.png"}))
                .await
                .unwrap();
            assert_eq!(out.status, openslate_core::types::ToolOutputStatus::Success);
            assert!(
                out.content.contains("[image file: cam.png"),
                "说明文本进 content：{}",
                out.content
            );
            let atts = out.attachments.as_ref().expect("Image block → 附件收集");
            assert_eq!(atts.len(), 1);
            assert_eq!(atts[0].id, "tool-0");
            assert_eq!(atts[0].mime, "image/png", "小 PNG passthrough");
            assert_eq!(
                atts[0].data.as_deref().unwrap(),
                &png[..] as &[u8],
                "字节 = 原图"
            );

            // runtime 同款 Tool Message 组装（runtime.rs 顺序/并行两路径）。
            let msg = Message {
                attachments: out.attachments,
                role: MessageRole::Tool,
                content: out.content,
                tool_call_id: Some(openslate_core::types::ToolCallId::from("call-1")),
                name: Some("read_file".into()),
                tool_calls: None,
                reasoning_content: None,
            };
            let value = serde_json::to_value(&msg).unwrap();
            let att = &value["attachments"][0];
            assert_eq!(att["id"].as_str(), Some("tool-0"), "{value}");
            assert!(att.get("data").is_none(), "data serde-skip：{value}");
        });
    }

    /// 缩略图 FFI 往返：import 一张 3000×2000 大图 → 归一化落库 →
    /// get_attachment_thumbnail 出 ≤128 长边 JPEG（base64 可解、尺寸
    /// 正确、mime 标 image/jpeg）；错误路径（未知 id / 视频附件）走
    /// `{"ok":false,"error"}`。
    #[test]
    fn attachment_thumbnail_ffi_roundtrip() {
        let (paths, _dir) = temp_paths();
        let (collector, rx) = Collector::new();
        let runtime =
            MobileRuntime::create_with(paths.clone(), collector.clone(), scripted_opts(vec![]))
                .unwrap();
        let _ = collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

        // 3000×2000 JPEG（归一化入口 → 落库 ≤2048）。
        let big = dir_join_jpeg(_dir.path(), "big.jpg");
        let env: serde_json::Value = serde_json::from_str(
            &runtime.import_attachment_json(big.to_str().unwrap(), "image/jpeg", "big.jpg"),
        )
        .unwrap();
        assert!(env["ok"].as_bool().unwrap(), "{env}");
        assert_eq!(env["mime"].as_str().unwrap(), "image/jpeg");
        let img_id = env["id"].as_str().unwrap().to_owned();

        // 视频附件（缩略图不支持 → error）。
        let mp4 = write_fake_mp4(_dir.path());
        let venv: serde_json::Value = serde_json::from_str(
            &runtime.import_attachment_json(mp4.to_str().unwrap(), "video/mp4", "clip.mp4"),
        )
        .unwrap();
        assert!(venv["ok"].as_bool().unwrap(), "{venv}");
        let vid_id = venv["id"].as_str().unwrap().to_owned();

        // 图像 → ok + 可解码 JPEG + 长边 128（只缩不放）。
        let out: serde_json::Value =
            serde_json::from_str(&runtime.get_attachment_thumbnail_json(&img_id, 128)).unwrap();
        assert!(out["ok"].as_bool().unwrap(), "{out}");
        assert_eq!(out["mime"].as_str().unwrap(), "image/jpeg");
        let b64 = out["data"].as_str().unwrap();
        let bytes = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .expect("合法 base64")
        };
        let thumb = image::load_from_memory(&bytes).expect("JPEG 可解码");
        assert_eq!(thumb.width().max(thumb.height()), 128);

        // max_px<=0 → 256 兜底。
        let out: serde_json::Value =
            serde_json::from_str(&runtime.get_attachment_thumbnail_json(&img_id, 0)).unwrap();
        assert!(out["ok"].as_bool().unwrap(), "{out}");
        let bytes = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(out["data"].as_str().unwrap())
                .unwrap()
        };
        let thumb = image::load_from_memory(&bytes).unwrap();
        assert_eq!(thumb.width().max(thumb.height()), 256);

        // 错误路径：未知 id / 视频附件。
        let miss: serde_json::Value = serde_json::from_str(
            &runtime.get_attachment_thumbnail_json("att-nope", 128),
        )
        .unwrap();
        assert!(!miss["ok"].as_bool().unwrap(), "{miss}");
        assert!(miss["error"].as_str().unwrap().contains("不存在"), "{miss}");
        let vid: serde_json::Value =
            serde_json::from_str(&runtime.get_attachment_thumbnail_json(&vid_id, 128)).unwrap();
        assert!(!vid["ok"].as_bool().unwrap(), "{vid}");
        assert!(vid["error"].as_str().unwrap().contains("图像"), "{vid}");

        runtime.shutdown();
    }

    /// 生成一张 3000×2000 的 JPEG 测试图（缩略图/归一化入口用）。
    fn dir_join_jpeg(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let img = image::RgbImage::from_pixel(3000, 2000, image::Rgb([180, 40, 40]));
        let mut buf = Vec::new();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 95);
        image::DynamicImage::ImageRgb8(img)
            .write_with_encoder(enc)
            .unwrap();
        let path = dir.join(name);
        std::fs::write(&path, &buf).unwrap();
        path
    }

    // ── 附件导入（import JSON 契约：无网络、无 tokens 键）────────────

    /// 导入是纯本地操作：成功 JSON 恒无 `tokens` 键（发起任何 API 请求
    /// 之前绝不把附件数据发到任何服务器——token 数由回合结束后响应侧
    /// 回填，见 `attachment_tokens` 模块）；对比信息键（快速通道态）就位。
    #[test]
    fn import_json_never_estimates_tokens() {
        let (paths, _dir) = temp_paths();
        let (collector, _rx) = Collector::new();
        let runtime =
            MobileRuntime::create_with(paths.clone(), collector, scripted_opts(vec![])).unwrap();
        let (png, _) = write_tiny_png(_dir.path());
        let env: serde_json::Value = serde_json::from_str(
            &runtime.import_attachment_json(png.to_str().unwrap(), "image/png", "s.png"),
        )
        .unwrap();
        assert!(env["ok"].as_bool().unwrap(), "{env}");
        assert!(env.get("tokens").is_none(), "导入侧绝不预估 token：{env}");
        assert!(env["size_bytes"].as_u64().unwrap() > 0, "{env}");
        // 对比信息（48×32 小 PNG → 快速通道）。
        assert_eq!(env["processed"].as_str(), Some("passthrough"), "{env}");
        assert_eq!(env["orig_width"].as_u64(), Some(48), "{env}");
        assert_eq!(env["orig_height"].as_u64(), Some(32), "{env}");
        assert_eq!(env["width"].as_u64(), Some(48), "{env}");
        assert_eq!(env["height"].as_u64(), Some(32), "{env}");
        assert_eq!(
            env["orig_bytes"].as_u64(),
            env["size_bytes"].as_u64(),
            "passthrough 字节相等：{env}"
        );
        runtime.shutdown();
    }

    /// 成功 JSON 的对比信息三态：转码（超尺寸 JPEG → 缩放）与视频
    /// passthrough（尺寸键省略）。
    #[test]
    fn import_json_reports_transcode_and_video_states() {
        let (paths, _dir) = temp_paths();
        let (collector, _rx) = Collector::new();
        let runtime =
            MobileRuntime::create_with(paths.clone(), collector, scripted_opts(vec![])).unwrap();

        // 3000×2000 JPEG → transcode：orig=3000×2000、处理后 2048×1365。
        let img = image::RgbImage::from_pixel(3000, 2000, image::Rgb([30, 90, 200]));
        let mut jpeg = Vec::new();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90);
        image::DynamicImage::ImageRgb8(img)
            .write_with_encoder(enc)
            .unwrap();
        let src = _dir.path().join("big.jpg");
        std::fs::write(&src, &jpeg).unwrap();
        let env: serde_json::Value = serde_json::from_str(
            &runtime.import_attachment_json(src.to_str().unwrap(), "image/jpeg", "big.jpg"),
        )
        .unwrap();
        assert!(env["ok"].as_bool().unwrap(), "{env}");
        assert_eq!(env["processed"].as_str(), Some("transcode"), "{env}");
        assert_eq!(env["orig_width"].as_u64(), Some(3000), "{env}");
        assert_eq!(env["orig_height"].as_u64(), Some(2000), "{env}");
        assert_eq!(env["width"].as_u64(), Some(2048), "{env}");
        assert_eq!(env["height"].as_u64(), Some(1365), "{env}");
        assert_eq!(env["orig_bytes"].as_u64(), Some(jpeg.len() as u64), "{env}");
        assert!(env["size_bytes"].as_u64().unwrap() < jpeg.len() as u64, "{env}");

        // MP4 → video_passthrough：尺寸四键全部省略。
        let mp4 = write_fake_mp4(_dir.path());
        let env: serde_json::Value = serde_json::from_str(
            &runtime.import_attachment_json(mp4.to_str().unwrap(), "video/mp4", "clip.mp4"),
        )
        .unwrap();
        assert!(env["ok"].as_bool().unwrap(), "{env}");
        assert_eq!(env["processed"].as_str(), Some("video_passthrough"), "{env}");
        for key in ["orig_width", "orig_height", "width", "height"] {
            assert!(env.get(key).is_none(), "视频省略尺寸键 {key}：{env}");
        }
        assert_eq!(
            env["orig_bytes"].as_u64(),
            env["size_bytes"].as_u64(),
            "视频 passthrough 字节相等：{env}"
        );
        runtime.shutdown();
    }

    // ── 附件 image tokens 响应侧回填（协议 v5）──────────────────────

    /// 带实测 image tokens 的流脚本：Usage 事件（tap 闩值来源）与 Done
    /// 的 ModelResponse.usage（meta_json 持久化来源）同值——真实 genai
    /// 流路径两处一致。
    fn image_usage_response(text: &str, image_tokens: u64) -> Vec<ModelStreamEvent> {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 5,
            cached_input_tokens: None,
            reasoning_tokens: None,
            image_tokens: Some(image_tokens),
        };
        vec![
            ModelStreamEvent::Delta(text.to_owned()),
            ModelStreamEvent::Usage(usage),
            ModelStreamEvent::Done(ModelResponse {
                content: Some(text.to_owned()),
                tool_calls: vec![],
                reasoning_content: None,
                usage: Some(usage),
                finish_reason: Some("stop".into()),
            }),
        ]
    }

    /// 无分模态计数的流脚本（智谱口径：image_tokens None）。
    fn no_image_usage_response(text: &str) -> Vec<ModelStreamEvent> {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 5,
            cached_input_tokens: None,
            reasoning_tokens: None,
            image_tokens: None,
        };
        vec![
            ModelStreamEvent::Delta(text.to_owned()),
            ModelStreamEvent::Usage(usage),
            ModelStreamEvent::Done(ModelResponse {
                content: Some(text.to_owned()),
                tool_calls: vec![],
                reasoning_content: None,
                usage: Some(usage),
                finish_reason: Some("stop".into()),
            }),
        ]
    }

    /// 轮询等待附件回填值落 map（sync_loop 消费 completed 队列 ≤1s）。
    fn wait_token_backfill(
        runtime: &MobileRuntime,
        att_id: &str,
        deadline: Duration,
    ) -> Option<u64> {
        let start = Instant::now();
        loop {
            if let Some(t) = runtime.attachment_tokens.get(att_id) {
                return Some(t);
            }
            if start.elapsed() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// 从最近一条 snapshot 事件里取 User 条目的附件数组（按 att id 命中）。
    fn snapshot_user_attachment<'a>(
        collector: &Collector,
        att_id: &str,
    ) -> Option<serde_json::Value> {
        let all = collector.events.lock().unwrap().clone();
        let snapshot = all
            .iter()
            .rev()
            .find(|e| e.get("type").and_then(|t| t.as_str()) == Some("snapshot"))?;
        snapshot
            .pointer("/session/transcript")?
            .as_array()?
            .iter()
            .filter_map(|e| {
                let atts = e.get("attachments")?.as_array()?;
                atts.iter()
                    .find(|a| a.get("id").and_then(|i| i.as_str()) == Some(att_id))
                    .cloned()
            })
            .next_back()
    }

    /// 单图回合：首次调用 image_tokens 经 tap 闩值、sync_loop 落
    /// transcript，快照 User 附件携带 `tokens`；同回合第二条 Usage
    /// （agentic 后续调用）忽略。
    #[test]
    fn single_image_turn_backfills_tokens_into_snapshot() {
        let (paths, _dir) = temp_paths();
        let (collector, rx) = Collector::new();
        let mut script = image_usage_response("看到了", 728);
        // 同回合第二次调用（历史图重发）：值更大但必须被忽略。
        script.extend(image_usage_response("再确认", 9999));
        let runtime =
            MobileRuntime::create_with(paths.clone(), collector.clone(), scripted_opts(vec![script]))
                .unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

        let (png_path, _bytes) = write_tiny_png(_dir.path());
        let env: serde_json::Value = serde_json::from_str(
            &runtime.import_attachment_json(png_path.to_str().unwrap(), "image/png", "s.png"),
        )
        .unwrap();
        let att_id = env["id"].as_str().unwrap().to_owned();
        runtime
            .send(&format!(
                r#"{{"type":"submit","text":"看图","attachments":[{{"id":"{att_id}","mime":"image/png"}}]}}"#
            ))
            .unwrap();
        collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));

        // sync_loop 消费 completed（空闲周期 ≤1s）→ map + transcript 就位。
        let t = wait_token_backfill(&runtime, &att_id, Duration::from_secs(5));
        assert_eq!(t, Some(728), "首次调用值（9999 是后续调用，被忽略）");

        // 重发 Hello → 新快照的 User 附件带 tokens。
        runtime.send(r#"{"type":"hello","proto":5}"#).unwrap();
        collector.wait_for_count(&rx, "snapshot", 2, Duration::from_secs(10));
        let att = snapshot_user_attachment(&collector, &att_id)
            .expect("快照 User 条目应带该附件");
        assert_eq!(att["tokens"].as_u64(), Some(728), "{att}");
        runtime.shutdown();
    }

    /// 多图不拆分：两图 Submit 不设标记，usage 有 image_tokens 也不回填。
    #[test]
    fn multi_image_submit_never_backfills_tokens() {
        let (paths, _dir) = temp_paths();
        let (collector, rx) = Collector::new();
        let runtime = MobileRuntime::create_with(
            paths.clone(),
            collector.clone(),
            scripted_opts(vec![image_usage_response("两张都收到了", 500)]),
        )
        .unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));

        let mut ids = Vec::new();
        for i in 0..2 {
            let (png_path, _) = write_tiny_png(_dir.path());
            let env: serde_json::Value = serde_json::from_str(
                &runtime.import_attachment_json(
                    png_path.to_str().unwrap(),
                    "image/png",
                    &format!("s{i}.png"),
                ),
            )
            .unwrap();
            ids.push(env["id"].as_str().unwrap().to_owned());
        }
        let atts_json = ids
            .iter()
            .map(|id| format!(r#"{{"id":"{id}","mime":"image/png"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        runtime
            .send(&format!(
                r#"{{"type":"submit","text":"两张图","attachments":[{atts_json}]}}"#
            ))
            .unwrap();
        collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));
        // 覆盖一个 sync 周期后仍无回填。
        std::thread::sleep(Duration::from_millis(1300));
        for id in &ids {
            assert_eq!(
                runtime.attachment_tokens.get(id),
                None,
                "多图 usage 不分图，不回填（{id}）"
            );
        }
        runtime.send(r#"{"type":"hello","proto":5}"#).unwrap();
        collector.wait_for_count(&rx, "snapshot", 2, Duration::from_secs(10));
        for id in &ids {
            let att =
                snapshot_user_attachment(&collector, id).expect("快照 User 条目应带该附件");
            assert!(
                att.get("tokens").is_none(),
                "多图条目无 tokens 键：{att}"
            );
        }
        runtime.shutdown();
    }

    /// 智谱口径：usage 不分模态（image_tokens None）→ 无回填，UI 回退
    /// 文件大小。
    #[test]
    fn no_modality_usage_means_no_backfill() {
        let (paths, _dir) = temp_paths();
        let (collector, rx) = Collector::new();
        let runtime = MobileRuntime::create_with(
            paths.clone(),
            collector.clone(),
            scripted_opts(vec![no_image_usage_response("好的")]),
        )
        .unwrap();
        collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
        let (png_path, _) = write_tiny_png(_dir.path());
        let env: serde_json::Value = serde_json::from_str(
            &runtime.import_attachment_json(png_path.to_str().unwrap(), "image/png", "s.png"),
        )
        .unwrap();
        let att_id = env["id"].as_str().unwrap().to_owned();
        runtime
            .send(&format!(
                r#"{{"type":"submit","text":"看图","attachments":[{{"id":"{att_id}","mime":"image/png"}}]}}"#
            ))
            .unwrap();
        collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(1300));
        assert_eq!(runtime.attachment_tokens.get(&att_id), None);
        runtime.shutdown();
    }

    /// 重启可见性：transcript_entries 行在回填前已落库（追加式不可变），
    /// 重开经 messages.meta_json 重放同口径回填 → 首份快照即带 tokens。
    #[test]
    fn attachment_tokens_survive_restart_via_meta_replay() {
        let (paths, _dir) = temp_paths();
        {
            let (collector, rx) = Collector::new();
            let runtime = MobileRuntime::create_with(
                paths.clone(),
                collector.clone(),
                scripted_opts(vec![image_usage_response("是截图", 728)]),
            )
            .unwrap();
            collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
            let (png_path, _) = write_tiny_png(_dir.path());
            let env: serde_json::Value = serde_json::from_str(
                &runtime.import_attachment_json(png_path.to_str().unwrap(), "image/png", "s.png"),
            )
            .unwrap();
            let att_id = env["id"].as_str().unwrap().to_owned();
            runtime
                .send(&format!(
                    r#"{{"type":"submit","text":"看图","attachments":[{{"id":"{att_id}","mime":"image/png"}}]}}"#
                ))
                .unwrap();
            collector.wait_for(&rx, "turn_ok", Duration::from_secs(20));
            // 等 sync_loop 把 User 条目落库 + 回填消费完（避免崩溃窗口
            // 尾部与测试竞速）。
            assert_eq!(
                wait_token_backfill(&runtime, &att_id, Duration::from_secs(5)),
                Some(728)
            );
            std::thread::sleep(Duration::from_millis(400));
            runtime.shutdown();
        }
        // 重启：首份 snapshot（rebuild_transcript → meta 重放）即带 tokens。
        {
            let (collector, rx) = Collector::new();
            let runtime = MobileRuntime::create_with(
                paths.clone(),
                collector.clone(),
                scripted_opts(vec![no_image_usage_response("继续")]),
            )
            .unwrap();
            let events = collector.wait_for(&rx, "snapshot", Duration::from_secs(10));
            let snapshot = events
                .iter()
                .rev()
                .find(|e| e.get("type").and_then(|t| t.as_str()) == Some("snapshot"))
                .expect("snapshot");
            let users_with_tokens: Vec<&serde_json::Value> = snapshot
                .pointer("/session/transcript")
                .and_then(|t| t.as_array())
                .map(|a| {
                    a.iter()
                        .filter(|e| {
                            e.get("attachments")
                                .and_then(|x| x.as_array())
                                .is_some_and(|atts| {
                                    atts.iter().any(|x| x.get("tokens").is_some())
                                })
                        })
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(
                users_with_tokens.len(),
                1,
                "重启后恰好一条 User 附件带 tokens：{snapshot}"
            );
            let att = &users_with_tokens[0]["attachments"][0];
            assert_eq!(att["tokens"].as_u64(), Some(728), "{att}");
            runtime.shutdown();
        }
    }
}
