use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use one_core::ThinkingLevel;
use one_session::SessionManager;
use tokio::sync::Mutex;

use crate::events::BotSessionKey;

/// Per-session contextual state in the Bot Gateway.
pub struct BotSessionState {
    pub session_manager: Arc<Mutex<SessionManager>>,
    pub model: Option<String>,
    pub thinking_level: ThinkingLevel,
    pub workspace_dir: PathBuf,
}

/// Mapper connecting IM BotSessionKeys to one_session managers.
#[derive(Clone)]
pub struct SessionMapper {
    default_workspace: PathBuf,
    default_model: String,
    sessions: Arc<Mutex<HashMap<String, BotSessionState>>>,
}

impl SessionMapper {
    pub fn new(default_workspace: PathBuf, default_model: String) -> Self {
        Self {
            default_workspace,
            default_model,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Retrieve or initialize a session for the given session key.
    pub async fn get_or_create(&self, key: &BotSessionKey) -> Arc<Mutex<SessionManager>> {
        let string_key = key.to_string_key();
        let mut map = self.sessions.lock().await;

        if !map.contains_key(&string_key) {
            let session_manager = SessionManager::create(&self.default_workspace)
                .await
                .unwrap_or_else(|_| SessionManager::in_memory(&self.default_workspace));

            map.insert(
                string_key.clone(),
                BotSessionState {
                    session_manager: Arc::new(Mutex::new(session_manager)),
                    model: Some(self.default_model.clone()),
                    thinking_level: ThinkingLevel::default(),
                    workspace_dir: self.default_workspace.clone(),
                },
            );
        }

        map.get(&string_key).unwrap().session_manager.clone()
    }

    /// Reset session (implements `/new` or `/reset`).
    pub async fn reset_session(&self, key: &BotSessionKey) -> Arc<Mutex<SessionManager>> {
        let string_key = key.to_string_key();
        let mut map = self.sessions.lock().await;

        let session_manager = SessionManager::create(&self.default_workspace)
            .await
            .unwrap_or_else(|_| SessionManager::in_memory(&self.default_workspace));

        let sm_arc = Arc::new(Mutex::new(session_manager));
        map.insert(
            string_key,
            BotSessionState {
                session_manager: sm_arc.clone(),
                model: Some(self.default_model.clone()),
                thinking_level: ThinkingLevel::default(),
                workspace_dir: self.default_workspace.clone(),
            },
        );

        sm_arc
    }

    /// Get current model for session.
    pub async fn get_model(&self, key: &BotSessionKey) -> String {
        let string_key = key.to_string_key();
        let map = self.sessions.lock().await;
        map.get(&string_key)
            .and_then(|s| s.model.clone())
            .unwrap_or_else(|| self.default_model.clone())
    }

    /// Set model for session (implements `/model <name>`).
    pub async fn set_model(&self, key: &BotSessionKey, model: String) {
        let string_key = key.to_string_key();
        let mut map = self.sessions.lock().await;
        if let Some(state) = map.get_mut(&string_key) {
            state.model = Some(model);
        }
    }
}
