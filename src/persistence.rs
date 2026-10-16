use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use taceta::backend::OllamaEndpointMode;
use taceta::domain::{
    Attachment, ChatMessage, DEFAULT_CHATGPT_WEB_REQUEST_LIMIT, InferenceProvider, Role, ThinkingMode,
    normalize_chatgpt_web_request_limit,
};
use taceta::web_search::ProviderKind;
use uuid::Uuid;

const APP_STATE_STORAGE_KEY: &str = "taceta.application-state.v1";
pub const UNREADABLE_APP_STATE_KEY_PREFIX: &str = "taceta.application-state.unreadable.";

pub const CONTEXT_LENGTH_OPTIONS: [u32; 7] =
    [4_096, 8_192, 16_384, 32_768, 65_536, 131_072, 262_144];
pub const DEFAULT_CONTEXT_LENGTH: u32 = 32_768;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub context_window: Option<u64>,
}

/// One coding CLI's own session for a conversation. The conversation's
/// messages are the record; the session is that CLI's working context.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct CliSession {
    pub session_id: String,
    /// How many of the conversation's messages this session already holds.
    /// Later messages came from another engine and are handed over on the
    /// next turn.
    pub seen_messages: usize,
}

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
    pub agent_enabled: bool,
    pub workspace: Option<std::path::PathBuf>,
    /// Legacy conversations inherit their existing Taceta provider on load.
    pub provider: Option<InferenceProvider>,
    pub cli_sessions: HashMap<InferenceProvider, CliSession>,
    pub cli_usage: Option<TokenUsage>,
    /// How many of the messages Taceta's own task record holds. `None` until
    /// the record has been written back once; messages after this count came
    /// from another engine and are imported into the record on the next run.
    pub task_seen_messages: Option<usize>,
    /// Stands in for the oldest messages when the conversation is handed to
    /// an engine it would not otherwise fit. The messages are kept.
    pub summary: Option<taceta::prior_context::Summary>,
    /// What was typed here and not sent, kept while another conversation is open.
    pub draft: String,
    /// With Grok or Claude: talk only, with no tools and no workspace.
    pub cli_chat: bool,
}

impl Default for Conversation {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            title: "New chat".to_owned(),
            messages: Vec::new(),
            title_is_custom: false,
            web_search_enabled: false,
            agent_enabled: false,
            workspace: None,
            provider: None,
            cli_sessions: HashMap::new(),
            cli_usage: None,
            task_seen_messages: None,
            summary: None,
            draft: String::new(),
            cli_chat: false,
        }
    }
}

impl Conversation {
    pub fn for_provider(provider: InferenceProvider) -> Self {
        Self {
            provider: Some(provider),
            ..Default::default()
        }
    }

    /// Whether this conversation works in a folder (Code) rather than only
    /// talking (Chat). Each engine keeps the choice in its own field.
    pub fn is_code(&self) -> bool {
        match self.provider {
            Some(InferenceProvider::Grok | InferenceProvider::Claude | InferenceProvider::Gpt) => !self.cli_chat,
            _ => self.agent_enabled,
        }
    }

    pub fn has_history(&self) -> bool {
        !self.messages.is_empty()
    }

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
    pub inference_provider: InferenceProvider,
    pub provider_models: HashMap<InferenceProvider, String>,
    pub agent_max_steps: u32,
    pub agent_max_duration_secs: u64,
    pub thinking_modes: HashMap<String, ThinkingMode>,
    pub show_thinking_trace: bool,
    pub context_length: u32,
    pub draft: String,
    pub pending_attachments: Vec<Attachment>,
    #[serde(default)]
    pub web_search_provider: ProviderKind,
    #[serde(default = "default_max_search_results")]
    pub max_search_results: u8,
    #[serde(default = "default_chatgpt_web_request_limit")]
    pub chatgpt_web_request_limit: u8,
    #[serde(default)]
    pub fetch_search_pages: bool,
    /// Automatic mode follows Ollama's macOS OLLAMA_HOST configuration.
    #[serde(default)]
    pub ollama_endpoint_mode: OllamaEndpointMode,
    /// Retained while automatic mode is active so switching modes is reversible.
    #[serde(default)]
    pub ollama_custom_endpoint: String,
    /// Whether the one-time Taceta Link browser setup guide was acknowledged.
    /// Missing fields are treated as acknowledged so upgrades do not repeat
    /// the guide for existing users.
    #[serde(default = "default_link_setup_acknowledged")]
    pub taceta_link_setup_acknowledged: bool,
    /// Whether coding CLIs ask Taceta before each action. When off, each
    /// CLI's own permission settings decide.
    #[serde(default = "default_cli_ask_every_time")]
    pub cli_ask_every_time: bool,
    /// The system prompt put ahead of a plain Ollama chat. `None` uses the
    /// built-in one in the current language; an empty text sends nothing.
    #[serde(default)]
    pub local_system_prompt: Option<String>,
}

fn default_cli_ask_every_time() -> bool {
    true
}

impl Default for PersistedAppState {
    fn default() -> Self {
        let conversation = Conversation::default();
        Self {
            active_conversation_id: conversation.id,
            conversations: vec![conversation],
            selected_model: None,
            inference_provider: InferenceProvider::Ollama,
            provider_models: HashMap::new(),
            agent_max_steps: 100,
            agent_max_duration_secs: 3_600,
            thinking_modes: HashMap::new(),
            show_thinking_trace: false,
            context_length: DEFAULT_CONTEXT_LENGTH,
            draft: String::new(),
            pending_attachments: Vec::new(),
            web_search_provider: ProviderKind::Brave,
            cli_ask_every_time: true,
            max_search_results: 5,
            chatgpt_web_request_limit: DEFAULT_CHATGPT_WEB_REQUEST_LIMIT,
            fetch_search_pages: true,
            ollama_endpoint_mode: OllamaEndpointMode::Auto,
            ollama_custom_endpoint: String::new(),
            taceta_link_setup_acknowledged: false,
            local_system_prompt: None,
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
        self.agent_max_steps = self.agent_max_steps.clamp(1, 1_000);
        self.agent_max_duration_secs = self.agent_max_duration_secs.clamp(60, 43_200);
        self.max_search_results = self.max_search_results.clamp(1, 5);
        self.chatgpt_web_request_limit =
            normalize_chatgpt_web_request_limit(self.chatgpt_web_request_limit);
        let legacy_provider = if self.inference_provider == InferenceProvider::Gpt {
            InferenceProvider::Ollama
        } else {
            self.inference_provider
        };
        for conversation in &mut self.conversations {
            // Stop placeholders saved before the interrupted flag existed.
            for message in &mut conversation.messages {
                if message.role == Role::Assistant
                    && matches!(
                        message.content.trim(),
                        "生成を停止しました" | "生成を停止しました。" | "Generation stopped" | "Generation stopped."
                    )
                {
                    message.interrupted = true;
                }
            }
            if conversation.provider.is_none() {
                conversation.provider = Some(legacy_provider);
            }
        }
        let active_provider = self.active_conversation().provider.unwrap_or(legacy_provider);
        self.remember_provider_model(active_provider);
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
        let conversation = Conversation::for_provider(self.inference_provider);
        self.active_conversation_mut().draft = std::mem::take(&mut self.draft);
        self.active_conversation_id = conversation.id;
        self.conversations.insert(0, conversation);
        self.pending_attachments.clear();
    }

    fn remember_provider_model(&mut self, provider: InferenceProvider) {
        if provider != self.inference_provider {
            if let Some(model) = self.selected_model.take() {
                self.provider_models.insert(self.inference_provider, model);
            }
            self.selected_model = self.provider_models.get(&provider).cloned();
            self.inference_provider = provider;
        }
    }

    pub fn switch_provider(&mut self, provider: InferenceProvider) {
        let current_provider = self.inference_provider;
        if self.active_conversation().provider.is_none() {
            self.active_conversation_mut().provider = Some(current_provider);
        }
        self.remember_provider_model(provider);
        let conversation = self.active_conversation_mut();
        conversation.provider = Some(provider);
        if provider != InferenceProvider::Ollama {
            conversation.web_search_enabled = false;
        }
        self.pending_attachments.clear();
    }

    pub fn select_conversation(&mut self, id: Uuid) -> bool {
        let Some(conversation) = self.conversations.iter().find(|chat| chat.id == id) else {
            return false;
        };
        let provider = conversation.provider.unwrap_or(self.inference_provider);
        self.remember_provider_model(provider);
        if id != self.active_conversation_id {
            // An unsent draft stays with the conversation it was typed in.
            self.active_conversation_mut().draft = std::mem::take(&mut self.draft);
            self.active_conversation_id = id;
            self.draft = std::mem::take(&mut self.active_conversation_mut().draft);
            self.pending_attachments.clear();
        }
        true
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
        self.delete_conversations(&[id]) == 1
    }

    pub fn delete_conversations(&mut self, ids: &[Uuid]) -> usize {
        let selected = ids.iter().copied().collect::<HashSet<_>>();
        if selected.is_empty() {
            return 0;
        }

        let deleted_active_conversation = selected.contains(&self.active_conversation_id);
        let replacement_id = if deleted_active_conversation {
            let active_index = self
                .conversations
                .iter()
                .position(|conversation| conversation.id == self.active_conversation_id)
                .unwrap_or(0);
            self.conversations
                .iter()
                .skip(active_index + 1)
                .find(|conversation| !selected.contains(&conversation.id))
                .or_else(|| {
                    self.conversations[..active_index]
                        .iter()
                        .rev()
                        .find(|conversation| !selected.contains(&conversation.id))
                })
                .map(|conversation| conversation.id)
        } else {
            None
        };

        let previous_count = self.conversations.len();
        self.conversations
            .retain(|conversation| !selected.contains(&conversation.id));
        let deleted_count = previous_count - self.conversations.len();
        if deleted_count == 0 {
            return 0;
        }

        if self.conversations.is_empty() {
            let conversation = Conversation::for_provider(self.inference_provider);
            self.active_conversation_id = conversation.id;
            self.conversations.push(conversation);
        } else if deleted_active_conversation {
            self.active_conversation_id = replacement_id.unwrap_or(self.conversations[0].id);
        }

        if deleted_active_conversation {
            let provider = self.active_conversation().provider.unwrap_or(self.inference_provider);
            self.remember_provider_model(provider);
            self.draft.clear();
            self.pending_attachments.clear();
        }
        deleted_count
    }
}

fn default_max_search_results() -> u8 {
    5
}

fn default_chatgpt_web_request_limit() -> u8 {
    DEFAULT_CHATGPT_WEB_REQUEST_LIMIT
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

/// The application state for this launch. `unreadable` holds the exact stored
/// text when a saved state exists but this build cannot decode it.
pub struct LoadedAppState {
    pub state: PersistedAppState,
    pub unreadable: Option<String>,
}

pub fn load_app_state(storage: Option<&dyn eframe::Storage>) -> LoadedAppState {
    let stored = storage.and_then(|storage| storage.get_string(APP_STATE_STORAGE_KEY));
    let decoded = storage
        .and_then(|storage| eframe::get_value::<PersistedAppState>(storage, APP_STATE_STORAGE_KEY));
    let unreadable = if decoded.is_none() { stored } else { None };
    LoadedAppState { state: decoded.unwrap_or_default().normalized(), unreadable }
}

/// Saves the state. An undecodable previous state is first kept verbatim under
/// its own key, so replacing it never destroys the only copy.
pub fn save_app_state(
    storage: &mut dyn eframe::Storage,
    state: &PersistedAppState,
    unreadable: &mut Option<String>,
) {
    if let Some(raw) = unreadable.take() {
        let key = format!("{UNREADABLE_APP_STATE_KEY_PREFIX}{}", chrono::Utc::now().timestamp());
        storage.set_string(&key, raw);
    }
    eframe::set_value(storage, APP_STATE_STORAGE_KEY, &state.clone().normalized());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_placeholders_saved_without_the_flag_are_marked_interrupted() {
        let mut state = PersistedAppState::default();
        let messages = &mut state.conversations[0].messages;
        messages.push(ChatMessage::new_assistant("生成を停止しました。"));
        messages.push(ChatMessage::new_assistant("An answer that mentions Generation stopped."));
        let state = state.normalized();
        let messages = &state.conversations[0].messages;
        assert!(messages[0].interrupted);
        assert!(!messages[1].interrupted);
    }

    #[test]
    fn switching_providers_keeps_the_conversation_and_each_engines_own_session() {
        let mut state = PersistedAppState::default();
        state.active_conversation_mut().messages.push(ChatMessage::new_user("local conversation"));
        let id = state.active_conversation_id;
        for provider in [InferenceProvider::Grok, InferenceProvider::Gpt] {
            state.switch_provider(provider);
            assert_eq!(state.active_conversation_id, id);
            assert_eq!(state.inference_provider, provider);
            assert_eq!(state.active_conversation().messages[0].content, "local conversation");
        }
        assert_eq!(state.conversations.len(), 1);
        state.active_conversation_mut().cli_sessions.insert(
            InferenceProvider::Gpt,
            CliSession { session_id: "thread-1".into(), seen_messages: 1 },
        );

        // Leaving GPT and coming back finds the same session.
        state.switch_provider(InferenceProvider::Ollama);
        state.switch_provider(InferenceProvider::Claude);
        state.switch_provider(InferenceProvider::Gpt);
        assert_eq!(state.active_conversation_id, id);
        let session = &state.active_conversation().cli_sessions[&InferenceProvider::Gpt];
        assert_eq!((session.session_id.as_str(), session.seen_messages), ("thread-1", 1));
    }

    #[test]
    fn a_state_saved_with_the_fields_of_an_earlier_gpt_connection_still_loads() {
        let saved = serde_json::json!({"conversations":[{
            "id":"4635145f-4448-499e-a8a8-49adb02580f6","title":"Earlier GPT work","provider":"Gpt","agent_enabled":true,
            "gpt":{"thread_id":"old-thread","status":"Completed"},"archived_gpt_thread_ids":["older"],"gpt_ownership_revision":2
        }],"gpt_max_tool_actions":45});
        let state: PersistedAppState = serde_json::from_value(saved).unwrap();
        let state = state.normalized();
        assert_eq!(state.conversations[0].title, "Earlier GPT work");
        assert_eq!(state.conversations[0].provider, Some(InferenceProvider::Gpt));
        // Its session starts anew; the conversation itself is handed over.
        assert!(state.conversations[0].cli_sessions.is_empty());
    }

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
        assert_eq!(
            PersistedAppState::default().chatgpt_web_request_limit,
            DEFAULT_CHATGPT_WEB_REQUEST_LIMIT
        );
    }

    #[test]
    fn browser_search_routes_migrate_legacy_default_to_google() {
        let state: PersistedAppState =
            serde_json::from_value(serde_json::json!({
                "web_search_provider": "DefaultSearch"
            }))
            .unwrap();

        assert_eq!(state.web_search_provider, ProviderKind::GoogleSearch);
        assert_eq!(state.web_search_provider.wire_value(), "google_search");
        assert_eq!(
            serde_json::to_value(state.web_search_provider).unwrap(),
            serde_json::json!("GoogleSearch")
        );
    }

    #[test]
    fn chatgpt_web_request_limit_defaults_to_one_and_stays_within_one_to_three() {
        let legacy: PersistedAppState = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(
            legacy.chatgpt_web_request_limit,
            DEFAULT_CHATGPT_WEB_REQUEST_LIMIT
        );

        let below_minimum = PersistedAppState {
            chatgpt_web_request_limit: 0,
            ..Default::default()
        }
        .normalized();
        assert_eq!(below_minimum.chatgpt_web_request_limit, 1);

        let above_maximum = PersistedAppState {
            chatgpt_web_request_limit: 4,
            ..Default::default()
        }
        .normalized();
        assert_eq!(above_maximum.chatgpt_web_request_limit, 3);
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
    fn deleting_multiple_non_active_conversations_preserves_the_active_chat() {
        let mut state = PersistedAppState::default();
        let first_id = state.active_conversation_id;
        state.start_new_conversation();
        let second_id = state.active_conversation_id;
        state.start_new_conversation();
        let active_id = state.active_conversation_id;
        state.draft = "keep this draft".to_owned();

        assert_eq!(state.delete_conversations(&[first_id, second_id]), 2);
        assert_eq!(state.conversations.len(), 1);
        assert_eq!(state.active_conversation_id, active_id);
        assert_eq!(state.draft, "keep this draft");
    }

    #[test]
    fn deleting_multiple_conversations_including_active_selects_the_next_survivor() {
        let mut state = PersistedAppState::default();
        let oldest_id = state.active_conversation_id;
        state.start_new_conversation();
        let active_id = state.active_conversation_id;
        state.start_new_conversation();
        let newest_id = state.active_conversation_id;
        state.active_conversation_id = active_id;
        state.draft = "discard this draft".to_owned();

        assert_eq!(state.delete_conversations(&[newest_id, active_id]), 2);
        assert_eq!(state.active_conversation_id, oldest_id);
        assert!(state.draft.is_empty());
    }

    #[test]
    fn deleting_all_conversations_in_bulk_creates_one_fresh_chat() {
        let mut state = PersistedAppState::default();
        state.start_new_conversation();
        let deleted_ids = state
            .conversations
            .iter()
            .map(|conversation| conversation.id)
            .collect::<Vec<_>>();

        assert_eq!(state.delete_conversations(&deleted_ids), 2);
        assert_eq!(state.conversations.len(), 1);
        assert!(!deleted_ids.contains(&state.active_conversation_id));
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

    #[test]
    fn legacy_state_uses_automatic_ollama_endpoint_resolution() {
        let state: PersistedAppState = serde_json::from_value(serde_json::json!({})).unwrap();

        assert_eq!(state.ollama_endpoint_mode, OllamaEndpointMode::Auto);
        assert!(state.ollama_custom_endpoint.is_empty());
    }

    #[test]
    fn custom_ollama_endpoint_round_trips() {
        let state = PersistedAppState {
            ollama_endpoint_mode: OllamaEndpointMode::Custom,
            ollama_custom_endpoint: "http://127.0.0.1:23456".to_owned(),
            ..Default::default()
        };
        let value = serde_json::to_value(state).unwrap();
        let restored: PersistedAppState = serde_json::from_value(value).unwrap();

        assert_eq!(restored.ollama_endpoint_mode, OllamaEndpointMode::Custom);
        assert_eq!(restored.ollama_custom_endpoint, "http://127.0.0.1:23456");
    }

    #[test]
    fn an_unsent_draft_stays_with_its_conversation() {
        let mut state = PersistedAppState::default();
        let first = state.active_conversation_id;
        state.draft = "not sent yet".into();
        state.start_new_conversation();
        assert!(state.draft.is_empty());
        state.draft = "second draft".into();
        let second = state.active_conversation_id;
        assert!(state.select_conversation(first));
        assert_eq!(state.draft, "not sent yet");
        // Selecting the conversation that is already open changes nothing.
        assert!(state.select_conversation(first));
        assert_eq!(state.draft, "not sent yet");
        assert!(state.select_conversation(second));
        assert_eq!(state.draft, "second draft");
    }
}
