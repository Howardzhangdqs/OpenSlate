use crate::error::ProviderError;
use crate::types::{Message, ModelResponse, ModelStreamEvent, Usage};

#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct GenerateRequest {
    pub model_id: String,
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    async fn generate(&self, request: GenerateRequest) -> Result<ModelResponse, ProviderError>;

    /// Stream a chat completion request.
    ///
    /// Returns a receiver that yields `ModelStreamEvent`s as they arrive.
    /// The default implementation wraps `generate()` and returns a single
    /// `Done` event (no real-time streaming).
    async fn generate_stream(
        &self,
        request: GenerateRequest,
    ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let result = self.generate(request).await;
        match result {
            Ok(response) => {
                if let Some(usage) = response.usage {
                    let _ = tx.send(Ok(ModelStreamEvent::Usage(usage))).await;
                }
                let _ = tx.send(Ok(ModelStreamEvent::Done(response))).await;
            }
            Err(e) => {
                let _ = tx.send(Err(e)).await;
            }
        }
        rx
    }

    fn provider_name(&self) -> &str;
}

/// Callback trait for real-time progress updates during agent execution.
///
/// The runtime calls these methods at key points during execution.
/// The CLI implements this to update the spinner display.
pub trait ProgressCallback: Send {
    /// Called before a model request is sent (step number, model ID).
    fn on_request_start(&mut self, step: u32, model_id: &str);
    /// Called with an early estimate of the input-token count for the request,
    /// so the UI can show `↑N` during streaming before the provider's real
    /// usage arrives. Default is a no-op.
    fn on_input_estimate(&mut self, _tokens: u32) {}
    /// Called when the first content token arrives (for TTFT measurement).
    fn on_first_token(&mut self);
    /// Called for each content delta from the model.
    fn on_delta(&mut self, text: &str);
    /// Called for each reasoning/thinking delta from the model.
    ///
    /// Default is a no-op so existing implementations keep compiling. CLI
    /// implementations override this to stream the model's chain-of-thought
    /// (e.g. dimmed, above the spinner).
    fn on_reasoning(&mut self, _text: &str) {}
    /// Called when token usage info arrives.
    fn on_usage(&mut self, usage: Usage);
    /// Called after the model response is fully received.
    fn on_request_end(&mut self);
    /// Called before a tool is executed.
    fn on_tool_start(&mut self, name: &str, args: &str);
    /// Called after a tool execution completes. `preview` 携带输出前若干
    /// 字符（供 UI 投影；可能为空串——工具无输出时）。
    fn on_tool_end(&mut self, name: &str, bytes: usize, truncated: bool, preview: &str);

    /// Called once a step is fully done — after the LLM response AND any tool
    /// calls it requested have executed. Default no-op. Used to emit a per-step
    /// stats line *below* the tool `-> .../<- ...` lines (rather than between
    /// the response and the tool calls, which is where on_request_end fires).
    fn on_step_end(&mut self) {}
}

// ── 远端模型清单拉取（Provider 设置页"自动检测"）────────────────────
//
// adapter 感知：不同协议族的 /models 端点与鉴权头不同。响应解析刻意
// 宽松（data[].id / models[].name / models[].id 都认），只提取 id 列表。

/// 拉取 provider 的可用模型清单。`adapter` 为空按 "openai" 处理
/// （与 `ProviderConfig::adapter` 的缺省语义一致）。10s 超时。
///
/// 端点/鉴权矩阵：
/// - `openai`（含所有兼容端点，如 zhipu）：`GET {base}/models`，`Authorization: Bearer`
/// - `anthropic`：`GET {base}/v1/models`，`x-api-key` + `anthropic-version`
/// - `gemini`：`GET {base}/v1beta/models`，`x-goog-api-key`
/// - `ollama`：`GET {base}/api/tags`，无鉴权
pub async fn list_remote_models(
    base_url: &str,
    adapter: &str,
    api_key: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let base = base_url.trim_end_matches('/');
    let adapter = if adapter.is_empty() { "openai" } else { adapter };
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("openslate/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(10));
    // Android：rustls-platform-verifier 需要 JNI 宿主初始化（纯 .so 场景
    // 不可用）——与 model-genai 同款 webpki 静态根证书预配置 TLS。
    #[cfg(target_os = "android")]
    {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let tls = rustls::ClientConfig::builder_with_provider(provider.into())
            .with_safe_default_protocol_versions()
            .map_err(|e| anyhow::anyhow!("tls config: {e}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        builder = builder.use_preconfigured_tls(tls);
    }
    // 与 provider 工厂同款代理语义：OPENSLATE_HTTP_PROXY 显式启用，
    // 否则 no_proxy（desktop 默认直连）。
    match std::env::var("OPENSLATE_HTTP_PROXY").ok().filter(|u| !u.is_empty()) {
        Some(proxy_url) => {
            let proxy = reqwest::Proxy::all(&proxy_url)
                .map_err(|e| anyhow::anyhow!("invalid proxy url '{proxy_url}': {e}"))?;
            builder = builder.proxy(proxy);
        }
        None => builder = builder.no_proxy(),
    }
    let mut req = builder.build()?.get(match adapter {
            // base 带版本后缀时不再重复拼（anthropic 代理常配成
            // "…/v1"：直接拼 /v1/models 会变成 /v1/v1/models → 404）。
            "anthropic" => {
                if base.ends_with("/v1") {
                    format!("{base}/models")
                } else {
                    format!("{base}/v1/models")
                }
            }
            "gemini" => {
                let b = base.strip_suffix("/v1beta").unwrap_or(base);
                format!("{b}/v1beta/models")
            }
            "ollama" => format!("{base}/api/tags"),
            _ => format!("{base}/models"),
        });
    match adapter {
        "anthropic" => {
            req = req.header("anthropic-version", "2023-06-01");
            if let Some(k) = api_key {
                req = req.header("x-api-key", k);
            }
        }
        "gemini" => {
            if let Some(k) = api_key {
                req = req.header("x-goog-api-key", k);
            }
        }
        "ollama" => {}
        _ => {
            if let Some(k) = api_key {
                req = req.header("Authorization", format!("Bearer {k}"));
            }
        }
    }
    let body: serde_json::Value = req.send().await?.error_for_status()?.json().await?;
    let mut ids: Vec<String> = Vec::new();
    // OpenAI/Anthropic: {"data":[{"id":...}]}；Gemini: {"models":[{"name":
    // "models/xxx"}]}；Ollama /api/tags: {"models":[{"name":...}]}。逐个
    // 探测，兼容各家变体与代理实现。
    if let Some(arr) = body.get("data").and_then(|v| v.as_array()) {
        for item in arr {
            if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                ids.push(id.to_owned());
            }
        }
    }
    if let Some(arr) = body.get("models").and_then(|v| v.as_array()) {
        for item in arr {
            let name = item
                .get("name")
                .or_else(|| item.get("id"))
                .and_then(|v| v.as_str());
            if let Some(id) = name {
                // Gemini 的 name 带 "models/" 前缀，剥掉。
                ids.push(id.strip_prefix("models/").unwrap_or(id).to_owned());
            }
        }
    }
    if ids.is_empty() {
        anyhow::bail!(
            "models 响应里没有可识别的模型条目（adapter={adapter}，响应片段：{}）",
            serde_json::to_string(&body).unwrap_or_default().chars().take(200).collect::<String>()
        );
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition_construction() {
        let td = ToolDefinition {
            name: "bash".into(),
            description: "Run a shell command".into(),
            parameters: serde_json::json!({"type": "object"}),
        };
        assert_eq!(td.name, "bash");
    }

    #[test]
    fn generate_request_construction() {
        let req = GenerateRequest {
            model_id: "gpt-4".into(),
            system_prompt: Some("You are helpful.".into()),
            messages: vec![],
            tools: vec![],
            max_tokens: Some(100),
            temperature: Some(0.7),
        };
        assert_eq!(req.model_id, "gpt-4");
        assert_eq!(req.max_tokens, Some(100));
    }

    struct DummyProvider;

    #[async_trait::async_trait]
    impl ModelProvider for DummyProvider {
        async fn generate(
            &self,
            _request: GenerateRequest,
        ) -> Result<ModelResponse, ProviderError> {
            Ok(ModelResponse {
                content: Some("dummy".into()),
                tool_calls: vec![],
                reasoning_content: None,
                usage: None,
                finish_reason: Some("stop".into()),
            })
        }

        fn provider_name(&self) -> &str {
            "dummy"
        }
    }

    #[tokio::test]
    async fn trait_object_dispatch() {
        let provider: Box<dyn ModelProvider> = Box::new(DummyProvider);
        let req = GenerateRequest {
            model_id: "m".into(),
            system_prompt: None,
            messages: vec![],
            tools: vec![],
            max_tokens: None,
            temperature: None,
        };
        let result = provider.generate(req).await.unwrap();
        assert_eq!(result.content.as_deref(), Some("dummy"));
        assert_eq!(provider.provider_name(), "dummy");
    }
}
