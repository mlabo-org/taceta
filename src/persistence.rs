use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taceta::domain::{Attachment, ChatMessage, ThinkingMode};
use taceta::web_search::ProviderKind;
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
    /// User-authored titles must not be replaced by the first prompt.
    #[serde(default)]
    pub title_is_custom: bool,
    /// Web access is deliberately opt-in for each conversation.
    #[serde(default)]
    pub web_search_enabled: bool,
}

impl Default for Conversation {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            title: "New chat".to_owned(),
            messages: Vec::new(),
            title_is_custom: false,
            web_search_enabled: false,
        }
    }
}

impl Conversation {
    pub fn is_untitled(&self) -> bool {
        self.messages.is_empty() && !self.title_is_custom
    }

    pub fn should_generate_title(&self) -> bool {
        self.is_untitled()
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
    #[serde(default)]
    pub web_search_provider: ProviderKind,
    #[serde(default = "default_max_search_results")]
    pub max_search_results: u8,
    #[serde(default)]
    pub fetch_search_pages: bool,
    /// Whether the one-time Taceta Link browser setup guide was acknowledged.
    /// Missing fields are treated as acknowledged so upgrades do not repeat
    /// the guide for existing users.
    #[serde(default = "default_link_setup_acknowledged")]
    pub taceta_link_setup_acknowledged: bool,
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
            web_search_provider: ProviderKind::Brave,
            max_search_results: 5,
            fetch_search_pages: true,
            taceta_link_setup_acknowledged: false,
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
        self.max_search_results = self.max_search_results.clamp(1, 5);
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

    pub fn rename_conversation(&mut self, id: Uuid, title: &str) -> bool {
        let title = title.trim();
        if title.is_empty() {
            return false;
        }
        let Some(conversation) = self
            .conversations
            .iter_mut()
            .find(|conversation| conversation.id == id)
        else {
            return false;
        };
        conversation.title = title.to_owned();
        conversation.title_is_custom = true;
        true
    }

    pub fn delete_conversation(&mut self, id: Uuid) -> bool {
        let Some(index) = self
            .conversations
            .iter()
            .position(|conversation| conversation.id == id)
        else {
            return false;
        };
        let deleted_active_conversation = id == self.active_conversation_id;
        self.conversations.remove(index);

        if self.conversations.is_empty() {
            let conversation = Conversation::default();
            self.active_conversation_id = conversation.id;
            self.conversations.push(conversation);
        } else if deleted_active_conversation {
            let replacement_index = index.min(self.conversations.len() - 1);
            self.active_conversation_id = self.conversations[replacement_index].id;
        }

        if deleted_active_conversation {
            self.draft.clear();
            self.pending_attachments.clear();
        }
        true
    }
}

fn default_max_search_results() -> u8 {
    5
}

fn default_link_setup_acknowledged() -> bool {
    true
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

    #[test]
    fn new_conversations_start_with_web_search_disabled() {
        assert!(!Conversation::default().web_search_enabled);
        assert_eq!(
            PersistedAppState::default().web_search_provider,
            ProviderKind::Brave
        );
        assert_eq!(PersistedAppState::default().max_search_results, 5);
    }

    #[test]
    fn renaming_an_empty_conversation_preserves_the_custom_title() {
        let mut state = PersistedAppState::default();
        let id = state.active_conversation_id;

        assert!(state.rename_conversation(id, "  調査メモ  "));
        assert_eq!(state.active_conversation().title, "調査メモ");
        assert!(!state.active_conversation().should_generate_title());
        assert!(!state.rename_conversation(id, "   "));
        assert_eq!(state.active_conversation().title, "調査メモ");
    }

    #[test]
    fn deleting_the_active_conversation_selects_a_valid_replacement() {
        let mut state = PersistedAppState::default();
        let deleted_id = state.active_conversation_id;
        state.start_new_conversation();
        let replacement_id = state.active_conversation_id;
        state.active_conversation_id = deleted_id;
        state.draft = "discard this draft".to_owned();

        assert!(state.delete_conversation(deleted_id));
        assert_eq!(state.active_conversation_id, replacement_id);
        assert!(state.draft.is_empty());
        assert_eq!(state.conversations.len(), 1);
    }

    #[test]
    fn deleting_the_last_conversation_creates_a_fresh_chat() {
        let mut state = PersistedAppState::default();
        let deleted_id = state.active_conversation_id;

        assert!(state.delete_conversation(deleted_id));
        assert_eq!(state.conversations.len(), 1);
        assert_ne!(state.active_conversation_id, deleted_id);
        assert!(state.active_conversation().is_untitled());
    }

    #[test]
    fn unknown_persisted_executor_stays_unconfigured() {
        let state = PersistedAppState {
            web_search_provider: ProviderKind::Unknown,
            ..Default::default()
        }
        .normalized();
        assert_eq!(state.web_search_provider, ProviderKind::Unknown);
        assert_eq!(state.web_search_provider.wire_value(), "unknown");
        assert!(state.web_search_provider.account_name().is_none());
    }

    #[test]
    fn removed_executor_values_fail_closed_during_deserialization() {
        let value = serde_json::json!({"web_search_provider":"chatgpt_browser"});
        let state: PersistedAppState = serde_json::from_value(value).unwrap();
        assert_eq!(state.web_search_provider, ProviderKind::Unknown);
    }

    #[test]
    fn fresh_state_shows_link_setup_but_legacy_state_is_acknowledged() {
        assert!(!PersistedAppState::default().taceta_link_setup_acknowledged);

        let legacy: PersistedAppState = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(legacy.taceta_link_setup_acknowledged);
    }

    #[test]
    fn link_setup_acknowledgement_round_trips() {
        let value = serde_json::to_value(PersistedAppState {
            taceta_link_setup_acknowledged: true,
            ..Default::default()
        })
        .unwrap();
        let restored: PersistedAppState = serde_json::from_value(value).unwrap();
        assert!(restored.taceta_link_setup_acknowledged);
    }
}
