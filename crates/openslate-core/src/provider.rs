use crate::error::ProviderError;
use crate::types::{Message, ModelResponse};

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

    fn provider_name(&self) -> &str;
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
