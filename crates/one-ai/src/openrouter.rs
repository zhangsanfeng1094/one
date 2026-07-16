use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use one_core::agent::{CompletionRequest, CompletionResponse, LlmProvider};
use one_core::error::{OneError, Result};

use crate::compat::ResolvedOpenAiCompat;
use crate::openai::{OpenAiProvider, OpenaiWireApi};

const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-4";
const DEFAULT_BASE: &str = "https://openrouter.ai/api/v1";

pub struct OpenRouterProvider {
    inner: OpenAiProvider,
}

impl OpenRouterProvider {
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .map_err(|_| OneError::Provider("OPENROUTER_API_KEY is not set".to_string()))?;
        let model = std::env::var("OPENROUTER_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let base = std::env::var("OPENROUTER_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE.to_string());
        Ok(Self::with_base(api_key, model, base))
    }

    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_base(api_key, model, DEFAULT_BASE)
    }

    pub fn with_base(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        let model = model.into();
        let base_url = base_url.into();
        let compat = crate::compat::OpenAiCompletionsCompat::default().resolve(
            "openrouter",
            &base_url,
            &model,
        );
        Self {
            // OpenRouter speaks Chat Completions; detect sets thinkingFormat=openrouter.
            inner: OpenAiProvider::with_base(api_key, model, base_url)
                .with_wire_api(OpenaiWireApi::Completions)
                .with_provider_id("openrouter")
                .with_compat(compat)
                .with_reasoning_model(true),
        }
    }

    /// Override resolved OpenAI `compat` (from models.json merge + detect).
    pub fn with_compat(self, compat: ResolvedOpenAiCompat) -> Self {
        Self {
            inner: self.inner.with_compat(compat),
        }
    }

    /// Whether the selected model supports extended reasoning.
    pub fn with_reasoning_model(self, reasoning: bool) -> Self {
        Self {
            inner: self.inner.with_reasoning_model(reasoning),
        }
    }
}

#[async_trait]
impl LlmProvider for OpenRouterProvider {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        self.inner.complete(request).await
    }

    async fn complete_streaming(
        &self,
        request: CompletionRequest,
        on_event: &mut (dyn FnMut(one_core::streaming::StreamEvent) + Send),
        abort: Option<&AtomicBool>,
    ) -> Result<CompletionResponse> {
        self.inner.complete_streaming(request, on_event, abort).await
    }
}
