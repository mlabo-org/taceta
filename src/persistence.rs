use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taceta::domain::{Attachment, ChatMessage, ThinkingMode};
use uuid::Uuid;

const APP_STATE_STORAGE_KEY: &str = "taceta.application-state.v1";

pub const CONTEXT_LENGTH_OPTIONS: [u32; 7] =
    [4_096, 8_192, 16_384, 32_768, 65_536, 131_072, 262_144];
pub const DEFAULT_CONTEXT_LENGTH: u32 = 32_768;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Conversation {
    pub id: Uuid,
    pub title: String,
    pub messages: Vec<ChatMessage>,
}

impl Default for Conversation {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            title: "New chat".to_owned(),
            messages: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct PersistedAppState {
    pub conversations: Vec<Conversation>,
    pub active_conversation_id: Uuid,
    pub selected_model: Option<String>,
    pub thinking_modes: HashMap<String, ThinkingMode>,
    pub show_thinking_trace: bool,
    pub context_length: u32,
    pub draft: String,
    pub pending_attachments: Vec<Attachment>,
}

impl Default for PersistedAppState {
    fn default() -> Self {
        let conversation = Conversation::default();
        Self {
            active_conversation_id: conversation.id,
            conversations: vec![conversation],
            selected_model: None,
            thinking_modes: HashMap::new(),
            show_thinking_trace: false,
            context_length: DEFAULT_CONTEXT_LENGTH,
            draft: String::new(),
            pending_attachments: Vec::new(),
        }
    }
}

impl PersistedAppState {
    pub fn normalized(mut self) -> Self {
        if self.conversations.is_empty() {
            return Self::default();
        }
        if !self
            .conversations
            .iter()
            .any(|conversation| conversation.id == self.active_conversation_id)
        {
            self.active_conversation_id = self.conversations[0].id;
        }
        self.context_length = normalize_context_length(self.context_length);
        self
    }

    pub fn active_conversation(&self) -> &Conversation {
        self.conversations
            .iter()
            .find(|conversation| conversation.id == self.active_conversation_id)
            .unwrap_or(&self.conversations[0])
    }

    pub fn active_conversation_mut(&mut self) -> &mut Conversation {
        let index = self
            .conversations
            .iter()
            .position(|conversation| conversation.id == self.active_conversation_id)
            .unwrap_or(0);
        &mut self.conversations[index]
    }

    pub fn start_new_conversation(&mut self) {
        let conversation = Conversation::default();
        self.active_conversation_id = conversation.id;
        self.conversations.insert(0, conversation);
        self.draft.clear();
        self.pending_attachments.clear();
    }
}

pub fn normalize_context_length(value: u32) -> u32 {
    CONTEXT_LENGTH_OPTIONS
        .iter()
        .copied()
        .min_by_key(|candidate| candidate.abs_diff(value))
        .unwrap_or(DEFAULT_CONTEXT_LENGTH)
}

pub fn load_app_state(storage: Option<&dyn eframe::Storage>) -> PersistedAppState {
    storage
        .and_then(|storage| eframe::get_value::<PersistedAppState>(storage, APP_STATE_STORAGE_KEY))
        .unwrap_or_default()
        .normalized()
}

pub fn save_app_state(storage: &mut dyn eframe::Storage, state: &PersistedAppState) {
    eframe::set_value(storage, APP_STATE_STORAGE_KEY, &state.clone().normalized());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_repairs_an_unknown_active_conversation() {
        let state = PersistedAppState {
            active_conversation_id: Uuid::new_v4(),
            ..Default::default()
        }
        .normalized();
        assert_eq!(state.active_conversation_id, state.conversations[0].id);
    }

    #[test]
    fn context_length_is_normalized_to_a_supported_step() {
        assert_eq!(normalize_context_length(40_000), 32_768);
        assert_eq!(normalize_context_length(130_000), 131_072);
    }
}
