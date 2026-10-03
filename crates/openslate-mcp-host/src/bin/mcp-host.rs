//! mcp-host CLI：读清单 → 起 axum（/mcp + 鉴权）→ 常驻服务。
//!
//! 用法（Termux）：
//! ```sh
//! mcp-host --config ~/.openslate/mcp-host.toml
//! # token 未配置时自动生成并写入 ~/.openslate/mcp-host.toml.token，
//! # App 侧可经 RUN_COMMAND `cat` 取回（TermuxExec 回传通道）。
//! ```

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;

use openslate_mcp_host::{build_router, load_config, resolve_bind, DownstreamHub};

#[derive(Parser)]
#[command(
    name = "mcp-host",
    about = "OpenSlate MCP 聚合宿主：spawn 下游 stdio MCP servers，统一以 Streamable HTTP 暴露给本机 App"
)]
struct Args {
    /// 清单文件路径
    #[arg(short, long, default_value = None)]
    config: Option<std::path::PathBuf>,
}

fn default_config_path() -> std::path::PathBuf {
    dirs_or_env_home().join(".openslate").join("mcp-host.toml")
}

/// $HOME 优先（Termux 下即 Termux 私有目录，App 不可读——恰好隔离）。
fn dirs_or_env_home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/data/data/com.termux/files/home"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mcp_host=info,axum=warn,tower=warn".into()),
        )
        .init();

    let args = Args::parse();
    let config_path = args.config.unwrap_or_else(default_config_path);
    let mut cfg = load_config(&config_path)?;

    // token 语义：Some(t) 非空 → 用；Some("") → 显式关闭鉴权（调试）；
    // None → 生成随机 token 并落盘旁车文件。
    if let Some(token) = &cfg.token {
        // 清单里显式空串 = 关闭鉴权，不生成。
        if !token.is_empty() {
            // 已配置 token：直接使用。
            tracing::info!(target: "mcp_host", "using token from config");
        }
    } else {
        let generated = openslate_mcp_host::generate_token()
            .context("generating token from /dev/urandom")?;
        let sidecar = config_path.with_extension("toml.token");
        std::fs::write(&sidecar, &generated)
            .with_context(|| format!("writing token file {}", sidecar.display()))?;
        tracing::info!(target: "mcp_host", "generated new token → {}", sidecar.display());
        cfg.token = Some(generated);
    }

    let bind = resolve_bind(&cfg)?;
    let n_servers = cfg.servers.len();
    let auth_on = cfg
        .token
        .as_deref()
        .is_some_and(|t| !t.is_empty());
    let hub = Arc::new(DownstreamHub::new(cfg.servers));
    let router = build_router(hub, cfg.token);
    tracing::info!(target: "mcp_host",
        "listening on http://{bind}/mcp ({n_servers} downstream server(s), auth {})",
        if auth_on { "on" } else { "OFF" });

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!(target: "mcp_host", "shutting down");
        })
        .await
        .context("axum server error")
}
