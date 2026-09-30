//! openslate-server（web-1）：共享单会话 HTTP+WS 后端。
//!
//! - REST 只读：`GET /api/{health,config,agents,skills,sessions}`
//!   （`--auth-token` 时 `?token=` 鉴权）。
//! - WS 全功能单通道：`GET /api/ws`（hello → snapshot → 事件流，写操作
//!   全走这里）。
//! - 引擎：`RunManager` + server 版审批桥 + pre-turn auto-compact。
//! - 日志：`~/.local/share/openslate/logs/server.log`（与 tui.log 同
//!   规格，承载子 agent 委派链 tracing）。
//!
//! 组合根见 [`serve`]；测试可直接用 [`build_router`] 起真 listener。

pub mod approval;
pub mod config_ops;
pub mod discovery;
pub mod engine;
pub mod rest;
pub mod state;
pub mod ws;

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::routing::get;
use axum::Router;
use openslate_app::wiring::{apply_approval, build_app_context};
use openslate_core::approval::ApprovalManager;
use openslate_protocol::{ServerInfo, SkillInfoDto, PROTOCOL_VERSION};
use state::{AppState, ConfigPaths, ConnectionHub, CoreInner, ProviderFactory, SessionCore};

/// stdout 运行时事件的专用 target：stdout fmt layer 只放行它（见
/// [`init_logging`]），第三方 crate 的 info 日志不会刷到前台。
pub const STDOUT_TARGET: &str = "serve_stdout";

/// 单行 stdout 事件（英文文案、grep 友好；同时经文件 layer 落
/// server.log）。**token / api key 值绝不进这里。**
macro_rules! stdout_event {
    ($($arg:tt)*) => {
        tracing::info!(target: crate::STDOUT_TARGET, $($arg)*)
    };
}
pub(crate) use stdout_event;

/// serve 选项（cli `serve` 子命令转发）。
pub struct ServeOptions {
    pub bind: IpAddr,
    pub port: u16,
    pub auth_token: Option<String>,
    /// CLI `--config` 直通（None = 发现链 global→local merge）。
    pub config_flag: Option<String>,
    /// 测试接缝：None = 生产 provider 工厂。
    pub provider_factory: Option<ProviderFactory>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            bind: IpAddr::from([127, 0, 0, 1]),
            port: 7800,
            auth_token: None,
            config_flag: None,
            provider_factory: None,
        }
    }
}

/// 装配 AppState（build_app_context 一条龙 + 审批桥 + 会话核心）。
/// 集成测试也走这里（注入 scripted provider_factory 与临时 config）。
pub async fn build_state(opts: &ServeOptions) -> Result<Arc<AppState>> {
    let ctx = build_app_context(opts.config_flag.as_deref())
        .await
        .context("server 启动装配失败")?;

    let openslate_app::wiring::AppContext {
        config,
        agents,
        store,
        agent_tree,
        mut manager,
        skills,
        mcp_connections,
        config_path,
        global_config_path,
        agents_path: _,
    } = ctx;

    let hub = Arc::new(ConnectionHub::new());
    let approval_bridge = Arc::new(approval::ServerApprovalBridge::new(hub.clone()));

    // server = 交互式语义：审批策略按 interactive 推导，回调挂桥。
    apply_approval(&mut manager, &config, true, false);
    let policy = manager.approval.policy().clone();
    manager.approval = ApprovalManager::new(policy).with_callback(approval_bridge.clone());

    let root_alias = agent_tree.get_root().model_alias.clone();
    let session_label = config
        .project
        .as_ref()
        .and_then(|p| p.name.clone())
        .unwrap_or_else(|| "openslate 会话".to_owned());

    // local 路径：发现链里 active==global 说明无本地叠加 → None；
    // 其余（含 --config 显式）local 与 active 同文件。
    let local = if global_config_path
        .as_deref()
        .map(|g| g == config_path.as_path())
        .unwrap_or(false)
    {
        None
    } else {
        Some(config_path.clone())
    };
    let paths = ConfigPaths {
        active: config_path,
        global: global_config_path,
        local,
    };

    let inner = CoreInner {
        session_id: new_session_id(),
        session_label,
        history: Vec::new(),
        transcript: Vec::new(),
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
            .map(|s| SkillInfoDto {
                name: s.name.clone(),
                description: s.description.clone(),
            })
            .collect(),
        store,
        session_run: None,
        manager: Some(manager),
        engine_task: None,
        paths,
        stats: Default::default(),
        stream: Default::default(),
        tool_timers: Default::default(),
        pending_step_meta: None,
    };

    Ok(Arc::new(AppState {
        core: Arc::new(SessionCore::new(inner)),
        hub,
        approval: approval_bridge,
        auth_token: opts.auth_token.clone(),
        provider_factory: opts
            .provider_factory
            .clone()
            .unwrap_or_else(default_provider_factory),
        _mcp: mcp_connections,
    }))
}

fn default_provider_factory() -> ProviderFactory {
    Arc::new(openslate_app::build_provider_for_model)
}

fn new_session_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 完整路由（REST + WS）。
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/ws", get(ws::ws_handler))
        .nest("/api", rest::rest_router())
        .with_state(state)
}

/// server 主入口：装配 → 防双守卫 → 监听 → server.json 落盘 → 优雅停机。
///
/// 停机序（spec §4）：Ctrl-C → deny pending 审批 → cancel 当前回合 →
/// await 引擎任务 → 收尾 session run 落库 → 关闭全部 WS 连接 → 删除
/// server.json → 退出。
pub async fn serve(opts: ServeOptions) -> Result<()> {
    serve_with_shutdown(opts, ctrl_c_signal()).await
}

/// 生产停机信号：Ctrl-C。注册失败降级为立即完成（原「停机序降级为
/// 直接退出」语义）。
async fn ctrl_c_signal() {
    if tokio::signal::ctrl_c().await.is_err() {
        tracing::warn!("Ctrl-C 信号注册失败，停机序降级为直接退出");
    }
}

/// 测试接缝版 [`serve`]：停机信号可注入（生产 = Ctrl-C）。server.json
/// 的落盘/删除与防双守卫只在这里（serve 家族唯一入口）。
pub async fn serve_with_shutdown<F>(opts: ServeOptions, shutdown: F) -> Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let log_path = init_logging();

    let state = build_state(&opts).await?;

    // 防双守卫（在 bind 之前；health 探测是唯一判据，见 discovery 模块
    // 文档）。config 目录 = active config 父目录（发现链同源）。
    let info_dir = {
        let inner = state.core.lock();
        discovery::resolve_info_dir(&inner.paths.active, inner.paths.global.as_deref())
    };
    discovery::guard_against_running_server(&info_dir).await?;

    let addr = SocketAddr::new(opts.bind, opts.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("无法监听 {addr}"))?;
    // port 0 → 实际分配端口（server.json 必须写真值）。
    let addr = listener.local_addr().context("读取实际监听地址失败")?;

    let info = ServerInfo {
        proto: PROTOCOL_VERSION,
        url: discovery::client_ws_url(opts.bind, addr.port()),
        pid: std::process::id(),
        port: addr.port(),
        bind: opts.bind.to_string(),
        started_at: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        token: opts.auth_token.clone(),
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    };
    // 落盘失败不阻断 serve（--server 显式连接不受影响，仅自动发现缺失）。
    let info_path = match discovery::write_server_info(&info_dir, &info) {
        Ok(path) => Some(path),
        Err(e) => {
            tracing::warn!("server.json 落盘失败（自动发现不可用）: {e:#}");
            None
        }
    };

    let (active, global) = {
        let inner = state.core.lock();
        (inner.paths.active.clone(), inner.paths.global.clone())
    };
    tracing::info!(
        "openslate server v{} listening on http://{addr} (proto v{})",
        env!("CARGO_PKG_VERSION"),
        PROTOCOL_VERSION
    );
    tracing::info!("active config: {}", active.display());
    if let Some(g) = &global {
        tracing::info!("global library: {}", g.display());
    }
    if let Some(p) = &info_path {
        tracing::info!("server info: {}", p.display());
    }
    if opts.auth_token.is_none() && addr.ip() != IpAddr::from([127, 0, 0, 1]) {
        tracing::warn!(
            "监听非回环地址但未配置 --auth-token：任何人可连入本 server（建议 localhost 或加 token）"
        );
    }

    // 启动块（println 直出，不走 tracing——无论如何可见）。
    {
        let mut out = std::io::stdout().lock();
        let _ = write_startup_block(
            &mut out,
            &StartupBlock {
                proto: PROTOCOL_VERSION,
                addr,
                active: &active,
                global: global.as_deref(),
                server_info: info_path.as_deref(),
                log: log_path.as_deref(),
            },
        );
    }

    let app = build_router(state.clone());
    let shutdown_state = state.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.await;
            graceful_shutdown_work(shutdown_state).await;
        })
        .await
        .context("server 运行错误")?;

    discovery::remove_server_info(&info_dir);
    stdout_event!("bye");
    tracing::info!("server stopped");
    Ok(())
}

/// 启动块字段（`write_startup_block` 的入参；测试用 writer 注入断言）。
struct StartupBlock<'a> {
    proto: u32,
    addr: SocketAddr,
    active: &'a Path,
    global: Option<&'a Path>,
    /// server.json 落盘路径；写失败为 None（行省略）。
    server_info: Option<&'a Path>,
    /// server.log 路径；日志装配失败为 None（行省略）。
    log: Option<&'a Path>,
}

/// 启动块（监听就绪后一次性打印，stdout 前台可观察性）。
fn write_startup_block(out: &mut impl std::io::Write, b: &StartupBlock<'_>) -> std::io::Result<()> {
    writeln!(out, "openslate serve (proto {})", b.proto)?;
    writeln!(
        out,
        "  {:<11} http://{}   REST /api/* · WS /api/ws",
        "listening", b.addr
    )?;
    match b.global {
        Some(g) => writeln!(
            out,
            "  {:<11} {} (+global {})",
            "config",
            b.active.display(),
            g.display()
        )?,
        None => writeln!(out, "  {:<11} {}", "config", b.active.display())?,
    }
    if let Some(info) = b.server_info {
        writeln!(out, "  {:<11} {}", "server info", info.display())?;
    }
    if let Some(log) = b.log {
        writeln!(out, "  {:<11} {}", "log", log.display())?;
    }
    Ok(())
}

/// 优雅停机收尾（信号已触发；见 [`serve`] 文档）。
async fn graceful_shutdown_work(state: Arc<AppState>) {
    stdout_event!("shutting down…");

    // 1. 审批全 deny（引擎解栈）+ 取消当前回合。
    state.approval.deny_all();
    let (engine_task, session_run, stats_cost) = {
        let mut inner = state.core.lock();
        if let Some(cancel) = &inner.cancel {
            cancel.cancel();
        }
        (
            inner.engine_task.take(),
            inner.session_run.take(),
            inner.stats.total_cost_usd,
        )
    };
    stdout_event!("turn cancelled, approvals denied");

    // 2. 等引擎落地（manager 回传、事件广播完毕）。
    if let Some(task) = engine_task {
        let _ = task.await;
    }

    // 3. 收尾 run 行落库。
    if let Some(run) = session_run {
        if let Err(e) = run.recorder.finish("interrupted", None, stats_cost).await {
            tracing::warn!("failed to persist session run on shutdown: {e}");
        }
    }
    stdout_event!("engine settled, run persisted");

    // 4. 关闭全部连接（写任务发 Close 帧退出 → axum 收尾）。
    state.hub.close_all();
}

/// 日志双路：`~/.local/share/openslate/logs/server.log`（追加，全量——
/// tracing fmt 无 ANSI，子 agent 委派链 tracing 落这里）+ stdout 单行
/// 事件层（只放行 [`STDOUT_TARGET`]，裸格式无时间戳/级别，见
/// `stdout_event!`）。全局 subscriber 只能装一个：调用方（cli serve）
/// 不得先装（cli 的 main.rs 已为 serve 路径让位——两层都经这里装配，
/// `openslate serve` 入口路径下 stdout 层必然生效）；已装时退化为
/// stderr 警告并返回 None。
fn init_logging() -> Option<PathBuf> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, Layer};

    let dir = server_log_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("无法创建日志目录 {}: {e}", dir.display());
        return None;
    }
    let log_path = dir.join("server.log");
    let file = match std::fs::File::options()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("无法打开 server.log: {e}");
            return None;
        }
    };
    // 与 CLI 同款默认：rmcp 的 INFO 刷屏静音，RUST_LOG 可覆盖（作用于
    // 两层——RUST_LOG=off 时 stdout 事件同样静音）。
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,rmcp=warn"));
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file));
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_level(false)
        .with_writer(std::io::stdout)
        .with_filter(
            tracing_subscriber::filter::Targets::new()
                .with_target(STDOUT_TARGET, tracing_subscriber::filter::LevelFilter::INFO),
        );
    let installed = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stdout_layer)
        .try_init()
        .is_ok();
    if !installed {
        eprintln!("全局 tracing subscriber 已存在，server.log 不再重复装配");
    }
    Some(log_path)
}

fn server_log_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
        .map(|base| base.join("openslate/logs"))
        .unwrap_or_else(|| PathBuf::from(".openslate/logs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 启动块（writer 注入捕获）：listening + server info 两行是
    /// auto-attach-1 的断言重点。
    #[test]
    fn startup_block_lines() {
        let b = StartupBlock {
            proto: 1,
            addr: "127.0.0.1:7800".parse().unwrap(),
            active: Path::new("/p/.openslate/openslate.toml"),
            global: Some(Path::new("/home/u/.config/openslate/openslate.toml")),
            server_info: Some(Path::new("/p/.openslate/server.json")),
            log: Some(Path::new("/home/u/.local/share/openslate/logs/server.log")),
        };
        let mut buf = Vec::new();
        write_startup_block(&mut buf, &b).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "openslate serve (proto 1)");
        assert_eq!(
            lines[1],
            "  listening   http://127.0.0.1:7800   REST /api/* · WS /api/ws"
        );
        assert_eq!(
            lines[2],
            "  config      /p/.openslate/openslate.toml (+global /home/u/.config/openslate/openslate.toml)"
        );
        assert_eq!(lines[3], "  server info /p/.openslate/server.json");
        assert!(lines[4].starts_with("  log         "));
    }

    /// 无 global merge / 落盘失败的字段省略行为。
    #[test]
    fn startup_block_omits_missing_fields() {
        let b = StartupBlock {
            proto: 1,
            addr: "127.0.0.1:7800".parse().unwrap(),
            active: Path::new("/p/.openslate/openslate.toml"),
            global: None,
            server_info: None,
            log: None,
        };
        let mut buf = Vec::new();
        write_startup_block(&mut buf, &b).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("  config      /p/.openslate/openslate.toml\n"));
        assert!(!text.contains("server info"));
        assert!(!text.contains("(+global"));
    }
}
