//! Child-agent 能力演示:root → researcher → verifier 三层递归委派。
//!
//! 用脚本化的 provider 确定性地驱动模型行为(无需 API key),展示
//! OpenSlate 的 subagent(child agent)核心能力:
//!   - 父 agent 通过 `call_agent` 工具委派子 agent
//!   - 子 agent 进一步委派孙 agent(真正的递归,非单层)
//!   - 结果逐层回传到根
//!   - 执行树在运行时增长(parent / depth / status 全程可见)
//!   - token 跨层累计
//!
//! 运行:`cargo run -p openslate-core --example child_agent_demo`

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use openslate_core::agent_tree::AgentTree;
use openslate_core::config::parse_openslate_toml;
use openslate_core::error::ProviderError;
use openslate_core::execution::ExecutionStatus;
use openslate_core::provider::{GenerateRequest, ModelProvider};
use openslate_core::run_manager::RunManager;
use openslate_core::skills::SkillsCatalog;
use openslate_core::tool::ToolRegistry;
use openslate_core::types::*;

/// 脚本化 provider:按顺序返回预设响应,并在每次被调用时打印一行,
/// 让递归调用序列对用户可见。
struct DemoProvider {
    steps: Vec<(&'static str, ModelResponse)>, // (人类可读标签, 模型响应)
    idx: AtomicUsize,
}

#[async_trait]
impl ModelProvider for DemoProvider {
    async fn generate(&self, _req: GenerateRequest) -> Result<ModelResponse, ProviderError> {
        let i = self.idx.fetch_add(1, Ordering::SeqCst);
        let (label, resp) = self
            .steps
            .get(i)
            .cloned()
            .ok_or(ProviderError::ServerError(500))?;
        println!("  [#{i}] {label}");
        Ok(resp)
    }
    fn provider_name(&self) -> &str {
        "demo"
    }
}

fn call_agent(tag: &str, child: &str, task: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId(format!("ca-{tag}")),
        name: "call_agent".into(),
        arguments: serde_json::json!({"agent_id": child, "task": task}),
    }
}

fn text(t: &str, usage: (u32, u32)) -> ModelResponse {
    ModelResponse {
        content: Some(t.into()),
        tool_calls: vec![],
        usage: Some(Usage {
            input_tokens: usage.0,
            output_tokens: usage.1,
            cached_input_tokens: None,
        }),
        finish_reason: Some("stop".into()),
    }
}

fn delegate(tag: &str, child: &str, task: &str, usage: (u32, u32)) -> ModelResponse {
    ModelResponse {
        content: None,
        tool_calls: vec![call_agent(tag, child, task)],
        usage: Some(Usage {
            input_tokens: usage.0,
            output_tokens: usage.1,
            cached_input_tokens: None,
        }),
        finish_reason: Some("tool_calls".into()),
    }
}

#[tokio::main]
async fn main() {
    println!("╔════════════════════════════════════════════════════════════╗");
    println!("║   OpenSlate Child Agent 能力演示                            ║");
    println!("║   root → researcher → verifier 三层递归委派                 ║");
    println!("╚════════════════════════════════════════════════════════════╝\n");

    // --- 配置:脚本化 mock provider,无需真实 API key ---
    let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"
[models.main]
provider = "mock"
model = "root-model"
[models.fast]
provider = "mock"
model = "fast-model"
"#;
    let config = parse_openslate_toml(toml).expect("config parses");

    // --- 静态 agent 树:root → researcher → verifier ---
    let agents = vec![
        AgentConfig {
            id: AgentId("root".into()),
            name: "Root 协调者".into(),
            model: "main".into(),
            children: vec![AgentId("researcher".into())],
            tools: vec![],
            default_prompt: "你是根协调者,把任务委派给 researcher".into(),
        },
        AgentConfig {
            id: AgentId("researcher".into()),
            name: "Researcher 研究员".into(),
            model: "fast".into(),
            children: vec![AgentId("verifier".into())],
            tools: vec![],
            default_prompt: "你是研究员,委托 verifier 验证后汇总".into(),
        },
        AgentConfig {
            id: AgentId("verifier".into()),
            name: "Verifier 验证员".into(),
            model: "fast".into(),
            children: vec![],
            tools: vec![],
            default_prompt: "你是验证员".into(),
        },
    ];
    let tree = AgentTree::from_configs(&agents).expect("tree builds");

    println!("【配置的静态 Agent 树】");
    println!("  root        (model: main)  ─┐");
    println!("  researcher  (model: fast)   ├─ root 的 child");
    println!("  verifier    (model: fast)   └─ researcher 的 child\n");

    // --- 脚本化模型响应:确定性驱动三层委派(1 根委派 + 1 子委派 + 3 个回答/汇总)---
    let provider = DemoProvider {
        steps: vec![
            (
                "root        ⇒ 委派 call_agent(researcher, '研究 2+2')",
                delegate("1", "researcher", "研究 2+2", (30, 10)),
            ),
            (
                "researcher  ⇒ 委派 call_agent(verifier, '验证 2+2=4')",
                delegate("2", "verifier", "验证 2+2=4", (25, 12)),
            ),
            (
                "verifier    ⇐ 回答:验证通过,2+2=4",
                text("验证通过:2+2=4", (15, 8)),
            ),
            (
                "researcher  ⇐ 汇总子结果:'研究完成:经验证 2+2=4'",
                text("研究完成:经验证 2+2=4", (35, 14)),
            ),
            (
                "root        ⇐ 汇总子结果:'最终答案:4'",
                text("最终答案:4", (40, 6)),
            ),
        ],
        idx: AtomicUsize::new(0),
    };

    println!("【运行中:模型调用序列(每行 = 某一层 agent 的一次推理)】");
    let manager = RunManager::new(config, tree, ToolRegistry::new(), SkillsCatalog::default());
    let result = manager
        .execute(&provider, "计算 2+2", None)
        .await
        .expect("run completes");
    println!();

    // --- 最终答案(回到用户的最后一条 assistant 消息)---
    let final_text = result
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Assistant && !m.content.is_empty())
        .map(|m| m.content.clone())
        .unwrap_or_default();
    println!("【最终答案(逐层回传到用户)】");
    println!("  {final_text}\n");

    // --- 执行树:运行时实际产生的节点(根 + 每次委派产生的子节点)---
    println!("【运行时执行树(AgentRunner 递归产生)】");
    let mut nodes: Vec<_> = result.execution_tree.all_nodes();
    nodes.sort_by_key(|n| (n.depth, n.agent_id.0.clone()));
    for n in &nodes {
        let origin = if n.parent_execution_id.is_some() {
            "← 由父 agent 的 call_agent 委派产生"
        } else {
            "(根,用户入口)"
        };
        let st = match n.status {
            ExecutionStatus::Running => "Running",
            ExecutionStatus::Completed => "Completed",
            ExecutionStatus::Failed => "Failed",
        };
        println!(
            "  depth {}  {:<12}  {:<10}  {}",
            n.depth, n.agent_id.0, st, origin
        );
    }
    println!();

    // --- 统计 ---
    let delegations = result.execution_tree.node_count() - 1;
    println!("【统计】");
    println!(
        "  执行节点总数 : {}  (1 个根 + {delegations} 次委派)",
        result.execution_tree.node_count()
    );
    println!(
        "  input tokens : {}  (跨 root/researcher/verifier 三层累计)",
        result.total_input_tokens
    );
    println!(
        "  output tokens: {}  (跨三层累计)",
        result.total_output_tokens
    );
    println!("\n演示完成。递归委派 / 执行树增长 / 结果逐层回传 / token 跨层累计 均已展示。");
}
