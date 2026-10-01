//! 网关流式探针：用与 App 完全相同的 GenaiProvider 栈打目标网关，
//! 打印流事件序列——用于诊断「空回合」。
//!
//! ```sh
//! RUST_LOG=debug cargo run -p openslate-model-genai --example gw_probe -- \
//!   https://<gateway-host>:6443/api/anthropic anthropic glm-5.3-flash $KEY
//! ```

use openslate_model_genai::provider::{GenaiConfig, GenaiProvider};
use openslate_core::provider::ModelProvider;
use openslate_core::types::{Message, MessageRole};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // 抓 spawn 任务里的 panic（否则被 tokio 吞掉 → 零事件）。
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("!!! PANIC in task: {info}");
        if let Some(loc) = info.location() {
            eprintln!("    at {}:{}:{}", loc.file(), loc.line(), loc.column());
        }
        default_hook(info);
    }));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let args: Vec<String> = std::env::args().collect();
    let (base_url, adapter, model, key) = match args.as_slice() {
        [_, b, a, m, k] => (b.clone(), a.clone(), m.clone(), k.clone()),
        _ => anyhow::bail!("用法: gw_probe <base_url> <adapter> <model> <api_key>"),
    };

    let cfg = GenaiConfig {
        provider_name: "probe".into(),
        model: model.clone(),
        api_key: Some(key),
        base_url: Some(base_url),
        proxy: None,
        adapter: Some(adapter),
        timeout_ms: 60_000,
        max_attempts: 1,
        retry_base_ms: 500,
    };
    let provider = GenaiProvider::new(cfg)?;

    let req = openslate_core::provider::GenerateRequest {
        model_id: model.clone(),
        system_prompt: Some("You are a helpful assistant.".into()),
        messages: vec![Message {
            role: MessageRole::User,
            content: "用一句话介绍你自己".into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        }],
        tools: vec![],
        max_tokens: None,
        temperature: None,
    };

    println!("== generate_stream ==");
    let mut rx = provider.generate_stream(req).await;
    use openslate_core::types::ModelStreamEvent;
    let mut events = 0usize;
    while let Some(ev) = rx.recv().await {
        events += 1;
        match ev {
            Ok(ModelStreamEvent::Delta(t)) => println!("Delta({:?})", &t[..t.len().min(60)]),
            Ok(ModelStreamEvent::Reasoning(t)) => println!("Reasoning({} chars)", t.chars().count()),
            Ok(ModelStreamEvent::Usage(u)) => println!("Usage(↑{} ↓{})", u.input_tokens, u.output_tokens),
            Ok(ModelStreamEvent::Done(r)) => {
                println!(
                    "Done(content={:?}, tool_calls={}, finish={:?})",
                    r.content.as_deref().map(|c| &c[..c.len().min(80)]),
                    r.tool_calls.len(),
                    r.finish_reason
                );
            }
            Err(e) => println!("Err: {e}"),
        }
        if events > 200 { println!("(>200 events, stop)"); break; }
    }
    println!("== 共 {events} 个事件 ==");
    Ok(())
}
