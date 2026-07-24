use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use one_core::agent::{
    Agent, AgentConfig, Citation, CompletionRequest, CompletionResponse, LlmProvider, ServerTool,
    TokenUsage,
};
use one_core::events::AgentEvent;
use one_core::message::{AgentMessage, ContentBlock, StopReason};
use one_core::streaming::{ServerToolStatus, StreamEvent};
use one_core::ThinkingLevel;

#[test]
fn completion_request_keeps_server_tools_separate_from_local_tools() {
    let request = CompletionRequest {
        system_prompt: String::new(),
        messages: vec![AgentMessage::user_text("latest rust release")],
        tools: Vec::new(),
        server_tools: vec![ServerTool::WebSearch, ServerTool::XSearch],
        thinking_level: ThinkingLevel::Off,
    };

    assert!(request.tools.is_empty());
    assert_eq!(
        request.server_tools,
        vec![ServerTool::WebSearch, ServerTool::XSearch]
    );
}

struct SearchProvider {
    seen: Mutex<Vec<ServerTool>>,
}

#[async_trait]
impl LlmProvider for SearchProvider {
    fn name(&self) -> &str {
        "test"
    }

    fn model(&self) -> &str {
        "gpt-test"
    }

    fn server_tools(&self) -> Vec<ServerTool> {
        vec![ServerTool::WebSearch]
    }

    async fn complete(&self, request: CompletionRequest) -> one_core::Result<CompletionResponse> {
        *self.seen.lock().unwrap() = request.server_tools;
        Ok(CompletionResponse {
            provider: self.name().into(),
            model: self.model().into(),
            content: vec![ContentBlock::text("done")],
            stop_reason: StopReason::Stop,
            usage: TokenUsage::default(),
            citations: vec![Citation {
                url: "https://example.com/rust".into(),
                title: "Rust".into(),
                start_index: 0,
                end_index: 4,
            }],
        })
    }

    async fn complete_streaming(
        &self,
        request: CompletionRequest,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
        _abort: Option<&std::sync::atomic::AtomicBool>,
    ) -> one_core::Result<CompletionResponse> {
        on_event(StreamEvent::ServerTool {
            tool: ServerTool::WebSearch,
            status: ServerToolStatus::Started,
        });
        on_event(StreamEvent::ServerTool {
            tool: ServerTool::WebSearch,
            status: ServerToolStatus::Completed,
        });
        self.complete(request).await
    }
}

#[tokio::test]
async fn agent_attaches_provider_server_tools_only_when_enabled() {
    let provider = SearchProvider {
        seen: Mutex::new(Vec::new()),
    };
    let mut agent = Agent::new(
        AgentConfig {
            server_search: true,
            ..AgentConfig::default()
        },
        Vec::new(),
    );

    let events = Arc::new(Mutex::new(Vec::new()));
    let events_copy = events.clone();
    agent.subscribe(Box::new(move |event| {
        if matches!(event, AgentEvent::ServerTool { .. }) {
            events_copy.lock().unwrap().push(event.clone());
        }
    }));

    agent.prompt(&provider, "search").await.unwrap();
    assert_eq!(*provider.seen.lock().unwrap(), vec![ServerTool::WebSearch]);
    assert_eq!(events.lock().unwrap().len(), 2);
    let citations = agent
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            AgentMessage::Assistant(message) => Some(message.citations.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(citations[0].url, "https://example.com/rust");
}

#[test]
fn old_assistant_messages_default_to_no_citations() {
    let mut value =
        serde_json::to_value(AgentMessage::assistant_text("openai", "gpt-5", "hello")).unwrap();
    value.as_object_mut().unwrap().remove("citations");
    let message: AgentMessage = serde_json::from_value(value).unwrap();
    let AgentMessage::Assistant(message) = message else {
        panic!("assistant")
    };
    assert!(message.citations.is_empty());
}
