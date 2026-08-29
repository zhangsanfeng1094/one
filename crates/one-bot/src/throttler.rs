use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tracing::debug;

use crate::adapters::PlatformAdapter;
use crate::events::{BotOutboundMessage, MessageHandle, MessageTarget};

/// Adaptive Stream Throttler to handle IM rate limits when streaming LLM responses.
pub struct StreamThrottler {
    adapter: Arc<dyn PlatformAdapter>,
    target: MessageTarget,
    handle: Mutex<Option<MessageHandle>>,
    current_text: Mutex<String>,
    current_thinking: Mutex<Option<String>>,
    current_tool_status: Mutex<Option<String>>,
    last_flush: Mutex<Instant>,
    min_interval: Duration,
    dirty: Mutex<bool>,
}

impl StreamThrottler {
    pub fn new(
        adapter: Arc<dyn PlatformAdapter>,
        target: MessageTarget,
        min_interval_ms: u64,
    ) -> Self {
        Self {
            adapter,
            target,
            handle: Mutex::new(None),
            current_text: Mutex::new(String::new()),
            current_thinking: Mutex::new(None),
            current_tool_status: Mutex::new(None),
            last_flush: Mutex::new(Instant::now()),
            min_interval: Duration::from_millis(min_interval_ms.max(500)),
            dirty: Mutex::new(false),
        }
    }

    /// Append text tokens.
    pub async fn push_text_delta(&self, delta: &str) {
        {
            let mut text = self.current_text.lock().await;
            text.push_str(delta);
            let mut dirty = self.dirty.lock().await;
            *dirty = true;
        }
        self.maybe_flush().await;
    }

    /// Append thinking tokens.
    pub async fn push_thinking_delta(&self, delta: &str) {
        {
            let mut thinking = self.current_thinking.lock().await;
            if let Some(t) = thinking.as_mut() {
                t.push_str(delta);
            } else {
                *thinking = Some(delta.to_string());
            }
            let mut dirty = self.dirty.lock().await;
            *dirty = true;
        }
        self.maybe_flush().await;
    }

    /// Update tool execution status.
    pub async fn set_tool_status(&self, status: Option<String>) {
        {
            let mut tool = self.current_tool_status.lock().await;
            *tool = status;
            let mut dirty = self.dirty.lock().await;
            *dirty = true;
        }
        self.maybe_flush().await;
    }

    /// Check if interval elapsed and flush updates to IM platform.
    async fn maybe_flush(&self) {
        let now = Instant::now();
        let should_flush = {
            let last = self.last_flush.lock().await;
            let dirty = self.dirty.lock().await;
            *dirty && now.duration_since(*last) >= self.min_interval
        };

        if should_flush {
            self.flush_internal(false).await;
        }
    }

    /// Flush all buffered output to IM platform and mark finalized.
    pub async fn finish(&self) {
        self.flush_internal(true).await;
    }

    async fn flush_internal(&self, is_final: bool) {
        let msg = {
            let text = self.current_text.lock().await.clone();
            let thinking = self.current_thinking.lock().await.clone();
            let tool_status = self.current_tool_status.lock().await.clone();
            BotOutboundMessage {
                text,
                thinking,
                tool_status,
                buttons: Vec::new(),
                is_final,
            }
        };

        let mut handle_guard = self.handle.lock().await;
        if let Some(handle) = &*handle_guard {
            if let Err(e) = self.adapter.edit_message(handle, &msg).await {
                debug!("Stream throttler edit_message error: {}", e);
            }
        } else {
            // First time sending message
            match self.adapter.send_message(&self.target, &msg).await {
                Ok(h) => {
                    *handle_guard = Some(h);
                }
                Err(e) => {
                    debug!("Stream throttler send_message error: {}", e);
                }
            }
        }

        let mut last = self.last_flush.lock().await;
        *last = Instant::now();
        let mut dirty = self.dirty.lock().await;
        *dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::Result;
    use async_trait::async_trait;
    use tokio::sync::mpsc;

    struct MockAdapter {
        sent: Mutex<Vec<BotOutboundMessage>>,
        edited: Mutex<Vec<BotOutboundMessage>>,
    }

    impl MockAdapter {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                edited: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl PlatformAdapter for MockAdapter {
        fn platform_id(&self) -> &'static str {
            "mock"
        }

        async fn start_listening(
            &self,
            _tx: mpsc::Sender<crate::events::BotInboundEvent>,
        ) -> Result<()> {
            Ok(())
        }

        async fn send_message(
            &self,
            target: &MessageTarget,
            message: &BotOutboundMessage,
        ) -> Result<MessageHandle> {
            let mut sent = self.sent.lock().await;
            sent.push(message.clone());
            Ok(MessageHandle {
                target: target.clone(),
                message_id: format!("msg_{}", sent.len()),
            })
        }

        async fn edit_message(
            &self,
            _handle: &MessageHandle,
            message: &BotOutboundMessage,
        ) -> Result<()> {
            let mut edited = self.edited.lock().await;
            edited.push(message.clone());
            Ok(())
        }

        async fn send_typing(&self, _target: &MessageTarget) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_throttler_streaming() {
        let adapter = Arc::new(MockAdapter::new());
        let target = MessageTarget::new("mock", "c1", None, Some("u1".into()));
        let throttler = StreamThrottler::new(adapter.clone(), target, 10);

        throttler.push_text_delta("Hello").await;
        throttler.push_text_delta(" World").await;
        throttler.set_tool_status(Some("tool_running".into())).await;
        throttler.finish().await;

        let sent = adapter.sent.lock().await;
        assert_eq!(sent.len(), 1);
        assert!(sent[0].text.contains("Hello World"));
        assert_eq!(sent[0].tool_status, Some("tool_running".into()));
        assert!(sent[0].is_final);
    }
}
