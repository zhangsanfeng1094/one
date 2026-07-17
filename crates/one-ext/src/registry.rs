//! Extension registry builder (Codex `ExtensionRegistryBuilder` analogue).

use std::sync::Arc;

use crate::traits::Extension;

/// Mutable builder used during session / process init.
#[derive(Default)]
pub struct ExtensionRegistryBuilder {
    extensions: Vec<Arc<dyn Extension>>,
}

impl ExtensionRegistryBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a compiled-in or discovered extension.
    pub fn install(&mut self, extension: Arc<dyn Extension>) -> &mut Self {
        // Skip duplicate names (first wins).
        let name = extension.name().to_string();
        if self.extensions.iter().any(|e| e.name() == name) {
            tracing::warn!(extension = %name, "duplicate extension name; skipping");
            return self;
        }
        self.extensions.push(extension);
        self
    }

    pub fn install_all(&mut self, extensions: impl IntoIterator<Item = Arc<dyn Extension>>) -> &mut Self {
        for ext in extensions {
            self.install(ext);
        }
        self
    }

    pub fn build(self) -> ExtensionRegistry {
        ExtensionRegistry {
            extensions: self.extensions,
        }
    }
}

/// Immutable set of installed extensions.
pub struct ExtensionRegistry {
    extensions: Vec<Arc<dyn Extension>>,
}

impl ExtensionRegistry {
    pub fn empty() -> Self {
        Self {
            extensions: Vec::new(),
        }
    }

    pub fn extensions(&self) -> &[Arc<dyn Extension>] {
        &self.extensions
    }

    pub fn names(&self) -> Vec<String> {
        self.extensions
            .iter()
            .map(|e| e.name().to_string())
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Extension>> {
        self.extensions.iter().find(|e| e.name() == name)
    }

    pub fn is_empty(&self) -> bool {
        self.extensions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.extensions.len()
    }
}
