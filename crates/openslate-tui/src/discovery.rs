//! server 自动发现（auto-attach-1）：无 `--server` 时读 `<config 目录>/
//! server.json` 自动 attach。
//!
//! 解析链：**flag > env `OPENSLATE_SERVER` > server.json 发现 > 报错退出
//! （码 1）**。flag/env 两级由 clap 归并（`--server` 带
//! `env = "OPENSLATE_SERVER"`，flag 优先是 clap 语义），本模块的入口
//! [`resolve_server`] 拿到的 `Option<String>` 已是归并结果。
//!
//! 发现目录与 GAP-2 本地 `[tui]` 段发现同构：`--config <path>` 显式
//! 指定时只查其父目录；否则 `./.openslate/` → `~/.config/openslate/`，
//! 取第一个可用的 server.json（文件名常量在 protocol crate，
//! server 侧 openslate-server::discovery 写同一份文件——本 crate
//! 不依赖 server，仅共享协议类型）。
//!
//! 陈旧文件语义：JSON 损坏或 `proto` ≠ [`PROTOCOL_VERSION`] → WARN 到
//! tui.log 并跳过该文件（继续查下一个目录）；全部落空 = 未发现。
//! 文件里的 `token` 自动携带；`--server-token`/env `OPENSLATE_TOKEN`
//! 仍优先于文件值。

use std::path::{Path, PathBuf};

use openslate_protocol::{ServerInfo, PROTOCOL_VERSION, SERVER_INFO_FILE};

/// 解析出的 server 连接参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    /// WS URL（可能是裸 host[:port]，归一化由 `client::connect` 做）。
    pub url: String,
    /// 鉴权 token：flag/env 优先，否则 server.json 里的值。
    pub token: Option<String>,
    /// server.json 来源路径；显式指定（flag/env）路径为 `None`——
    /// 连接失败的错误文案按它区分（发现但连不上 vs 连接失败）。
    pub discovered_from: Option<PathBuf>,
}

/// 发现链目录（与 GAP-2 `[tui]` 段发现同构；见模块文档）。
pub fn discovery_dirs(config_flag: Option<&str>) -> Vec<PathBuf> {
    match config_flag {
        Some(flag) => vec![parent_of(flag)],
        None => {
            let mut v = Vec::new();
            if let Ok(cwd) = std::env::current_dir() {
                v.push(cwd.join(".openslate"));
            }
            if let Some(cfg) = dirs::config_dir() {
                v.push(cfg.join("openslate"));
            }
            v
        }
    }
}

/// `--config` 显式路径的父目录（bare 文件名 → 空路径 = cwd 相对，与
/// server 侧 resolve_info_dir 的落盘位置对称）。
fn parent_of(path: &str) -> PathBuf {
    Path::new(path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// 按目录顺序取第一个**可用**的 server.json（读坏/JSON 非法/proto
/// 陈旧 → WARN 跳过；目录/文件不存在静默跳过）。
pub fn discover_in(dirs: &[PathBuf]) -> Option<ServerSpec> {
    for dir in dirs {
        let path = dir.join(SERVER_INFO_FILE);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        match serde_json::from_str::<ServerInfo>(&content) {
            Err(e) => {
                tracing::warn!("{} 解析失败，当作未发现处理: {e}", path.display());
                continue;
            }
            Ok(info) if info.proto != PROTOCOL_VERSION => {
                tracing::warn!(
                    "{} 协议版本陈旧（文件 {} ≠ 本端 {}），跳过",
                    path.display(),
                    info.proto,
                    PROTOCOL_VERSION
                );
                continue;
            }
            Ok(info) => {
                tracing::info!(
                    "发现 server: {}（来自 {}，pid {}）",
                    info.url,
                    path.display(),
                    info.pid
                );
                return Some(ServerSpec {
                    url: info.url,
                    token: info.token,
                    discovered_from: Some(path),
                });
            }
        }
    }
    None
}

/// 主解析链：显式值（flag/env，clap 归并后）直通；否则走发现链。
/// `None` = 未发现（调用方报错退出码 1）。
pub fn resolve_server(
    explicit: Option<&str>,
    token_flag: Option<String>,
    config_flag: Option<&str>,
) -> Option<ServerSpec> {
    if let Some(url) = explicit {
        return Some(ServerSpec {
            url: url.to_owned(),
            token: token_flag,
            discovered_from: None,
        });
    }
    let spec = discover_in(&discovery_dirs(config_flag))?;
    // token 优先级：flag/env（--server-token / OPENSLATE_TOKEN）> 文件值。
    Some(ServerSpec {
        token: token_flag.or(spec.token),
        ..spec
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_info(dir: &Path, url: &str, proto: u32, token: Option<&str>) {
        std::fs::create_dir_all(dir).unwrap();
        let info = ServerInfo {
            proto,
            url: url.into(),
            pid: 123,
            port: 7800,
            bind: "127.0.0.1".into(),
            started_at: "2026-09-30T15:20:19+08:00".into(),
            token: token.map(str::to_owned),
            cwd: "/tmp".into(),
        };
        std::fs::write(
            dir.join(SERVER_INFO_FILE),
            serde_json::to_string(&info).unwrap(),
        )
        .unwrap();
    }

    // 路径 1：flag（env 经 clap 归并后同为 Some——「flag/env」一路在此
    // 覆盖；clap 的 env→flag 映射是 clap 自身语义，不在本模块）。
    #[test]
    fn explicit_value_bypasses_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let local = tmp.path().join(".openslate");
        write_info(&local, "ws://127.0.0.1:1/api/ws", 1, Some("file-token"));
        let config = local.join("openslate.toml").to_string_lossy().into_owned();

        let spec = resolve_server(
            Some("127.0.0.1:2"),
            Some("flag-token".into()),
            Some(&config),
        )
        .unwrap();
        assert_eq!(spec.url, "127.0.0.1:2");
        assert_eq!(spec.token.as_deref(), Some("flag-token"));
        assert!(spec.discovered_from.is_none(), "显式指定无来源文件标记");
    }

    // 路径 2：server.json 发现（--config 显式目录）+ 文件 token 携带。
    #[test]
    fn discovers_via_explicit_config_parent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let local = tmp.path().join(".openslate");
        write_info(&local, "ws://127.0.0.1:7800/api/ws", 1, Some("file-token"));
        let config = local.join("openslate.toml").to_string_lossy().into_owned();

        let spec = resolve_server(None, None, Some(&config)).unwrap();
        assert_eq!(spec.url, "ws://127.0.0.1:7800/api/ws");
        assert_eq!(
            spec.token.as_deref(),
            Some("file-token"),
            "文件 token 自动携带"
        );
        assert_eq!(
            spec.discovered_from.as_deref(),
            Some(local.join(SERVER_INFO_FILE).as_path())
        );

        // token 优先级：flag/env 值 > 文件值。
        let spec = resolve_server(None, Some("flag-token".into()), Some(&config)).unwrap();
        assert_eq!(spec.token.as_deref(), Some("flag-token"));
    }

    #[test]
    fn discovery_order_first_dir_wins() {
        let tmp = tempfile::TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        write_info(&a, "ws://127.0.0.1:1/api/ws", 1, None);
        write_info(&b, "ws://127.0.0.1:2/api/ws", 1, None);
        let spec = discover_in(&[a.clone(), b]).unwrap();
        assert_eq!(spec.url, "ws://127.0.0.1:1/api/ws", "第一个目录优先");

        // 首目录文件坏/陈旧 → 顺延到下一个目录。
        let c = tmp.path().join("c");
        let d = tmp.path().join("d");
        std::fs::create_dir_all(&c).unwrap();
        std::fs::write(c.join(SERVER_INFO_FILE), "{not json").unwrap();
        write_info(&d, "ws://127.0.0.1:4/api/ws", PROTOCOL_VERSION, None);
        let proto_mismatch = tmp.path().join("e");
        write_info(
            &proto_mismatch,
            "ws://127.0.0.1:5/api/ws",
            PROTOCOL_VERSION + 1,
            None,
        );
        let spec = discover_in(&[c, proto_mismatch, d.clone()]).unwrap();
        assert_eq!(
            spec.url, "ws://127.0.0.1:4/api/ws",
            "坏 JSON 与 proto 陈旧均跳过"
        );
    }

    // 路径 3：报错（未发现）——候选目录无可用文件。
    #[test]
    fn not_found_when_no_candidate() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(
            resolve_server(
                None,
                None,
                Some(&tmp.path().join("x/openslate.toml").to_string_lossy())
            )
            .is_none(),
            "空目录 → 未发现"
        );
        // JSON 损坏 / proto 陈旧的唯一候选同样算未发现。
        let bad = tmp.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join(SERVER_INFO_FILE), "not json").unwrap();
        assert!(discover_in(&[bad]).is_none());
        let stale = tmp.path().join("stale");
        write_info(
            &stale,
            "ws://127.0.0.1:9/api/ws",
            PROTOCOL_VERSION + 7,
            None,
        );
        assert!(discover_in(&[stale]).is_none());
    }

    #[test]
    fn parent_of_bare_filename_is_cwd_relative() {
        // bare `--config openslate.toml` → 空父目录，join 后 = cwd 相对
        // server.json（与 server 侧落盘位置对称）。
        assert_eq!(parent_of("openslate.toml"), PathBuf::from(""));
        assert_eq!(
            parent_of("/p/.openslate/openslate.toml"),
            PathBuf::from("/p/.openslate")
        );
        // 发现链目录顺序：本地 .openslate 在全局 config 前（顺序在
        // discovery_order_first_dir_wins 用目录列表语义覆盖；此断言只钉
        // parent 语义）。
    }
}
