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

    /// 数据库路径对齐（修"首启烙印"）。
    ///
    /// 动机：`openslate.toml` 的 `[database] path` 是**绝对路径**，首启
    /// 时被烙上当时的 data_dir；宿主之后迁移 data_dir（Android 迁移存储 /
    /// 备份恢复等），配置仍指向旧位置 → 新旧数据分叉。本方法在每次启动
    /// 时把 path 对齐到当前 `self.data_dir`：
    /// - 已对齐（父目录 == data_dir）→ 不动（自定义文件名也尊重）；
    /// - 旧库文件存在 → 迁移（rename，失败回退 copy+delete，再失败
    ///   warn 放弃——不破坏可用安装，也不改写 toml）；
    /// - 旧库文件不存在 → 直接改写 toml 指向新位置。
    ///
    /// toml_edit 原地改写 path，其余键/注释/格式零扰动。全程
    /// best-effort：任何失败 warn 后返回 `Ok(())`，仅在 toml 完全无法
    /// 解析时 `Err`（由调用方决定是否阻断）。
    pub fn realign_database_path(&self) -> Result<()> {
        let config_path = self.config_file();
        let raw = fs::read_to_string(&config_path)
            .with_context(|| format!("读取配置失败：{}", config_path.display()))?;
        // 唯一的 Err 路径：toml 彻底无法解析（配置损坏，让上层暴露）。
        let mut doc: toml_edit::DocumentMut = raw
            .parse()
            .with_context(|| format!("解析 {} 失败", config_path.display()))?;

        // 读取现有 [database] path；缺失则无事可做（store 层会用默认）。
        let Some(current) = doc
            .get("database")
            .and_then(|db| db.get("path"))
            .and_then(|item| item.as_str())
            .map(str::to_owned)
        else {
            return Ok(());
        };
        let old_path = PathBuf::from(&current);
        let target = self.data_dir.join("openslate.db");
        // 已对齐：父目录一致即可（自定义文件名不改写）。
        if old_path.parent() == Some(self.data_dir.as_path()) {
            return Ok(());
        }
        crate::alog!(
            "realign_database_path: {} → {}",
            old_path.display(),
            target.display()
        );
        // 目标已存在：可能是新位置已产生数据，迁移会覆盖它 → 放弃
        // （warn 返回，不破坏可用安装）。
        if target.exists() {
            crate::alog!(
                "realign_database_path: target {} 已存在，跳过迁移（避免覆盖新数据）",
                target.display()
            );
            return Ok(());
        }
        // 旧库文件存在则迁移；失败回退 copy+delete；再失败放弃（保持
        // toml 指向旧路径，安装仍可用）。
        if old_path.exists() {
            if let Some(parent) = target.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    crate::alog!("realign_database_path: 创建目标目录失败（放弃迁移）：{e}");
                    return Ok(());
                }
            }
            let moved = match fs::rename(&old_path, &target) {
                Ok(()) => true,
                Err(rename_err) => {
                    // 跨文件系统 rename 会失败 → copy + delete 回退。
                    let copied = fs::copy(&old_path, &target)
                        .map(|_| ())
                        .and_then(|()| fs::remove_file(&old_path));
                    if let Err(copy_err) = &copied {
                        crate::alog!(
                            "realign_database_path: rename 失败（{rename_err}），copy+delete 亦失败（{copy_err}），保持旧路径",
                        );
                    }
                    copied.is_ok()
                }
            };
            if !moved {
                return Ok(());
            }
        }
        // 改写 toml（旧文件不存在也照样改写——指针本身要归位）。
        // toml_edit 只动 path 的 value，decor/注释/其余键原样保留。
        doc["database"]["path"] = toml_edit::value(target.display().to_string());
        if let Err(e) = fs::write(&config_path, doc.to_string()) {
            crate::alog!(
                "realign_database_path: 写回 {} 失败（non-fatal）：{e}",
                config_path.display()
            );
        }
        Ok(())
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

# 功能 → 代号映射（主对话/上下文压缩/标题生成各用哪档模型）。
[capabilities]
main = "main"
compact = "fast"
title = "fast"

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

    /// 已对齐（父目录 == data_dir）→ toml 与文件均零改动。
    #[test]
    fn realign_noop_when_already_aligned() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = MobilePaths {
            config_dir: tmp.path().join("config"),
            workspace_dir: tmp.path().join("workspace"),
            data_dir: tmp.path().join("data"),
            cache_dir: tmp.path().join("cache"),
        };
        paths.ensure_layout().unwrap();
        let config = paths.config_file();
        let before = std::fs::read_to_string(&config).unwrap();
        paths.realign_database_path().unwrap();
        let after = std::fs::read_to_string(&config).unwrap();
        assert_eq!(before, after);
    }

    /// 未对齐 + 旧库文件存在 → 文件迁移 + toml 指针归位，其余内容
    /// （含注释与自定义文件名以外的键）零扰动。
    #[test]
    fn realign_moves_db_file_and_rewrites_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let old_data = tmp.path().join("old_data");
        let paths = MobilePaths {
            config_dir: tmp.path().join("config"),
            workspace_dir: tmp.path().join("workspace"),
            data_dir: tmp.path().join("data"),
            cache_dir: tmp.path().join("cache"),
        };
        paths.ensure_layout().unwrap();
        // 手工制造"烙印"：toml 指向 old_data，且旧库文件存在。
        let old_db = old_data.join("openslate.db");
        std::fs::create_dir_all(&old_data).unwrap();
        std::fs::write(&old_db, b"sqlite-bytes").unwrap();
        let config = paths.config_file();
        let body = std::fs::read_to_string(&config).unwrap();
        let patched = body.replace(
            &paths.data_dir.join("openslate.db").display().to_string(),
            &old_db.display().to_string(),
        );
        std::fs::write(&config, patched).unwrap();

        paths.realign_database_path().unwrap();

        // 旧文件被搬走，新位置有内容。
        assert!(!old_db.exists(), "旧库文件应被迁移走");
        let new_db = paths.data_dir.join("openslate.db");
        assert_eq!(std::fs::read(&new_db).unwrap(), b"sqlite-bytes");
        // toml 指针归位，且其余内容（注释等）与"从未烙印"时一致。
        let after = std::fs::read_to_string(&config).unwrap();
        let expected = body.replace(
            &old_db.display().to_string(),
            &paths.data_dir.join("openslate.db").display().to_string(),
        );
        assert_eq!(after, expected);
        assert!(after.contains("# Mobile 关闭桌面内置文件/Shell 工具"));
    }

    /// 未对齐 + 旧库文件不存在 → 只改 toml 指针，不产生文件。
    #[test]
    fn realign_rewrites_toml_when_old_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = MobilePaths {
            config_dir: tmp.path().join("config"),
            workspace_dir: tmp.path().join("workspace"),
            data_dir: tmp.path().join("data"),
            cache_dir: tmp.path().join("cache"),
        };
        paths.ensure_layout().unwrap();
        let old_db = tmp.path().join("elsewhere").join("openslate.db");
        let config = paths.config_file();
        let body = std::fs::read_to_string(&config).unwrap();
        let patched = body.replace(
            &paths.data_dir.join("openslate.db").display().to_string(),
            &old_db.display().to_string(),
        );
        std::fs::write(&config, patched).unwrap();

        paths.realign_database_path().unwrap();

        let after = std::fs::read_to_string(&config).unwrap();
        assert!(after.contains(&paths.data_dir.join("openslate.db").display().to_string()));
        assert!(!paths.data_dir.join("openslate.db").exists(), "不应生成空库文件");
    }

    /// toml 彻底无法解析 → 唯一的 Err 路径。
    #[test]
    fn realign_errors_only_on_unparseable_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = MobilePaths {
            config_dir: tmp.path().join("config"),
            workspace_dir: tmp.path().join("workspace"),
            data_dir: tmp.path().join("data"),
            cache_dir: tmp.path().join("cache"),
        };
        paths.ensure_layout().unwrap();
        std::fs::write(paths.config_file(), "not [ valid toml").unwrap();
        assert!(paths.realign_database_path().is_err());
    }
}
