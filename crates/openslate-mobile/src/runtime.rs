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
use openslate_protocol::{ClientMsg, EntryDto, ServerMsg, ToolStatusDto, PROTOCOL_VERSION};
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
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            provider_factory: None,
            extra_tools: Vec::new(),
            host_call_timeout: HOST_CALL_TIMEOUT,
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

    // ── 历史会话（列表 / 切换）──────────────────────────────────

    /// 历史会话列表（JSON 数组，最近 50 条）：
    /// [{id,title,status,started_ms,cost_usd}]。title = 首条用户消息摘要。
    pub fn list_sessions_json(&self) -> String {
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
            let Ok(runs) = store.list_runs(50, 0).await else {
                return "[]".to_owned();
            };
            let mut arr = Vec::with_capacity(runs.len());
            for r in runs {
                // 首条 user 消息摘要作标题（content_json 是 Message JSON）。
                let title = store
                    .list_messages_by_run(&r.id)
                    .await
                    .ok()
                    .and_then(|ms| {
                        ms.iter()
                            .find(|m| m.role == "user")
                            .and_then(|m| serde_json::from_str::<serde_json::Value>(&m.content_json).ok())
                            .and_then(|v| v.get("content").and_then(|c| c.as_str()).map(|s| s.to_owned()))
                    })
                    .map(|s: String| s.chars().take(40).collect())
                    .unwrap_or_else(|| r.title.clone().unwrap_or_else(|| "(空会话)".into()));
                arr.push(serde_json::json!({
                    "id": r.id,
                    "title": title,
                    "status": r.status,
                    "started_ms": r.started_at,
                    "cost_usd": r.cost_usd,
                }));
            }
            serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_owned())
        })
    }

    /// 切换到指定历史会话：加载消息 → 续用该 run → 广播新 snapshot。
    pub fn open_session(&self, run_id: String) -> Result<()> {
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
            let root_agent_id = state.core.lock().agent_tree.get_root().id.0.clone();
            let recorder = openslate_store_sqlite::recorder::RunRecorder::resume(
                store,
                openslate_core::types::RunId(run.id.clone()),
                &root_agent_id,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
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
/// 注入 factory）。
async fn assemble_state(
    config_path: std::path::PathBuf,
    sink: Arc<EventSink>,
    provider_factory: ProviderFactory,
    extra_tools: Vec<Box<dyn Tool>>,
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

    let root_alias = agent_tree.get_root().model_alias.clone();
    let session_label = config
        .project
        .as_ref()
        .and_then(|p| p.name.clone())
        .unwrap_or_else(|| "OpenSlate Mobile".to_owned());
    let root_agent_id = agent_tree.get_root().id.0.clone();

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
        model_alias: root_alias,
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
