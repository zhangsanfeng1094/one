pub mod anthropic;
pub mod auth;
pub mod cache;
pub mod compat;
pub mod gemini;
pub mod media;
pub mod mock;
pub mod models_file;
#[cfg(feature = "network")]
pub mod ollama;
pub mod openai;
#[cfg(feature = "http-providers")]
pub mod openai_codex;
pub mod openai_codex_models;
pub mod opencode_models;
#[cfg(feature = "http-providers")]
pub mod openrouter;
pub mod registry;
pub mod xai_models;
#[cfg(feature = "network")]
pub mod remote_models;
#[cfg(feature = "network")]
pub mod sse;
pub mod thinking;

pub use anthropic::AnthropicProvider;
pub use auth::{
    login_provider, oauth_provider_catalog, AuthCredential, AuthError, AuthEvent, AuthInteraction,
    AuthPrompt, AuthStatus, AuthStorage, ModelAuth, OAuthCredential, OAuthProviderInfo,
    SelectOption, PROVIDER_OPENAI_CODEX, PROVIDER_OPENCODE, PROVIDER_OPENCODE_GO, PROVIDER_XAI,
};
pub use compat::{
    format_thinking_level_map, normalize_compat_key, parse_thinking_level_map,
    AnthropicMessagesCompat, CompatConfig, MaxTokensField, OpenAiCompletionsCompat,
    ResolvedAnthropicCompat, ResolvedOpenAiCompat, ThinkingFormat, ThinkingLevelMap,
    COMPAT_BOOL_FIELDS,
};
pub use gemini::GeminiProvider;
pub use mock::MockProvider;
pub use models_file::{
    load_models_file, resolve_secret, save_models_file, try_load_models_file, ModelsConfig,
};
#[cfg(feature = "network")]
pub use ollama::OllamaProvider;
pub use openai::{OpenAiProvider, OpenaiWireApi, ProviderApi};
#[cfg(feature = "http-providers")]
pub use openai_codex::OpenAiCodexProvider;
pub use openai_codex_models::{
    seed_openai_codex_models, seed_openai_codex_models_default, CodexSeedReport,
    OPENAI_CODEX_BUILTIN_MODELS, OPENAI_CODEX_DEFAULT_MODEL,
};
pub use opencode_models::{
    seed_opencode_models, seed_opencode_models_default, OpencodeSeedReport,
    OPENCODE_GO_BUILTIN_MODELS, OPENCODE_GO_DEFAULT_MODEL, OPENCODE_ZEN_BUILTIN_MODELS,
    OPENCODE_ZEN_DEFAULT_MODEL,
};
#[cfg(feature = "http-providers")]
pub use openrouter::OpenRouterProvider;
pub use registry::{ModelEntry, ModelRegistry, ProviderConfig};
#[cfg(feature = "network")]
pub use remote_models::{list_openai_compatible_models, RemoteModel};
pub use thinking::ThinkingWire;
pub use xai_models::{
    seed_xai_models, seed_xai_models_default, XaiSeedReport, XAI_BUILTIN_MODELS, XAI_DEFAULT_MODEL,
};
