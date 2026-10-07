//! 配置 CRUD 写回链（客户端消息 → persist 落盘 → 重装 merged config →
//! validate 兜底 → 热替换 → 广播 config_changed）。
//!
//! 与 TUI `/provider` 面板写回链（app.rs `apply_models_change`）同源同
//! 路由：provider/model 条目写**全局库**（无全局库写 active），levels 写
//! **active**；`.env` 派生键写在库文件所在目录（0600）。引用完整性
//! 守卫先于落盘；validate 失败只定向 notice 不广播（磁盘已改、运行时
//! 保留旧 config——TUI 同款兜底语义）。
//!
//! levels 与 capabilities 对模型条目都是**松耦合**引用：删除被引用的
//! 模型条目一律放行，悬空是合法状态（UI 显示"绑定缺失"、
//! `resolve_model` 使用时报 `InvalidLevelRef`、compact/title 运行时
//! 静默回退默认档），删除成功后按悬空组发 Info notice 提示改绑。
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
    SetCapability { capability: String, alias: String },
    UpsertMcpServer {
        name: String,
        url: String,
        headers: Option<std::collections::HashMap<String, String>>,
    },
    RemoveMcpServer { name: String },
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
            ClientMsg::SetCapability { capability, alias } => {
                Some(Self::SetCapability { capability, alias })
            }
            ClientMsg::UpsertMcpServer {
                name,
                url,
                headers,
            } => Some(Self::UpsertMcpServer {
                name,
                url,
                headers,
            }),
            ClientMsg::RemoveMcpServer { name } => Some(Self::RemoveMcpServer { name }),
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
            ConfigChange::SetCapability { capability, alias } => {
                format!("set_capability {capability} → {alias}")
            }
            ConfigChange::UpsertMcpServer { name, .. } => {
                format!("upsert_mcp_server {name}")
            }
            ConfigChange::RemoveMcpServer { name } => format!("remove_mcp_server {name}"),
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
    // 删模型不设引用拦截（levels / capabilities 均松耦合，悬空合法）；
    // `dangling` / `cap_dangling` 记录删除后将悬空的引用，落盘+热替换
    // 成功后据此发 Info 提示改绑。
    let mut dangling: Vec<String> = Vec::new();
    let mut cap_dangling: Vec<String> = Vec::new();
    {
        let inner = state.core.lock();
        match &change {
            ConfigChange::UpsertModel { entry, model } => {
                // 同一 (provider, model) 组合全局唯一：entry 只是内部键，
                // (provider, model) 才是用户视角的"模型"。另一条目已占用
                // 同组合 → 拒绝落盘，防止客户端（如自动生成随机后缀
                // entry 的自动检测）把同一模型写两份、列表出现重复。
                // 同 entry 覆盖更新不在此列（查重跳过 key == entry）。
                if let Some(dup) = inner
                    .config
                    .models
                    .iter()
                    .find(|(k, m)| {
                        *k != entry && m.provider == model.provider && m.model == model.model
                    })
                    .map(|(k, _)| k.clone())
                {
                    notice(
                        state,
                        conn_id,
                        format!(
                            "provider {} 下已存在模型 {}（条目 {}），请编辑原条目或先删除重复条目",
                            model.provider, model.model, dup
                        ),
                        NoticeLevel::Error,
                    );
                    return None;
                }
            }
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
                // 删模型不设引用拦截（悬空合法）。此处只统计删除后将
                // 悬空的引用，供成功后的 Info notice 用：
                // ① levels 中 value == entry 的代号；
                dangling = inner
                    .config
                    .levels
                    .iter()
                    .filter(|(_, v)| **v == *entry)
                    .map(|(k, _)| k.clone())
                    .collect::<Vec<_>>();
                // ② capabilities 直绑（value 是 models 条目名而非
                //    levels key）的功能。entry 是 levels key 时该 value
                //    走 levels 解析、属 ① 的路径，不重复计。
                if !inner.config.levels.contains_key(entry) {
                    cap_dangling = inner
                        .config
                        .capabilities
                        .iter()
                        .filter(|(_, v)| **v == *entry)
                        .map(|(k, _)| k.clone())
                        .collect::<Vec<_>>();
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
                // capabilities 引用的代号不可删（会留下悬空映射）。
                let cap_refs: Vec<String> = inner
                    .config
                    .capabilities
                    .iter()
                    .filter(|(_, v)| *v == level)
                    .map(|(k, _)| k.clone())
                    .collect();
                if !cap_refs.is_empty() {
                    notice(
                        state,
                        conn_id,
                        format!(
                            "代号 {level} 被功能引用（{}），先改绑再删除",
                            cap_refs.join(", ")
                        ),
                        NoticeLevel::Warn,
                    );
                    return None;
                }
            }
            ConfigChange::SetCapability { capability, alias } => {
                if !openslate_core::model_config::CAPABILITIES.contains(&capability.as_str()) {
                    notice(
                        state,
                        conn_id,
                        format!(
                            "未知功能 '{capability}'（可用：{}）",
                            openslate_core::model_config::CAPABILITIES.join(", ")
                        ),
                        NoticeLevel::Error,
                    );
                    return None;
                }
                // 与 SetModel 同款可解析性检查（levels key 或直绑 models 条目）。
                let resolvable = inner.config.models.contains_key(alias)
                    || openslate_core::model_config::resolve_model(&inner.config, alias).is_ok();
                if !resolvable {
                    let mut available: Vec<String> = inner.config.levels.keys().cloned().collect();
                    available.extend(inner.config.models.keys().cloned());
                    available.sort();
                    available.dedup();
                    notice(
                        state,
                        conn_id,
                        format!(
                            "未知代号 '{alias}'（可用：{}）",
                            available.join(", ")
                        ),
                        NoticeLevel::Error,
                    );
                    return None;
                }
            }
            _ => {}
        }
    }

    // ── 落盘（persist 层 toml_edit 原地编辑，注释零扰动）─────────────
    // main 能力改绑 = 换主对话模型：提取自 change（下方 match 会 move），
    // 热替换成功后即时切换会话模型（与 SetModel 等效），下回合即用新模型。
    let main_switch: Option<String> = match &change {
        ConfigChange::SetCapability { capability, alias }
            if capability == openslate_core::model_config::CAP_MAIN =>
        {
            Some(alias.clone())
        }
        _ => None,
    };
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
            ConfigChange::SetCapability { capability, alias } => {
                persist::set_capability(&active, &capability, &alias)
                    .map(|_| format!("capabilities.{capability} → {alias}"))
            }
            ConfigChange::UpsertMcpServer {
                name,
                url,
                headers,
            } => persist::upsert_mcp_server(&active, &name, &url, headers.as_ref())
                .map(|_| format!("MCP server {name}（重启会话后生效）")),
            ConfigChange::RemoveMcpServer { name } => {
                persist::remove_mcp_server(&active, &name).map(|_| format!("删除 MCP server {name}"))
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
        if let Some(alias) = main_switch {
            {
                let mut inner = state.core.lock();
                inner.model_alias = alias.clone();
            }
            state.sink.broadcast(ServerMsg::ModelChanged { alias });
        }
        // 删除的是被 levels / capabilities 引用的条目（松耦合放行）：
        // 落盘+热替换都成功后按悬空组提示改绑（两组都有时用分号连接
        // 两句）。此处 `what` 对 DeleteModel 即 "删除模型条目 {entry}"。
        let level_part = (!dangling.is_empty())
            .then(|| format!("代号 {} 现已悬空，请到「模型」页改绑", dangling.join("、")));
        let cap_part = (!cap_dangling.is_empty())
            .then(|| format!("功能 {} 现已悬空，请到「会话」页改绑", cap_dangling.join("、")));
        match (level_part, cap_part) {
            (Some(l), Some(c)) => {
                notice(state, conn_id, format!("已{what}；{l}；{c}"), NoticeLevel::Info)
            }
            (Some(l), None) => notice(state, conn_id, format!("已{what}；{l}"), NoticeLevel::Info),
            (None, Some(c)) => notice(state, conn_id, format!("已{what}；{c}"), NoticeLevel::Info),
            (None, None) => {}
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::SessionApprovalBridge;
    use crate::state::{ConfigPaths, CoreInner, MsgSink, SessionCore};
    use openslate_core::agent_tree::AgentTree;
    use openslate_core::config::{parse_openslate_toml, AgentsConfig};
    use openslate_core::types::{AgentConfig, AgentId};

    /// 最小可验证配置：zhipu + main(glm-5) / fast(glm-5-air)，自指
    /// levels，单 root agent —— `validate_config` 零 error（热替换路径
    /// 能走通）。
    const FIXTURE_TOML: &str = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "glm-5"

[models.fast]
provider = "zhipu"
model = "glm-5-air"

[levels]
main = "main"
fast = "fast"
"#;

    /// 录音型 [`MsgSink`]：把出站消息记进 vec 供断言（守卫 / 写回 /
    /// 广播路径全同步，无需队列）。
    #[derive(Default)]
    struct RecordingSink {
        sent: std::sync::Mutex<Vec<ServerMsg>>,
    }

    impl MsgSink for RecordingSink {
        fn broadcast(&self, msg: ServerMsg) {
            self.sent.lock().unwrap().push(msg);
        }
        fn send_to(&self, _conn_id: u64, msg: ServerMsg) -> bool {
            self.sent.lock().unwrap().push(msg);
            true
        }
        fn conn_ids(&self) -> Vec<u64> {
            vec![1]
        }
        fn count(&self) -> usize {
            1
        }
        fn close_all(&self) {}
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn error_notices(sink: &RecordingSink) -> Vec<String> {
        notices_at_level(sink, NoticeLevel::Error)
    }

    fn info_notices(sink: &RecordingSink) -> Vec<String> {
        notices_at_level(sink, NoticeLevel::Info)
    }

    fn notices_at_level(sink: &RecordingSink, level: NoticeLevel) -> Vec<String> {
        sink.sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|m| match m {
                ServerMsg::Notice { text, level: l } if *l == level => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// 装配最小 [`AppState`]：active 指向 tempdir 里的 toml（无全局库 →
    /// provider/model 条目写 active），内存 config 与磁盘同源。引擎/
    /// 审批均不装配（CRUD 路径不触及）。
    fn test_state(dir: &std::path::Path, toml: &str) -> (Arc<AppState>, Arc<RecordingSink>) {
        std::fs::write(dir.join("openslate.toml"), toml).expect("fixture write");
        let config = parse_openslate_toml(toml).expect("fixture parses");
        let agents = AgentsConfig {
            agents: vec![AgentConfig {
                id: AgentId("root".into()),
                name: "Root".into(),
                model: "main".into(),
                children: vec![],
                tools: vec![],
                default_prompt: "root prompt".into(),
            }],
        };
        let agent_tree = AgentTree::from_configs(&agents.agents).expect("tree builds");
        let sink = Arc::new(RecordingSink::default());
        let inner = CoreInner {
            session_id: "test-session".into(),
            session_label: "test".into(),
            history: Vec::new(),
            transcript: Vec::new(),
            running: false,
            compacting: false,
            depth_cur: 0,
            agents_running: 0,
            tool_calls_cur: 0,
            model_alias: "main".into(),
            cancel: None,
            config,
            agents_cfg: agents,
            agent_tree,
            skills: Vec::new(),
            store: None,
            session_run: None,
            manager: None,
            engine_task: None,
            paths: ConfigPaths {
                active: dir.join("openslate.toml"),
                global: None,
                local: None,
            },
            stats: Default::default(),
            stream: Default::default(),
            tool_timers: Default::default(),
            pending_step_meta: None,
        };
        let state = Arc::new(AppState {
            core: Arc::new(SessionCore::new(inner)),
            sink: sink.clone(),
            approval: Arc::new(SessionApprovalBridge::new(sink.clone())),
            auth_token: None,
            provider_factory: Arc::new(|_, _| Err(anyhow::anyhow!("unused in this test"))),
            origin: "test",
            _mcp: Default::default(),
        });
        (state, sink)
    }

    fn dto(provider: &str, model: &str) -> ModelDto {
        ModelDto {
            provider: provider.into(),
            model: model.into(),
            max_context_tokens: None,
            max_output_tokens: None,
            supports_tool_call: true,
            supports_vision: false,
            supports_reasoning: false,
            input_price_per_mtok: None,
            output_price_per_mtok: None,
        }
    }

    #[test]
    fn upsert_model_duplicate_provider_model_rejected() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (state, sink) = test_state(tmp.path(), FIXTURE_TOML);
        // entry "dup" 与既有 entry "main" 同为 (zhipu, glm-5) → 拒绝。
        let result = apply_config_change(
            &state,
            1,
            ConfigChange::UpsertModel {
                entry: "dup".into(),
                model: dto("zhipu", "glm-5"),
            },
        );
        assert!(
            result.is_none(),
            "duplicate (provider, model) under another entry must be rejected"
        );
        let notices = error_notices(&sink);
        assert!(
            notices
                .iter()
                .any(|t| t.contains("已存在模型 glm-5") && t.contains("条目 main")),
            "error notice names provider/model/conflicting entry: {notices:?}"
        );
        // 守卫先于落盘：磁盘不得出现新条目。
        let on_disk = std::fs::read_to_string(tmp.path().join("openslate.toml")).unwrap();
        assert!(
            !on_disk.contains("dup"),
            "guard must fire before persist: {on_disk}"
        );
    }

    #[test]
    fn upsert_model_same_entry_overwrite_allowed() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (state, sink) = test_state(tmp.path(), FIXTURE_TOML);
        // 同 entry 原值重写（查重跳过 key == entry）→ 放行。
        let identical = apply_config_change(
            &state,
            1,
            ConfigChange::UpsertModel {
                entry: "main".into(),
                model: dto("zhipu", "glm-5"),
            },
        );
        assert!(
            identical.is_some(),
            "same-entry overwrite must pass the dedup guard"
        );
        // 同 entry 换 model 值 → 放行、落盘、热替换。
        let updated = apply_config_change(
            &state,
            1,
            ConfigChange::UpsertModel {
                entry: "fast".into(),
                model: dto("zhipu", "glm-5-air-v3"),
            },
        );
        assert!(
            updated.is_some(),
            "same-entry update with a new model id must pass"
        );
        let on_disk = std::fs::read_to_string(tmp.path().join("openslate.toml")).unwrap();
        let reloaded = parse_openslate_toml(&on_disk).expect("re-parses");
        assert_eq!(
            reloaded.models.get("fast").map(|m| m.model.as_str()),
            Some("glm-5-air-v3"),
            "persist writes the new model id: {on_disk}"
        );
        assert_eq!(reloaded.models.len(), 2, "no new entries created: {on_disk}");
        // 热替换成功 → ConfigChanged 广播。
        assert!(
            sink.sent
                .lock()
                .unwrap()
                .iter()
                .any(|m| matches!(m, ServerMsg::ConfigChanged { .. })),
            "successful upsert broadcasts config_changed"
        );
        // 内存镜像同步热替换。
        let inner = state.core.lock();
        assert_eq!(
            inner.config.models.get("fast").map(|m| m.model.as_str()),
            Some("glm-5-air-v3"),
            "hot-swap must update the in-memory config mirror"
        );
    }

    /// 删模型一律放行（levels / capabilities 均松耦合）：被直绑 / 被代号
    /// 引用的条目删除成功，并按悬空组收到 Info notice。
    #[test]
    fn delete_model_dangling_references_allowed_and_noticed() {
        // extra 被 compact 直绑；premium 被 levels.deep 代号引用且被
        // title 直绑（双引用）；main 被 levels.main 自指。
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "glm-5"

[models.fast]
provider = "zhipu"
model = "glm-5-air"

[models.extra]
provider = "zhipu"
model = "glm-5-air"

[models.premium]
provider = "zhipu"
model = "glm-5-pro"

[levels]
main = "main"
fast = "fast"
deep = "premium"

[capabilities]
compact = "extra"
title = "premium"
"#;
        let tmp = tempfile::tempdir().expect("tmp");
        let (state, sink) = test_state(tmp.path(), toml);

        // ① 删被 capabilities 直绑的条目（用户命中的场景）→ 放行 +
        //    功能悬空 Info notice。
        let r = apply_config_change(
            &state,
            1,
            ConfigChange::DeleteModel {
                entry: "extra".into(),
            },
        );
        assert!(
            r.is_some(),
            "capabilities direct-bound entry deletion must pass"
        );
        let infos = info_notices(&sink);
        assert!(
            infos.iter().any(|t| t.contains("已删除模型条目 extra")
                && t.contains("功能 compact 现已悬空")
                && t.contains("会话")),
            "cap-only dangling notice: {infos:?}"
        );

        // ② 删被 levels 代号 + capabilities 双引用的条目 → 放行，两组
        //    悬空提示用分号拼进同一 notice。
        let r = apply_config_change(
            &state,
            1,
            ConfigChange::DeleteModel {
                entry: "premium".into(),
            },
        );
        assert!(r.is_some(), "level + capability referenced deletion must pass");
        let infos = info_notices(&sink);
        assert!(
            infos.iter().any(|t| t.contains("已删除模型条目 premium")
                && t.contains("代号 deep 现已悬空")
                && t.contains("功能 title 现已悬空")),
            "combined dangling notice carries both groups: {infos:?}"
        );

        // ③ 删被 levels 自指的条目 → 代号悬空 notice。
        let r = apply_config_change(
            &state,
            1,
            ConfigChange::DeleteModel {
                entry: "main".into(),
            },
        );
        assert!(r.is_some(), "level self-pointed entry deletion must pass");
        let infos = info_notices(&sink);
        assert!(
            infos
                .iter()
                .any(|t| t.contains("已删除模型条目 main") && t.contains("代号 main 现已悬空")),
            "level-only dangling notice: {infos:?}"
        );

        // 全程无拦截：不出现任何「先解除引用」Warn。
        assert!(
            !notices_at_level(&sink, NoticeLevel::Warn)
                .iter()
                .any(|t| t.contains("先解除引用")),
            "delete must never be intercepted"
        );

        // 磁盘终态：models 只剩 fast；capabilities/levels 映射留存
        //（悬空映射由用户决定改绑，persist 只删条目本身）。
        let on_disk = std::fs::read_to_string(tmp.path().join("openslate.toml")).unwrap();
        let reloaded = parse_openslate_toml(&on_disk).expect("re-parses");
        assert_eq!(reloaded.models.len(), 1, "only fast remains: {on_disk}");
        assert!(reloaded.models.contains_key("fast"));
        assert_eq!(
            reloaded.capabilities.get("compact").map(String::as_str),
            Some("extra"),
            "capabilities mapping survives (dangling by design): {on_disk}"
        );
        assert_eq!(
            reloaded.levels.get("deep").map(String::as_str),
            Some("premium"),
            "levels mapping survives (dangling by design): {on_disk}"
        );
    }
}
