//! PTC (Programmatic Tool Calling) 能力演示:模型用一段代码编排工具调用。
//!
//! 用脚本化的 provider 确定性地驱动模型行为(无需 API key),展示 PTC 的
//! P2 能力(PTC_PLAN.md §5):
//!   - 命名空间组合路径:MCP 风格工具 `github_list_prs` 在沙箱内以
//!     `tools.github.list_prs(...)` 暴露(dispatch 仍走完整注册名)
//!   - 三档披露(disclosure=auto):run_code 描述里注入 TypeScript 声明
//!   - 沙箱内建 `list_tools(pattern)` / `describe_tool(name)` 渐进披露
//!     (不走 bridge、不占 tool-call 预算)
//!   - `github_* = "ptc"` 模式:该工具从模型直调列表隐藏,仅代码可调
//!
//! 运行:`cargo run -p openslate-core --example ptc_demo`

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use openslate_core::agent_tree::AgentTree;
use openslate_core::config::parse_openslate_toml;
use openslate_core::error::{ProviderError, ToolError};
use openslate_core::provider::{GenerateRequest, ModelProvider};
use openslate_core::run_manager::RunManager;
use openslate_core::skills::SkillsCatalog;
use openslate_core::tool::{Tool, ToolRegistry};
use openslate_core::types::*;

/// 脚本化 provider:按顺序返回预设响应;第一次调用时顺带打印模型视角的
/// 工具列表与 run_code 描述片段,让「模型看到了什么」对用户可见。
struct DemoProvider {
    steps: Vec<(&'static str, ModelResponse)>, // (人类可读标签, 模型响应)
    idx: AtomicUsize,
}

#[async_trait]
impl ModelProvider for DemoProvider {
    async fn generate(&self, req: GenerateRequest) -> Result<ModelResponse, ProviderError> {
        let i = self.idx.fetch_add(1, Ordering::SeqCst);
        if i == 0 {
            let names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
            println!("  模型可见工具: [{}]", names.join(", "));
            if let Some(rc) = req.tools.iter().find(|t| t.name == "run_code") {
                let snippet: String = rc.description.chars().take(420).collect();
                println!(
                    "  run_code 描述(前 420 字符):\n    {}",
                    snippet.replace('\n', "\n    ")
                );
            }
            println!();
        }
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

/// 平铺内置风格工具:沙箱路径 `tools.get_version()`。
struct GetVersionTool;

#[async_trait]
impl Tool for GetVersionTool {
    fn name(&self) -> &str {
        "get_version"
    }
    fn description(&self) -> &str {
        "Get the demo build version."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: "v0.2.0-ptc-demo".into(),
            bytes: 0,
            duration_ms: 0,
            status: ToolOutputStatus::Success,
        })
    }
}

/// MCP 风格命名空间工具:注册名 `github_list_prs`,沙箱路径
/// `tools.github.list_prs(...)`(namespace 由 `Tool::namespace` 提供)。
struct GithubListPrsTool;

#[async_trait]
impl Tool for GithubListPrsTool {
    fn name(&self) -> &str {
        "github_list_prs"
    }
    fn namespace(&self) -> Option<String> {
        Some("github".to_string())
    }
    fn description(&self) -> &str {
        "List pull requests. Filter by state."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "state": {"type": "string", "enum": ["open", "closed", "all"]},
                "limit": {"type": "integer"}
            }
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        let state = args["state"].as_str().unwrap_or("all");
        Ok(ToolOutput {
            content: format!("state={state}, open_prs=3, recent=[#101, #97, #88]"),
            bytes: 0,
            duration_ms: 0,
            status: ToolOutputStatus::Success,
        })
    }
}

/// 模型发出的 run_code 代码:① 查目录;② 查单个工具签名;③ 组合路径调用
/// 命名空间工具;④ 调用平铺工具;⑤ 返回聚合结果。
const DEMO_CODE: &str = r#"async () => {
  const catalog = list_tools("*");
  const spec = describe_tool("github.list_prs").split("\n")[0];
  const prs = await tools.github.list_prs({ state: "open" });
  const version = await tools.get_version();
  return { catalog, spec, prs, version };
}"#;

fn run_code_call() -> ToolCall {
    ToolCall {
        id: ToolCallId("rc-1".into()),
        name: "run_code".into(),
        arguments: serde_json::json!({ "code": DEMO_CODE }),
    }
}

fn text(t: &str) -> ModelResponse {
    ModelResponse {
        content: Some(t.into()),
        tool_calls: vec![],
        reasoning_content: None,
        usage: None,
        finish_reason: Some("stop".into()),
    }
}

#[tokio::main]
async fn main() {
    println!("╔════════════════════════════════════════════════════════════╗");
    println!("║   OpenSlate PTC (Programmatic Tool Calling) 能力演示       ║");
    println!("║   命名空间 · 三档披露 · 沙箱内工具发现                      ║");
    println!("╚════════════════════════════════════════════════════════════╝\n");

    // --- 配置:脚本化 mock provider + PTC 开启;github_* 设为 ptc-only,
    //     展示「直调列表隐藏、代码可调」的模式过滤。 ---
    let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"
[models.main]
provider = "mock"
model = "demo-model"

[ptc]
enabled = true

[ptc.tool_modes]
"github_*" = "ptc"
"#;
    let config = parse_openslate_toml(toml).expect("config parses");

    // --- 单 agent 树 + 两个 mock 工具(一个平铺、一个命名空间) ---
    let agents = vec![AgentConfig {
        id: AgentId("root".into()),
        name: "Root".into(),
        model: "main".into(),
        children: vec![],
        tools: vec![],
        default_prompt: "你是根 agent,用 run_code 编排工具调用".into(),
    }];
    let tree = AgentTree::from_configs(&agents).expect("tree builds");

    let mut registry = ToolRegistry::new();
    registry.register(GetVersionTool);
    registry.register(GithubListPrsTool);

    println!("【模型视角(第 1 次请求捕获)】");
    let provider = DemoProvider {
        steps: vec![
            (
                "模型 ⇒ 发出 run_code:组合路径调用 + 目录查询",
                ModelResponse {
                    content: None,
                    tool_calls: vec![run_code_call()],
                    reasoning_content: None,
                    usage: None,
                    finish_reason: Some("tool_calls".into()),
                },
            ),
            (
                "模型 ⇐ 汇总代码执行结果,给出最终答案",
                text("代码模式完成:3 个 open PR,demo 版本 v0.2.0-ptc-demo。"),
            ),
        ],
        idx: AtomicUsize::new(0),
    };

    println!("【运行中】");
    let manager = RunManager::new(config, tree, registry, SkillsCatalog::default());
    let result = manager
        .execute(&provider, "汇总 open PR 并报告版本", None)
        .await
        .expect("run completes");
    println!();

    // --- run_code 的 tool result:沙箱真实执行的 [result] 聚合输出 ---
    println!("【run_code 工具结果(回给模型的 observation)】");
    let tool_msg = result
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .expect("run_code tool message");
    println!("  {}", tool_msg.content.replace('\n', "\n  "));
    println!();

    // --- 最终答案 ---
    let final_text = result
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Assistant && !m.content.is_empty())
        .map(|m| m.content.clone())
        .unwrap_or_default();
    println!("【最终答案】");
    println!("  {final_text}\n");

    // --- 统计 ---
    println!("【统计】");
    println!("  状态        : {:?}", result.status);
    println!("  steps       : {}", result.total_steps);
    println!(
        "  说明        : github_list_prs 不在直调列表(ptc-only),但在代码中 \
         以 tools.github.list_prs 组合路径可调;list_tools/describe_tool \
         为沙箱内建查询,未消耗 tool-call 预算"
    );

    println!("\nPTC demo completed");
}
