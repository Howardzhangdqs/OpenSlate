//! v5 L2 端到端：ManagedRunResult token 聚合 → `RunFinal` →
//! `runs.input_tokens/output_tokens` 聚合列（文件库断言），外加
//! assistant 消息 meta 经 sink 落 `meta_json` 的链路验证。
//!
//! 契约（docs/PLAN-storage-v5-multimodal.md §2）：
//! - run 行的 token 聚合 = 该 run 生命周期内全部回合的
//!   `ManagedRunResult` 累计（调用方聚合，`finish_with` 单事务落库）；
//! - assistant 消息的采集元数据（model_id / usage / finish_reason /
//!   latency_ms）经 `MessageSink::append_with_meta` 旁路落库，
//!   user 行无 meta。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use openslate_core::agent_tree::AgentTree;
use openslate_core::config::parse_openslate_toml;
use openslate_core::error::ProviderError;
use openslate_core::provider::{GenerateRequest, ModelProvider};
use openslate_core::run_manager::RunManager;
use openslate_core::runtime::{CancellationToken, MessageSink};
use openslate_core::skills::SkillsCatalog;
use openslate_core::tool::ToolRegistry;
use openslate_core::types::{
    AgentConfig, AgentId, Message, MessageMeta, MessageRole, ModelResponse, RunId, Usage,
};
use openslate_store_sqlite::recorder::{RunFinal, RunRecorder};
use openslate_store_sqlite::store::SqliteStore;

/// 脚本化 provider：每次调用按序回放响应（两回合，各带 usage）。
struct ScriptedProvider {
    responses: Vec<ModelResponse>,
    call_count: AtomicUsize,
}

impl ScriptedProvider {
    fn new() -> Self {
        Self {
            responses: vec![
                // 回合 1：直接回答。
                ModelResponse {
                    content: Some("first answer".into()),
                    tool_calls: vec![],
                    reasoning_content: None,
                    usage: Some(Usage {
                        input_tokens: 100,
                        output_tokens: 20,
                        cached_input_tokens: None,
                        reasoning_tokens: None,
                        image_tokens: None,
                    }),
                    finish_reason: Some("stop".into()),
                },
                // 回合 2：直接回答。
                ModelResponse {
                    content: Some("second answer".into()),
                    tool_calls: vec![],
                    reasoning_content: None,
                    usage: Some(Usage {
                        input_tokens: 150,
                        output_tokens: 5,
                        cached_input_tokens: None,
                        reasoning_tokens: None,
                        image_tokens: None,
                    }),
                    finish_reason: Some("stop".into()),
                },
            ],
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for ScriptedProvider {
    async fn generate(
        &self,
        _request: GenerateRequest,
    ) -> Result<ModelResponse, ProviderError> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        self.responses
            .get(idx)
            .cloned()
            .ok_or(ProviderError::ServerError(500))
    }

    fn provider_name(&self) -> &str {
        "scripted"
    }
}

fn user_msg(content: &str) -> Message {
    Message {
        attachments: None,
        role: MessageRole::User,
        content: content.to_owned(),
        tool_call_id: None,
        name: None,
        tool_calls: None,
        reasoning_content: None,
    }
}

#[tokio::test]
async fn finish_with_persists_run_lifetime_token_aggregation() {
    // 文件库（真迁移路径）。
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("runfinal.sqlite");
    let store = SqliteStore::new(db_path.to_str().expect("utf-8"))
        .await
        .expect("open store");
    store.run_migrations().await.expect("migrations");

    let config = parse_openslate_toml(
        r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "m"
"#,
    )
    .expect("config parses");

    let agents = vec![AgentConfig {
        hidden_tools: Vec::new(),
        id: AgentId("root".into()),
        name: "Root".into(),
        model: "main".into(),
        children: vec![],
        tools: vec![],
        default_prompt: "root prompt".into(),
    }];
    let agent_tree = AgentTree::from_configs(&agents).expect("tree builds");

    let run_id = RunId("run-final-agg".into());
    let recorder = Arc::new(
        RunRecorder::begin(
            store.clone(),
            run_id.clone(),
            "root",
            Some("agg test"),
            "{}",
        )
        .await
        .expect("begin"),
    );

    let mut manager = RunManager::new(config, agent_tree, ToolRegistry::new(), SkillsCatalog::default());
    manager.message_sink = Some(recorder.clone() as Arc<dyn MessageSink>);

    let provider = ScriptedProvider::new();

    // 回合 1（user 消息由前端层显式落库——session/REPL 的既有形态；
    // runtime sink 只负责 assistant/tool 消息）。
    recorder.write_message(&user_msg("turn one")).await.expect("persist u1");
    let r1 = manager
        .execute_with_run_id(
            run_id.clone(),
            &provider,
            &[user_msg("turn one")],
            CancellationToken::new(),
            None,
        )
        .await
        .expect("turn 1");

    // 回合 2：延续同一 run（会话多回合语义），历史带上回合 1 全量。
    let mut history2 = r1.messages.clone();
    history2.push(user_msg("turn two"));
    recorder
        .write_message(history2.last().expect("user msg"))
        .await
        .expect("persist u2");
    let r2 = manager
        .execute_with_run_id(
            run_id.clone(),
            &provider,
            &history2,
            CancellationToken::new(),
            None,
        )
        .await
        .expect("turn 2");

    // 调用方聚合（session/REPL 的既有 usage 累计路径同型）。
    let total_in = r1.total_input_tokens + r2.total_input_tokens;
    let total_out = r1.total_output_tokens + r2.total_output_tokens;
    let total_cost = r1.total_cost_usd + r2.total_cost_usd;
    assert_eq!(total_in, 250, "100 + 150");
    assert_eq!(total_out, 25, "20 + 5");

    recorder
        .finish_with(RunFinal {
            status: "completed".into(),
            output_json: None,
            cost_usd: total_cost,
            input_tokens: total_in,
            output_tokens: total_out,
        })
        .await
        .expect("finish_with");

    // runs 聚合列 = run 生命周期累计。
    let run = store.get_run("run-final-agg").await.expect("get").expect("row");
    assert_eq!(run.status, "completed");
    assert_eq!(run.input_tokens, 250);
    assert_eq!(run.output_tokens, 25);

    // 旁路链路：assistant 行带 meta（usage/model_id），user 行 NULL。
    let records = store
        .list_messages_by_run("run-final-agg")
        .await
        .expect("messages");
    let roles: Vec<(&str, bool)> = records
        .iter()
        .map(|r| (r.role.as_str(), r.meta_json.is_some()))
        .collect();
    assert_eq!(
        roles,
        vec![
            ("user", false),
            ("assistant", true),
            ("user", false),
            ("assistant", true),
        ],
        "user 行无 meta，assistant 行有 meta: {roles:?}"
    );
    let meta: MessageMeta =
        serde_json::from_str(records[1].meta_json.as_deref().expect("meta json"))
            .expect("meta deserializes");
    assert_eq!(meta.model_id.as_deref(), Some("m"));
    assert_eq!(meta.usage.expect("usage").input_tokens, 100);
    assert_eq!(meta.finish_reason.as_deref(), Some("stop"));
    assert!(meta.latency_ms.is_some());
}
