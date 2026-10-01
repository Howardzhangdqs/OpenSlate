//! 配置 CRUD 写回链（客户端消息 → persist 落盘 → 重装 merged config →
//! validate 兜底 → 热替换 → 广播 config_changed）。
//!
//! 与 TUI `/provider` 面板写回链（app.rs `apply_models_change`）同源同
//! 路由：provider/model 条目写**全局库**（无全局库写 active），levels 写
//! **active**；`.env` 派生键写在库文件所在目录（0600）。引用完整性
//! 守卫先于落盘；validate 失败只定向 notice 不广播（磁盘已改、运行时
//! 保留旧 config——TUI 同款兜底语义）。
//!
//! 注意（mobile 语义差异）：`SetApiKey` 在 desktop 写 `.env`，mobile
//! 宿主（Android）不应走本路径——secret 由 FFI 的 set_api_key 内存注入
//! 并持久化在 Android Keystore（PLAN §18）。mobile 侧可在装配时决定
//! 是否将本消息路由到这里。

use std::sync::Arc;

use openslate_app::wiring::load_config_layered;
use openslate_core::config::persist;
use openslate_core::config::validation::validate_config;
use openslate_protocol::{ClientMsg, ModelDto, NoticeLevel, ProviderDto, ServerMsg};

use crate::state::{build_config_view, AppState};

/// `<NAME 大写化>_API_KEY`：非字母数字折成 `_`（TUI models.rs
/// `suggest_env_key` 镜像）。
fn suggest_env_key(name: &str) -> String {
    let upper: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .to_uppercase();
    format!("{upper}_API_KEY")
}

/// 单条 CRUD 变更（从 ClientMsg 提取）。
pub enum ConfigChange {
    UpsertProvider { name: String, provider: ProviderDto },
    DeleteProvider { name: String },
    UpsertModel { entry: String, model: ModelDto },
    DeleteModel { entry: String },
    SetLevel { level: String, entry: String },
    DeleteLevel { level: String },
}

impl ConfigChange {
    pub fn from_msg(msg: ClientMsg) -> Option<Self> {
        match msg {
            ClientMsg::UpsertProvider { name, provider } => {
                Some(Self::UpsertProvider { name, provider })
            }
            ClientMsg::DeleteProvider { name } => Some(Self::DeleteProvider { name }),
            ClientMsg::UpsertModel { entry, model } => Some(Self::UpsertModel { entry, model }),
            ClientMsg::DeleteModel { entry } => Some(Self::DeleteModel { entry }),
            ClientMsg::SetLevel { level, entry } => Some(Self::SetLevel { level, entry }),
            ClientMsg::DeleteLevel { level } => Some(Self::DeleteLevel { level }),
            _ => None,
        }
    }

    /// 单行运行时事件的「操作 key」标签（如 `upsert_provider gatedemo`）。
    fn stdout_label(&self) -> String {
        match self {
            ConfigChange::UpsertProvider { name, .. } => format!("upsert_provider {name}"),
            ConfigChange::DeleteProvider { name } => format!("delete_provider {name}"),
            ConfigChange::UpsertModel { entry, .. } => format!("upsert_model {entry}"),
            ConfigChange::DeleteModel { entry } => format!("delete_model {entry}"),
            ConfigChange::SetLevel { level, entry } => format!("set_level {level} → {entry}"),
            ConfigChange::DeleteLevel { level } => format!("delete_level {level}"),
        }
    }
}

/// 处理一条 CRUD：成功广播 config_changed；失败定向 notice（不广播）。
/// 返回 Some(成功描述) 仅用于日志。
pub fn apply_config_change(
    state: &Arc<AppState>,
    conn_id: u64,
    change: ConfigChange,
) -> Option<String> {
    let stdout_label = change.stdout_label();
    let (active, global) = {
        let inner = state.core.lock();
        (inner.paths.active.clone(), inner.paths.global.clone())
    };
    let library = global.clone().unwrap_or_else(|| active.clone());

    // ── 引用守卫（先于落盘）──────────────────────────────────────────
    {
        let inner = state.core.lock();
        match &change {
            ConfigChange::DeleteProvider { name } => {
                let refs: Vec<String> = inner
                    .config
                    .models
                    .iter()
                    .filter(|(_, m)| m.provider == *name)
                    .map(|(k, _)| k.clone())
                    .collect();
                if !refs.is_empty() {
                    notice(
                        state,
                        conn_id,
                        format!(
                            "provider {name} 被模型条目引用（{}），先解除引用",
                            refs.join(", ")
                        ),
                        NoticeLevel::Warn,
                    );
                    return None;
                }
            }
            ConfigChange::DeleteModel { entry } => {
                let mut refs: Vec<String> = inner
                    .config
                    .levels
                    .iter()
                    .filter(|(_, v)| **v == *entry)
                    .map(|(k, _)| k.clone())
                    .collect();
                // levels 存在时，直绑别名（models key 即级别名）也算引用。
                if !inner.config.levels.is_empty() && inner.config.models.contains_key(entry) {
                    refs.push(entry.clone());
                }
                if !refs.is_empty() {
                    notice(
                        state,
                        conn_id,
                        format!(
                            "模型条目 {entry} 被级别引用（{}），先解除引用",
                            refs.join(", ")
                        ),
                        NoticeLevel::Warn,
                    );
                    return None;
                }
            }
            ConfigChange::DeleteLevel { level } => {
                if level == "main" || level == "fast" {
                    notice(
                        state,
                        conn_id,
                        format!("级别 {level} 不可删除（main/fast 必需）"),
                        NoticeLevel::Warn,
                    );
                    return None;
                }
                if !inner.config.levels.contains_key(level) {
                    notice(
                        state,
                        conn_id,
                        format!("级别 {level} 不存在"),
                        NoticeLevel::Warn,
                    );
                    return None;
                }
            }
            _ => {}
        }
    }

    // ── 落盘（persist 层 toml_edit 原地编辑，注释零扰动）─────────────
    let what =
        match change {
            ConfigChange::UpsertProvider { name, provider } => {
                persist::upsert_provider(&library, &name, &provider.into())
                    .map(|_| format!("provider {name}"))
            }
            ConfigChange::DeleteProvider { name } => {
                persist::remove_provider(&library, &name).map(|_| format!("删除 provider {name}"))
            }
            ConfigChange::UpsertModel { entry, model } => {
                persist::upsert_model(&library, &entry, &model.into())
                    .map(|_| format!("模型条目 {entry}"))
            }
            ConfigChange::DeleteModel { entry } => persist::remove_model_entry(&library, &entry)
                .map(|_| format!("删除模型条目 {entry}")),
            ConfigChange::SetLevel { level, entry } => persist::set_level(&active, &level, &entry)
                .map(|_| format!("levels.{level} → {entry}")),
            ConfigChange::DeleteLevel { level } => {
                persist::remove_level(&active, &level).map(|_| format!("删除级别 {level}"))
            }
        };
    let what = match what {
        Ok(w) => w,
        Err(e) => {
            notice(
                state,
                conn_id,
                format!("配置写入失败：{e}"),
                NoticeLevel::Error,
            );
            return None;
        }
    };

    // ── 重装 merged config + validate 兜底 + 热替换 + 广播 ────────────
    let result = reload_and_swap(state, conn_id, &what);
    if result.is_some() {
        crate::session_event!("config: {stdout_label}");
    }
    result
}

/// set_api_key：派生 `<NAME>_API_KEY` 写 `.env`（0600，persist 层负责
/// 权限），同时 set 进本进程 env（.env 只在启动时被 dotenvy 加载）。
/// 值不回显、不广播 config_changed（.env 不属于 config 视图）。
pub fn apply_set_api_key(state: &Arc<AppState>, conn_id: u64, provider: &str, value: &str) {
    if provider.trim().is_empty() || value.is_empty() {
        notice(
            state,
            conn_id,
            "provider 名与 key 值必填".to_owned(),
            NoticeLevel::Warn,
        );
        return;
    }
    let var = suggest_env_key(provider);
    let dir = {
        let inner = state.core.lock();
        inner
            .paths
            .global
            .clone()
            .unwrap_or_else(|| inner.paths.active.clone())
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default()
    };
    match persist::upsert_env_key(&dir, &var, value) {
        Ok(()) => {
            // 进程内即时生效（下一回合的 provider 构建读 env）。
            // SAFETY: desktop/server 单线程装配期外的写入；mobile 不走本路径。
            std::env::set_var(&var, value);
            tracing::info!("api key for provider '{provider}' stored as {var}");
            crate::session_event!("config: set_api_key {provider}");
            notice(
                state,
                conn_id,
                format!("已写入 {var}（不回显）"),
                NoticeLevel::Info,
            );
        }
        Err(e) => {
            notice(
                state,
                conn_id,
                format!("写入 .env 失败：{e}"),
                NoticeLevel::Error,
            );
        }
    }
}

/// 重载 merged config → validate → 热替换（含在家的 manager）→ 广播。
fn reload_and_swap(state: &Arc<AppState>, conn_id: u64, what: &str) -> Option<String> {
    let (active, global, agents_cfg) = {
        let inner = state.core.lock();
        (
            inner.paths.active.clone(),
            inner.paths.global.clone(),
            inner.agents_cfg.clone(),
        )
    };
    let merged = match load_config_layered(&active, global.as_deref()) {
        Ok(cfg) => cfg,
        Err(e) => {
            notice(
                state,
                conn_id,
                format!("配置重载失败（运行时保留旧配置）：{e}"),
                NoticeLevel::Error,
            );
            return None;
        }
    };
    if let Some(err) = validate_config(&merged, &agents_cfg)
        .into_iter()
        .next()
        .map(|e| format!("{}: {}", e.field, e.message))
    {
        notice(
            state,
            conn_id,
            format!("校验失败（磁盘已写入，运行时保留旧配置）：{err}"),
            NoticeLevel::Error,
        );
        return None;
    }

    let view = {
        let mut inner = state.core.lock();
        inner.config = merged.clone();
        // 引擎用 config 热替换：manager 在家才碰它——回合
        // 进行中引擎持有旧 config，下回合自然用新的。
        if let Some(manager) = inner.manager.as_mut() {
            manager.config = merged.clone();
        }
        build_config_view(&inner)
    };
    state.sink.broadcast(ServerMsg::ConfigChanged {
        config: Box::new(view),
    });
    tracing::info!("config changed: {what}");
    Some(what.to_owned())
}

fn notice(state: &Arc<AppState>, conn_id: u64, text: String, level: NoticeLevel) {
    tracing::debug!("notice → conn {conn_id}: {text}");
    state
        .sink
        .send_to(conn_id, ServerMsg::Notice { text, level });
}
