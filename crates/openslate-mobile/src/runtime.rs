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
use openslate_core::approval::ApprovalManager;
use openslate_core::error::ToolError;
use openslate_core::mcp::McpConnectionGuard;
use openslate_core::tool::Tool;
use openslate_core::types::{Message, MessageRole, ToolOutput};
use openslate_protocol::{
    ClientMsg, EntryDto, NoticeLevel, ServerMsg, ToolStatusDto, PROTOCOL_VERSION,
};
use openslate_session::approval::SessionApprovalBridge;
use openslate_session::session as session_dispatch;
use openslate_session::state::{
    build_snapshot, AppState, ConfigPaths, CoreInner, MsgSink, ProviderFactory, SessionCore,
};

use crate::askuser::{
    AskUserContext, AskUserExemptApproval, AskUserTool, MobileQuestionHandler,
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
    shutdown_flag: std::sync::atomic::AtomicBool,
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
        // 提问）+ bash 工具（按设置多选动态注册：单选 → bash；双选 →
        // bash=native + termux_bash）+ 注入项。
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

        // 外部 MCP 连接 guard + 工具名册随装配交出（热重载整体替换；
        // builtin 句柄已随 AppState._mcp 保活）。
        let (state, mcp_external, mcp_tool_names) = rt.block_on(assemble_state(
            config_path.clone(),
            sink.clone(),
            factory,
            extra_tools,
            opts.run_retention,
        ))?;
        crate::alog!("create_with: state assembled, wiring approval + ask_user");
        // ask_user 接线：上下文注入 store/配置 + QuestionHandler 装配
        // （dispatch 的 question_reply / cancel 由此进 mobile 实现）。
        ask_ctx.wire(&state);
        state
            .core
            .set_question_handler(Arc::new(MobileQuestionHandler::new(ask_ctx.clone())));
        // vault 持久层接线：加载 credentials 镜像（remember 凭据跨会话
        // 可用——env 注入/lookup/脱敏值源全走镜像）。
        if let Some(store) = state.core.lock().store.clone() {
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
                        message: format!("协议版本不匹配：宿主 {proto}，运行时 {PROTOCOL_VERSION}"),
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
        //    与 builtin/host 等非 MCP 工具冲突——装配期同样致命）。
        let mut registered: Vec<String> = Vec::with_capacity(new_names.len());
        for name in &new_names {
            let Some(tool) = scratch.get(name) else {
                continue;
            };
            if let Err(e) = self.registry.try_register(SharedTool(tool)) {
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
        //    兜底回装——bash 能力在 reload 前后始终有人在场，不因重载
        //    出现空窗。运行期 host 死亡不做热切换（工具列表已进上下文，
        //    McpTool 失败语义由调用方感知），接管/兜底的交接只在 reload
        //    边界发生。
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

    // ── 工具可见性（工具管理页）──────────────────────────────────────

    /// 全部已注册工具清单（JSON 数组，按 name 排序；宿主渲染工具管理页，
    /// 展开项展示详细参数）：
    /// `[{"name":"bash","description":"…","input_schema":{…}}, …]`——
    /// `input_schema` 为该工具参数的 JSON Schema 对象原样（无 schema 时
    /// 为 `{}`，键恒存在）。
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
                serde_json::json!({
                    "name": d.name,
                    "description": d.description,
                    "input_schema": input_schema,
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
/// 返回 `(AppState, 外部 MCP guard, 外部 MCP 工具名)`——后两者由
/// `MobileRuntime` 接手（热重载整体替换），builtin 句柄留在
/// `AppState._mcp` 保活。
async fn assemble_state(
    config_path: std::path::PathBuf,
    sink: Arc<EventSink>,
    provider_factory: ProviderFactory,
    extra_tools: Vec<Box<dyn Tool>>,
    run_retention: i64,
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
    let policy = manager.approval.policy().clone();
    let approval_bridge = Arc::new(SessionApprovalBridge::new(sink.clone()));
    // ask_user 审批豁免（设计 §0）：Manual 全审模式下提问不触发审批
    // （「问个问题还要先批准才能问」是体验灾难）；respond 通道照旧走
    // 原 bridge（AppState.approval 不动）。
    manager.approval = ApprovalManager::new(policy)
        .with_callback(Arc::new(AskUserExemptApproval::new(approval_bridge.clone())));

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
                        // load_messages 的经典丢弃（模型重新决策）。
                        if !keep_dangling
                            && matches!(
                                restored_history.last(),
                                Some(m) if m.role == MessageRole::Assistant
                                    && m.tool_calls.as_ref().is_some_and(|tcs| !tcs.is_empty())
                            )
                        {
                            restored_history.pop();
                        }
                        restored_transcript = messages_to_transcript(&restored_history);
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

    Ok((
        Arc::new(AppState {
            core: Arc::new(SessionCore::new(inner)),
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
                if let Some(EntryDto::ToolCall { detail, .. }) =
                    out.iter_mut().rev().find_map(|e| {
                        if let EntryDto::ToolCall { name, call_id, .. } = e {
                            if call_id.as_deref() == m.tool_call_id.as_ref().map(|x| x.0.as_str())
                                || (m.tool_call_id.is_none()
                                    && name == m.name.as_deref().unwrap_or(""))
                            {
                                return Some(e);
                            }
                            None
                        } else {
                            None
                        }
                    })
                {
                    detail.output = Some(m.content.chars().take(2000).collect::<String>());
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
        // 起始无 MCP：HostTool 兜底占位 termux_bash（双选命名）。
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
        assert!(runtime.registry.contains("bash"), "HostTool 兜底（顶替名）");

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
            runtime.registry.contains("bash"),
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
            .get("bash")
            .expect("HostTool 兜底（顶替名）");
        let ping_before = runtime.registry.get("mobile.ping").expect("extra ping");

        // 模板 `{tool}`：裸名原样注册 → bash（让位）+ mobile.ping（与
        // extra_tools 演示工具真冲突 → 回滚）。
        let url = spawn_mock_mcp_host(&["bash", "mobile.ping"]);
        append_mcp_config(&paths, &url, Some("{tool}"));
        let env = envelope(&runtime.reload_mcp());
        assert!(!env["ok"].as_bool().unwrap(), "应因真冲突失败: {env}");
        assert!(
            env["error"].as_str().unwrap().contains("撞名"),
            "错误文案: {env}"
        );

        // （host_tool 变量保用于断言兜底语义：装回的 bash 必须仍是
        // HostTool 而非 mock 工具，见下方 description 断言。）
        let _ = host_tool;
        // 回滚后：让位的 HostTool 装回（SharedTool 包同一 Arc，外层 Arc
        // 必然新——以描述语义判定是 HostTool 而非 mock），ping 未被顶替，
        // MCP 名册仍为空。
        let restored = runtime.registry.get("bash").expect("回滚后兜底装回");
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

    /// termux-only 命名矩阵：仅选 termux 后端 + MCP 提供裸 bash →
    /// HostTool 顶替 `bash`，MCP 注册名 = builtin 模板展开（默认
    /// termux_bash；自定义 `{prefix}.bash` → termux.bash，证明不硬编码）。
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
        assert!(names.contains(&"bash".to_owned()), "{names:?}");
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
        assert!(names.contains(&"bash".to_owned()), "{names:?}");
        assert!(
            names.contains(&"termux.bash".to_owned()),
            "MCP 注册名 = 模板展开: {names:?}"
        );
        assert!(
            !names.contains(&"termux_bash".to_owned()),
            "不得硬编码 termux_bash: {names:?}"
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
                    role: MessageRole::User,
                    content: "选个数据库".into(),
                    tool_call_id: None,
                    name: None,
                    tool_calls: None,
                    reasoning_content: None,
                };
                let assistant_msg = Message {
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
                        &serde_json::to_string(&user_msg).unwrap(),
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
                        &serde_json::to_string(&assistant_msg).unwrap(),
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
}
