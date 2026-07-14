use std::sync::Arc;

use one_core::tool::Tool;
use serde_json::Value;

use crate::traits::{Extension, ExtensionContext, ExtensionEvent};

pub struct ExtensionRuntime {
    extensions: Vec<Arc<dyn Extension>>,
}

impl ExtensionRuntime {
    pub fn new(extensions: Vec<Arc<dyn Extension>>) -> Self {
        Self { extensions }
    }

    pub fn empty() -> Self {
        Self {
            extensions: Vec::new(),
        }
    }

    pub async fn load_all(&self, ctx: &ExtensionContext<'_>) -> crate::Result<()> {
        for extension in &self.extensions {
            extension.on_load(ctx).await?;
        }
        Ok(())
    }

    pub async fn emit(&self, event: &ExtensionEvent) -> crate::Result<()> {
        for extension in &self.extensions {
            extension.on_event(event).await?;
        }
        Ok(())
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.extensions
            .iter()
            .flat_map(|extension| extension.tools())
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.extensions
            .iter()
            .map(|extension| extension.name().to_string())
            .collect()
    }

    pub fn custom_states(&self) -> Vec<(String, Value)> {
        self.extensions
            .iter()
            .filter_map(|extension| extension.custom_state())
            .collect()
    }

    pub fn restore_custom(&self, custom_type: &str, data: Value) {
        for extension in &self.extensions {
            let _ = extension.restore_state(custom_type, &data);
        }
    }
}