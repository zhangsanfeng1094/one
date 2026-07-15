pub mod anthropic;
pub mod media;
pub mod mock;
pub mod models_file;
#[cfg(feature = "network")]
pub mod ollama;
pub mod openai;
#[cfg(feature = "http-providers")]
pub mod openrouter;
pub mod registry;
#[cfg(feature = "network")]
pub mod remote_models;
#[cfg(feature = "network")]
pub mod sse;
pub mod thinking;

pub use anthropic::AnthropicProvider;
pub use mock::MockProvider;
pub use models_file::{
    load_models_file, resolve_secret, save_models_file, try_load_models_file, ModelsConfig,
};
#[cfg(feature = "network")]
pub use ollama::OllamaProvider;
pub use openai::{OpenAiProvider, OpenaiWireApi};
#[cfg(feature = "http-providers")]
pub use openrouter::OpenRouterProvider;
pub use registry::{ModelEntry, ModelRegistry, ProviderConfig};
#[cfg(feature = "network")]
pub use remote_models::{list_openai_compatible_models, RemoteModel};
pub use thinking::ThinkingWire;
