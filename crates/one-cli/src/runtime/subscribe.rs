//! Agent event listeners (print / JSON / TUI collector).

use std::sync::{Arc, Mutex};

use one_core::events::AgentEvent;

use super::AppRuntime;

impl AppRuntime {
    pub async fn subscribe_printer(&mut self, json: bool) {
        let mut agent = self.agent.lock().await;
        agent.subscribe(Box::new(move |event: &AgentEvent| match event {
            AgentEvent::ThinkingDelta { delta } if !json => {
                // Print-mode: stream reasoning to stderr so stdout stays clean for piping.
                eprint!("{delta}");
            }
            AgentEvent::ThinkingDelta { delta } if json => {
                let line = serde_json::json!({"type":"thinking_delta","delta":delta});
                println!("{line}");
            }
            AgentEvent::TextDelta { delta } if !json => print!("{delta}"),
            AgentEvent::TextDelta { delta } if json => {
                let line = serde_json::json!({"type":"text_delta","delta":delta});
                println!("{line}");
            }
            AgentEvent::ToolExecutionStart { tool_call } if !json => {
                eprintln!("\n[tool] {}({})", tool_call.name, tool_call.arguments);
            }
            AgentEvent::ToolExecutionStart { tool_call } if json => {
                let line = serde_json::json!({
                    "type":"tool_start",
                    "name": tool_call.name,
                    "arguments": tool_call.arguments,
                });
                println!("{line}");
            }
            AgentEvent::ToolExecutionEnd { is_error, .. } if !json && *is_error => {
                eprintln!("[tool] error");
            }
            AgentEvent::ToolExecutionEnd {
                tool_call,
                is_error,
                output,
            } if json => {
                let line = serde_json::json!({
                    "type":"tool_end",
                    "name": tool_call.name,
                    "is_error": is_error,
                    "output_bytes": output.as_text().len(),
                });
                println!("{line}");
            }
            AgentEvent::TurnEnd { turn, tool_results, .. } if json => {
                let line = serde_json::json!({
                    "type":"turn_end",
                    "turn": turn,
                    "tool_results": tool_results.len(),
                });
                println!("{line}");
            }
            AgentEvent::AgentEnd { new_messages } if json => {
                let line = serde_json::json!({
                    "type":"agent_end",
                    "messages": new_messages.len(),
                });
                println!("{line}");
            }
            _ => {}
        }));
    }

    pub async fn subscribe_collector(&mut self, events: Arc<Mutex<Vec<AgentEvent>>>) {
        let mut agent = self.agent.lock().await;
        agent.clear_listeners();
        agent.subscribe(Box::new(move |event| {
            if let Ok(mut batch) = events.lock() {
                batch.push(event.clone());
            }
        }));
    }
}
