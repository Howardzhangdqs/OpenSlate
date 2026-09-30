//! `openslate serve` — web-1 server 前端转发（装配在 openslate-server
//! crate，这里只做 flag 解析与调用）。

use anyhow::Result;
use openslate_server::{serve, ServeOptions};
use std::net::IpAddr;

/// `serve` 子命令参数（main.rs Commands::Serve 的镜像）。
pub struct ServeArgs {
    pub bind: IpAddr,
    pub port: u16,
    pub auth_token: Option<String>,
}

/// 入口：转发给 openslate-server::serve。
pub async fn run_serve(config_flag: Option<&str>, args: ServeArgs) -> Result<()> {
    let opts = ServeOptions {
        bind: args.bind,
        port: args.port,
        auth_token: args.auth_token,
        config_flag: config_flag.map(str::to_owned),
        provider_factory: None,
    };
    serve(opts).await
}
