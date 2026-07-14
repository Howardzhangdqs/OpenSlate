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
    /// Called after a tool execution completes.
    fn on_tool_end(&mut self, name: &str, bytes: usize, truncated: bool);

    /// Called once a step is fully done — after the LLM response AND any tool
    /// calls it requested have executed. Default no-op. Used to emit a per-step
    /// stats line *below* the tool `-> .../<- ...` lines (rather than between
    /// the response and the tool calls, which is where on_request_end fires).
    fn on_step_end(&mut self) {}
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
