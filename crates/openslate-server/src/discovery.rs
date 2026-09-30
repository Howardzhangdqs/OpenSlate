//! server.json 落盘/删除/防双（auto-attach-1）。
//!
//! serve 监听成功后把 [`ServerInfo`] 原子写进 config 目录（tmp+rename，
//! 0600——token 在里面）；优雅停机末尾 best-effort 删除。防双启动的
//! 唯一判据是 health 探测（**不做 pid 校验**——pid 复用会误判）：目标
//! 目录已有 server.json 且其 `url` 探测通 → 报错退出（文案含 pid/url）；
//! 探测不通（文件陈旧/server 已死）→ 覆盖写。
//!
//! TUI 侧的发现链在 `openslate_tui::discovery`（读同一份文件，文件名
//! 常量 [`SERVER_INFO_FILE`] 在 protocol crate 单一真相源）。

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use openslate_protocol::{ServerInfo, SERVER_INFO_FILE};

/// health 探测超时（~1s；见防双判据）。
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_millis(1000);

/// 客户端视角的完整 WS URL。bind 为通配地址（0.0.0.0 / ::）时 host 写
/// 回环（127.0.0.1 / [::1]）——server.json 服务于本机自动 attach，url
/// 未必与实际 bind 相同（信息不对称由 `bind` 字段补齐）；显式 bind
/// 原样使用（IPv6 加方括号）。
pub fn client_ws_url(bind: IpAddr, port: u16) -> String {
    let host = match bind {
        IpAddr::V4(v4) if v4.is_unspecified() => "127.0.0.1".to_owned(),
        IpAddr::V6(v6) if v6.is_unspecified() => "[::1]".to_owned(),
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    format!("ws://{host}:{port}/api/ws")
}

/// WS URL → health 探测 URL：scheme ws→http / wss→https，去掉尾部
/// `/api/ws` 后接 `/api/health`。解析失败（无 scheme）→ None。
pub fn health_url_from_ws(ws_url: &str) -> Option<String> {
    let (scheme, rest) = ws_url.trim().split_once("://")?;
    let scheme = match scheme {
        "ws" => "http",
        "wss" => "https",
        _ => return None,
    };
    let base = rest.strip_suffix("/api/ws").unwrap_or(rest);
    Some(format!("{scheme}://{base}/api/health"))
}

/// `http(s)/ws(s)://host[:port]/path` → (host, port, path)。IPv6 host
/// 带方括号（connect 时剥掉）；缺 port 用 scheme 默认值。
fn parse_http_target(url: &str) -> Option<(String, u16, String)> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let default_port = match scheme {
        "http" | "ws" => 80,
        "https" | "wss" => 443,
        _ => return None,
    };
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        // [::1]:7800
        let end = inner.find(']')?;
        let port = inner[end + 1..]
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_port);
        (inner[..end].to_owned(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_owned(), p.parse().unwrap_or(default_port)),
            None => (authority.to_owned(), default_port),
        }
    };
    Some((host, port, path.to_owned()))
}

/// 极简 HTTP GET 探测：只看状态行是否 2xx。手写 TCP 而非引入 HTTP
/// 客户端依赖——探测对象是本机 server.json 里的地址，且只有「2xx
/// 响应」才阻断启动（一切失败方向都当探测不通 → 覆盖写，安全侧）。
/// 注：wss/https 目标不做 TLS（本 server 恒写 ws:// url；手写文件的
/// TLS 地址探测不通 → 当陈旧覆盖）。
pub async fn probe_health(url: &str) -> bool {
    let Some((host, port, path)) = parse_http_target(url) else {
        return false;
    };
    let fut = async {
        let mut stream = tokio::net::TcpStream::connect((host.as_str(), port))
            .await
            .ok()?;
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUser-Agent: openslate-serve-guard\r\nConnection: close\r\n\r\n"
        );
        tokio::io::AsyncWriteExt::write_all(&mut stream, req.as_bytes())
            .await
            .ok()?;
        let mut buf = [0u8; 64];
        let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
            .await
            .ok()?;
        let head = std::str::from_utf8(&buf[..n]).unwrap_or("");
        (head.starts_with("HTTP/1.1 2") || head.starts_with("HTTP/1.0 2")).then_some(())
    };
    tokio::time::timeout(HEALTH_PROBE_TIMEOUT, fut)
        .await
        .unwrap_or(None)
        .is_some()
}

/// 防双启动守卫：目标 config 目录已有 server.json 且其 `url` 的 health
/// 探测通过 → Err（文案含已在跑的 pid/url）；文件缺失/读坏/JSON 非法/
/// url 无法推导/探测不通 → Ok（当陈旧文件，随后覆盖写）。
pub async fn guard_against_running_server(dir: &Path) -> Result<()> {
    let path = dir.join(SERVER_INFO_FILE);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let info: ServerInfo = match serde_json::from_str(&content) {
        Ok(info) => info,
        Err(e) => {
            tracing::warn!("{} 解析失败，视为陈旧文件将覆盖: {e}", path.display());
            return Ok(());
        }
    };
    let Some(health) = health_url_from_ws(&info.url) else {
        tracing::warn!(
            "{} 的 url 无法推导 health 地址，视为陈旧文件将覆盖",
            path.display()
        );
        return Ok(());
    };
    if probe_health(&health).await {
        return Err(anyhow!(
            "已有 openslate server 在运行（pid {}，{}）：请复用它，或先停止后再启动",
            info.pid,
            info.url
        ));
    }
    tracing::info!(
        "{} 指向的 server 未响应（pid {}），视为陈旧文件覆盖写",
        path.display(),
        info.pid
    );
    Ok(())
}

/// server.json 的落盘目录 = active config 的父目录（本地 `.openslate/`
/// 或全局 `~/.config/openslate/`，与 config 发现链同源）。bare 文件名
/// （`--config openslate.toml`）的空父目录保持空串——随后
/// `"".join("server.json")` 落 cwd，与 TUI `--config` 显式路径只查其
/// 父目录的发现侧对称。active 无父目录（理论边角）时用全局目录兜底。
pub fn resolve_info_dir(active: &Path, global: Option<&Path>) -> PathBuf {
    if let Some(parent) = active.parent() {
        return parent.to_path_buf();
    }
    global
        .and_then(Path::parent)
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(global_config_dir_fallback)
}

/// XDG 全局 config 目录兜底（与 wiring 的 resolve_paths 同源）。
fn global_config_dir_fallback() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .map(|base| base.join("openslate"))
        .unwrap_or_else(|| PathBuf::from(".openslate"))
}

/// 原子写 server.json（tmp+rename）+ 0600（token 在里面）。返回最终
/// 路径；重复写 = 覆盖（rename 原子替换）。
pub fn write_server_info(dir: &Path, info: &ServerInfo) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("无法创建 server.json 目录 {}", dir.display()))?;
    let path = dir.join(SERVER_INFO_FILE);
    let tmp = dir.join(format!(".{SERVER_INFO_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(info).context("编码 server.json 失败")?;
    write_private(&tmp, &bytes).with_context(|| format!("无法写 {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("无法落盘 {}（tmp+rename）", path.display()))?;
    Ok(path)
}

/// 0600 私有写（与 core persist 的 .env 写法同款；非 unix 退化普通写）。
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("无法打开 {}", path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("无法写入 {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes).with_context(|| format!("无法写 {}", path.display()))?;
    }
    Ok(())
}

/// 停机清理（best-effort，失败仅 WARN；NotFound 静默）。
pub fn remove_server_info(dir: &Path) {
    let path = dir.join(SERVER_INFO_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => tracing::info!("已删除 {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("无法删除 {}（best-effort）: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_loopback_and_v6() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        assert_eq!(
            client_ws_url(IpAddr::V4(Ipv4Addr::LOCALHOST), 7800),
            "ws://127.0.0.1:7800/api/ws"
        );
        // 通配 bind → url 仍写回环（供本机客户端）。
        assert_eq!(
            client_ws_url(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 7800),
            "ws://127.0.0.1:7800/api/ws"
        );
        assert_eq!(
            client_ws_url(IpAddr::V6(Ipv6Addr::LOCALHOST), 7800),
            "ws://[::1]:7800/api/ws"
        );
        assert_eq!(
            client_ws_url(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 7800),
            "ws://[::1]:7800/api/ws"
        );
        // 显式非回环 bind 原样保留。
        assert_eq!(
            client_ws_url(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 9000),
            "ws://192.168.1.5:9000/api/ws"
        );
    }

    #[test]
    fn health_url_derivation() {
        assert_eq!(
            health_url_from_ws("ws://127.0.0.1:7800/api/ws").as_deref(),
            Some("http://127.0.0.1:7800/api/health")
        );
        // 无路径的裸 url 也可推导。
        assert_eq!(
            health_url_from_ws("ws://localhost:7800").as_deref(),
            Some("http://localhost:7800/api/health")
        );
        assert_eq!(
            health_url_from_ws("wss://example.com/api/ws").as_deref(),
            Some("https://example.com/api/health")
        );
        assert_eq!(health_url_from_ws("127.0.0.1:7800"), None);
        assert_eq!(health_url_from_ws("ftp://x/api/ws"), None);
    }

    #[test]
    fn http_target_parsing() {
        assert_eq!(
            parse_http_target("http://127.0.0.1:7800/api/health"),
            Some(("127.0.0.1".into(), 7800, "/api/health".into()))
        );
        assert_eq!(
            parse_http_target("http://example.com/api/health"),
            Some(("example.com".into(), 80, "/api/health".into()))
        );
        assert_eq!(
            parse_http_target("http://[::1]:7800/api/health"),
            Some(("::1".into(), 7800, "/api/health".into()))
        );
        assert_eq!(
            parse_http_target("http://[::1]/api/health"),
            Some(("::1".into(), 80, "/api/health".into()))
        );
        assert_eq!(parse_http_target("not-a-url"), None);
    }

    #[tokio::test]
    async fn probe_dead_port_is_false() {
        // 占一个端口再立刻关掉，拿一个几乎必然死掉的地址。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(!probe_health(&format!("http://127.0.0.1:{port}/api/health")).await);
    }

    #[tokio::test]
    async fn probe_real_http_responder() {
        // 真 HTTP 端点（最小 responder，非 axum——axum 层面的验证在
        // 集成测试对真 health 端点做）。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 256];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await;
            }
        });
        assert!(probe_health(&format!("http://127.0.0.1:{port}/api/health")).await);
        // 非 2xx 状态行不算活。
        // （同一 responder 只回 200，这里用 404 分支单独起一个。）
        let l404 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p404 = l404.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut sock, _)) = l404.accept().await {
                let mut buf = [0u8; 256];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });
        assert!(!probe_health(&format!("http://127.0.0.1:{p404}/api/health")).await);
    }

    #[test]
    fn info_dir_follows_active_parent() {
        assert_eq!(
            resolve_info_dir(Path::new("/p/.openslate/openslate.toml"), None),
            PathBuf::from("/p/.openslate")
        );
        // bare 文件名：空父目录保持（= cwd 相对），与 TUI --config 发现对称。
        assert_eq!(
            resolve_info_dir(Path::new("openslate.toml"), None),
            PathBuf::from("")
        );
        // 显式 --config 的路径优先于 global（不走 global 兜底）。
        assert_eq!(
            resolve_info_dir(
                Path::new("/p/.openslate/openslate.toml"),
                Some(Path::new("/home/u/.config/openslate/openslate.toml"))
            ),
            PathBuf::from("/p/.openslate")
        );
    }

    #[test]
    fn write_and_remove_server_info() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".openslate");
        let info = ServerInfo {
            proto: 1,
            url: "ws://127.0.0.1:7800/api/ws".into(),
            pid: 42,
            port: 7800,
            bind: "127.0.0.1".into(),
            started_at: "2026-09-30T15:20:19+08:00".into(),
            token: Some("s3cret".into()),
            cwd: "/tmp".into(),
        };
        let path = write_server_info(&dir, &info).unwrap();
        assert_eq!(path, dir.join(SERVER_INFO_FILE));
        let back: ServerInfo =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, info);

        // 0600（token 在里面）。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // tmp+rename：不留临时文件；重复写覆盖。
        assert!(!dir.join(format!(".{SERVER_INFO_FILE}.tmp")).exists());
        let info2 = ServerInfo { pid: 43, ..info };
        write_server_info(&dir, &info2).unwrap();
        let back2: ServerInfo =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back2.pid, 43);

        remove_server_info(&dir);
        assert!(!path.exists());
        // NotFound 静默（重复删不炸）。
        remove_server_info(&dir);
    }

    #[tokio::test]
    async fn guard_stale_files_pass() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".openslate");
        std::fs::create_dir_all(&dir).unwrap();
        // 文件缺失 → Ok。
        guard_against_running_server(&dir).await.unwrap();
        // JSON 坏 → Ok（当陈旧）。
        std::fs::write(dir.join(SERVER_INFO_FILE), "{not json").unwrap();
        guard_against_running_server(&dir).await.unwrap();
        // 死端口 → Ok（覆盖写路径）。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let stale = ServerInfo {
            proto: 1,
            url: format!("ws://127.0.0.1:{port}/api/ws"),
            pid: 999,
            port,
            bind: "127.0.0.1".into(),
            started_at: String::new(),
            token: None,
            cwd: String::new(),
        };
        std::fs::write(
            dir.join(SERVER_INFO_FILE),
            serde_json::to_string(&stale).unwrap(),
        )
        .unwrap();
        guard_against_running_server(&dir).await.unwrap();
    }
}
