//! 首启 bootstrap：目录布局 + 默认配置脚手架（mobile 版 `openslate init`）。
//!
//! 路径全部由宿主传入（PLAN §19：Kotlin 把 filesDir / cacheDir 等真实
//! 路径显式给 Rust，Rust 不做任何 Android 特定路径发现）。
//!
//! 数据库用**绝对路径**（`init_store` 对相对路径按 cwd 解析——Android
//! cwd 是 `/`，相对路径会指到不可写位置）。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Android 宿主注入的真实路径集（FFI Record 逐字段映射）。
#[derive(Debug, Clone)]
pub struct MobilePaths {
    /// 配置根（openslate.toml、agents/、prompts/ 居住地）。
    pub config_dir: PathBuf,
    /// 工作区根（未来文件类工具的沙箱；Phase 1 仅占位）。
    pub workspace_dir: PathBuf,
    /// 持久数据（openslate.db）。
    pub data_dir: PathBuf,
    /// 可丢弃缓存。
    pub cache_dir: PathBuf,
}

/// mobile 默认 root agent：显式工具白名单（tools 为空 = 暴露全部注册
/// 工具，mobile 语义下不可接受——Phase 1 只有 mobile.ping）。
const DEFAULT_ROOT_AGENT_MD: &str = r#"---
id: root
name: OpenSlate
model: main
tools:
    - mobile.ping
    - bash
    - termux_bash
---
You are a helpful AI assistant.
"#;

/// 旧版默认（历届自动生成的 Phase 1/2 白名单，历史命名 shell.run /
/// termux.run）。已装机设备的 root.md 与之一致时视为未定制，自动迁移
/// 到当前默认（bash / termux_bash 多选命名）；用户改过则保持不动。
const LEGACY_ROOT_AGENT_MDS: &[&str] = &[
    // v2：双后端固定名（shell.run / termux.run）。
    r#"---
id: root
name: OpenSlate
model: main
tools:
    - mobile.ping
    - shell.run
    - termux.run
---
You are a helpful AI assistant.
"#,
    // v1：仅 Termux、依赖 PC 中继。
    r#"---
id: root
name: OpenSlate
model: main
tools:
    - mobile.ping
    - termux.run
---
You are a helpful AI assistant.
"#,
];

impl MobilePaths {
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("openslate.toml")
    }

    fn agents_dir(&self) -> PathBuf {
        self.config_dir.join("agents")
    }

    /// 建目录 + 首启默认文件（已存在则跳过，幂等）。
    /// 返回实际生效的配置文件路径。
    pub fn ensure_layout(&self) -> Result<PathBuf> {
        for dir in [
            &self.config_dir,
            &self.workspace_dir,
            &self.data_dir,
            &self.cache_dir,
        ] {
            fs::create_dir_all(dir)
                .with_context(|| format!("创建目录失败：{}", dir.display()))?;
        }
        let agents = self.agents_dir();
        fs::create_dir_all(&agents)
            .with_context(|| format!("创建目录失败：{}", agents.display()))?;

        let config_path = self.config_file();
        if !config_path.exists() {
            let db_path = self.data_dir.join("openslate.db");
            let default = default_openslate_toml(&db_path);
            fs::write(&config_path, default)
                .with_context(|| format!("写入默认配置失败：{}", config_path.display()))?;
            tracing::info!("mobile bootstrap: wrote default config to {}", config_path.display());
        }

        let root_md = agents.join("root.md");
        if !root_md.exists() {
            fs::write(&root_md, DEFAULT_ROOT_AGENT_MD)
                .with_context(|| format!("写入默认 agent 失败：{}", root_md.display()))?;
            tracing::info!("mobile bootstrap: wrote default root agent");
        } else {
            let current = fs::read_to_string(&root_md).unwrap_or_default();
            if LEGACY_ROOT_AGENT_MDS.iter().any(|legacy| current == *legacy) {
                // 旧默认升级：原样覆盖即可获得 bash / termux_bash；用户
                // 定制过的文件不会被触碰。
                fs::write(&root_md, DEFAULT_ROOT_AGENT_MD)
                    .with_context(|| format!("迁移默认 agent 失败：{}", root_md.display()))?;
                tracing::info!("mobile bootstrap: migrated legacy root agent (bash/termux_bash)");
            }
        }

        Ok(config_path)
    }
}

/// mobile 默认 openslate.toml（与 CLI `openslate init` 同源，差异：
/// - `[builtin_tools] enabled = false`（fs/shell 桌面工具在 Android 无
///   意义且越权，PLAN §34）；
/// - 数据库绝对路径指向宿主 data_dir）。
fn default_openslate_toml(db_path: &Path) -> String {
    format!(
        r#"[project]
name = "OpenSlate Mobile"

[providers.zhipu]
label = "Zhipu BigModel"
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key_env = "ZHIPU_API_KEY"

[models.main]
provider = "zhipu"
model = "glm-5.3-flash"

[models.fast]
provider = "zhipu"
model = "glm-5.3-flash"

[levels]
main = "main"
fast = "fast"

[limits]
auto_compact = true
max_context_bytes = 60000
max_context_messages = 220
max_turn_output_bytes = 131072
# 无总时长预算（i64::MAX；TOML 整数为 i64 域，u64::MAX 字面量会被
# toml_edit 拒绝 → persist 层写回失败）；挂死防护 = 每请求 60s 无新数据即断。
timeout_ms = 9223372036854775807

# Mobile 关闭桌面内置文件/Shell 工具（Phase 3+ 由 Android 能力接替）。
[builtin_tools]
enabled = false

[database]
path = "{db}"
"#,
        db = db_path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_layout_is_idempotent_and_writes_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = MobilePaths {
            config_dir: tmp.path().join("config"),
            workspace_dir: tmp.path().join("workspace"),
            data_dir: tmp.path().join("data"),
            cache_dir: tmp.path().join("cache"),
        };
        let config = paths.ensure_layout().unwrap();
        assert!(config.is_file());
        assert!(paths.agents_dir().join("root.md").is_file());

        let body = std::fs::read_to_string(&config).unwrap();
        assert!(body.contains("enabled = false"));
        assert!(body.contains(&paths.data_dir.join("openslate.db").display().to_string()));

        // 第二次不改动。
        let before = std::fs::read_to_string(&config).unwrap();
        paths.ensure_layout().unwrap();
        let after = std::fs::read_to_string(&config).unwrap();
        assert_eq!(before, after);
    }
}
