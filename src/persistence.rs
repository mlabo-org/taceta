use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use taceta::backend::OllamaEndpointMode;
use taceta::domain::{
    Attachment, ChatMessage, DEFAULT_CHATGPT_WEB_REQUEST_LIMIT, InferenceProvider, ThinkingMode,
    normalize_chatgpt_web_request_limit,
};
use taceta::web_search::ProviderKind;
use taceta::gpt::{GptRunStatus, GptToolItem};
use uuid::Uuid;

const APP_STATE_STORAGE_KEY: &str = "taceta.application-state.v1";

pub const CONTEXT_LENGTH_OPTIONS: [u32; 7] =
    [4_096, 8_192, 16_384, 32_768, 65_536, 131_072, 262_144];
pub const DEFAULT_CONTEXT_LENGTH: u32 = 32_768;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct GptUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub context_window: Option<u64>,
}

/// A display cache and reference to Codex's authoritative conversation history.
/// These messages are never replayed as input to another execution service.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct GptConversation {
    pub thread_id: Option<String>,
    pub status: GptRunStatus,
    pub item_messages: HashMap<String, Uuid>,
    pub tools: Vec<GptToolItem>,
    pub diff: String,
    pub thinking: String,
    pub usage: Option<GptUsage>,
    pub error: Option<String>,
    /// The original AgentSession remains intact; its portable state is sent once
    /// when the user explicitly starts this new Codex thread.
    pub handoff_from_taceta: bool,
    pub pending_handoff: bool,
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
    pub gpt: Option<GptConversation>,
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
            gpt: None,
        }
    }
}

impl Conversation {
    pub fn for_provider(provider: InferenceProvider) -> Self {
        Self {
            provider: Some(provider),
            agent_enabled: provider == InferenceProvider::Gpt,
            gpt: (provider == InferenceProvider::Gpt).then(GptConversation::default),
            ..Default::default()
        }
    }

    pub fn is_gpt(&self) -> bool {
        self.provider == Some(InferenceProvider::Gpt) || self.gpt.is_some()
    }

    pub fn has_history(&self) -> bool {
        !self.messages.is_empty()
            || self.gpt.as_ref().is_some_and(|gpt| gpt.thread_id.is_some())
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
    pub gpt_max_tool_actions: u32,
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
            gpt_max_tool_actions: 100,
            agent_max_duration_secs: 3_600,
            thinking_modes: HashMap::new(),
            show_thinking_trace: false,
            context_length: DEFAULT_CONTEXT_LENGTH,
            draft: String::new(),
            pending_attachments: Vec::new(),
            web_search_provider: ProviderKind::Brave,
            max_search_results: 5,
            chatgpt_web_request_limit: DEFAULT_CHATGPT_WEB_REQUEST_LIMIT,
            fetch_search_pages: true,
            ollama_endpoint_mode: OllamaEndpointMode::Auto,
            ollama_custom_endpoint: String::new(),
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
        self.agent_max_steps = self.agent_max_steps.clamp(1, 1_000);
        self.gpt_max_tool_actions = self.gpt_max_tool_actions.clamp(1, 1_000);
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
            if conversation.is_gpt() {
                conversation.provider = Some(InferenceProvider::Gpt);
                let gpt = conversation.gpt.get_or_insert_with(Default::default);
                if matches!(gpt.status, GptRunStatus::Running | GptRunStatus::AwaitingApproval) {
                    gpt.status = GptRunStatus::Interrupted;
                }
            } else if conversation.provider.is_none() {
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
        self.active_conversation_id = conversation.id;
        self.conversations.insert(0, conversation);
        self.draft.clear();
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
        let transfer_work = provider == InferenceProvider::Gpt
            && !self.active_conversation().is_gpt()
            && self.active_conversation().agent_enabled
            && self.active_conversation().workspace.is_some()
            && self.active_conversation().has_history();
        if transfer_work {
            self.remember_provider_model(provider);
            let conversation = self.active_conversation_mut();
            conversation.provider = Some(provider);
            conversation.web_search_enabled = false;
            conversation.gpt = Some(GptConversation { handoff_from_taceta: true, pending_handoff: true, ..Default::default() });
            self.pending_attachments.clear();
            return;
        }
        let crosses_history_owner = self.active_conversation().is_gpt()
            != (provider == InferenceProvider::Gpt);
        let preserve_current = crosses_history_owner && self.active_conversation().has_history();
        self.remember_provider_model(provider);
        if preserve_current {
            self.start_new_conversation();
        } else {
            let conversation = self.active_conversation_mut();
            if crosses_history_owner {
                conversation.agent_enabled = provider == InferenceProvider::Gpt;
                conversation.gpt = (provider == InferenceProvider::Gpt)
                    .then(GptConversation::default);
                conversation.workspace = None;
                conversation.web_search_enabled = false;
                self.pending_attachments.clear();
            }
            self.active_conversation_mut().provider = Some(provider);
        }
    }

    pub fn select_conversation(&mut self, id: Uuid) -> bool {
        let Some(conversation) = self.conversations.iter().find(|chat| chat.id == id) else {
            return false;
        };
        let provider = conversation.provider.unwrap_or(self.inference_provider);
        self.remember_provider_model(provider);
        self.active_conversation_id = id;
        self.draft.clear();
        self.pending_attachments.clear();
        true
    }

    /// A Codex thread's execution environment is fixed for its lifetime.
    pub fn configure_gpt_environment(&mut self, coding: bool, workspace: Option<std::path::PathBuf>) {
        let current = self.active_conversation();
        if !current.is_gpt() || (current.agent_enabled == coding && current.workspace == workspace) {
            return;
        }
        if current.has_history() {
            self.start_new_conversation();
        }
        let conversation = self.active_conversation_mut();
        conversation.agent_enabled = coding;
        conversation.workspace = workspace;
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

#[derive(Deserialize, Serialize)]
struct GptBinding {
    id: Uuid,
    title: String,
    title_is_custom: bool,
    thread_id: String,
    coding: bool,
    workspace: Option<std::path::PathBuf>,
    handoff_from_taceta: bool,
    pending_handoff: bool,
}

pub fn gpt_bindings_root() -> Result<std::path::PathBuf, String> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
        .map(|home| home.join("Library/Application Support/Taceta/GptBindings"))
        .ok_or_else(|| "The current user's home directory is unavailable".into())
}

/// Unlike eframe's asynchronous flush, this checkpoint reaches disk before the
/// service receives ThreadSaved and is allowed to begin a mutating turn.
pub fn save_gpt_binding(root: &std::path::Path, conversation: &Conversation) -> Result<(), String> {
    use std::io::Write;
    let session = conversation.gpt.as_ref().ok_or("GPT conversation metadata is missing")?;
    let thread_id = session.thread_id.as_ref().filter(|id| !id.is_empty())
        .ok_or("GPT conversation ID is missing")?;
    let binding = GptBinding {
        id: conversation.id, title: conversation.title.clone(), title_is_custom: conversation.title_is_custom,
        thread_id: thread_id.clone(), coding: conversation.agent_enabled,
        workspace: conversation.workspace.clone(), handoff_from_taceta: session.handoff_from_taceta,
        pending_handoff: session.pending_handoff,
    };
    let bytes = serde_json::to_vec(&binding).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(root).map_err(|error| format!("Could not create GPT history storage: {error}"))?;
    let destination = root.join(format!("{}.json", conversation.id));
    let temporary = root.join(format!(".{}.tmp", Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)] {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &destination)?;
        std::fs::File::open(root)?.sync_all()?;
        if let Some(parent) = root.parent() { std::fs::File::open(parent)?.sync_all()?; }
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&temporary); }
    result.map_err(|error| format!("Could not save GPT conversation before execution: {error}"))
}

pub fn recover_gpt_bindings(state: &mut PersistedAppState, root: &std::path::Path) -> Result<usize, String> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(format!("Could not read GPT history bindings: {error}")),
    };
    let mut count = 0;
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") { continue; }
        let binding: GptBinding = serde_json::from_slice(&std::fs::read(&path).map_err(|error| error.to_string())?)
            .map_err(|error| format!("Could not restore a saved GPT conversation binding: {error}"))?;
        if binding.thread_id.is_empty() { return Err("A saved GPT conversation binding has no thread ID".into()); }
        let conversation = if let Some(index) = state.conversations.iter().position(|conversation| conversation.id == binding.id) {
            &mut state.conversations[index]
        } else {
            state.conversations.push(Conversation {
                id: binding.id, title: binding.title, title_is_custom: binding.title_is_custom,
                ..Conversation::for_provider(InferenceProvider::Gpt)
            });
            count += 1;
            state.conversations.last_mut().unwrap()
        };
        conversation.provider = Some(InferenceProvider::Gpt);
        conversation.agent_enabled = binding.coding;
        conversation.workspace = binding.workspace;
        let session = conversation.gpt.get_or_insert_with(Default::default);
        session.thread_id = Some(binding.thread_id);
        session.handoff_from_taceta = binding.handoff_from_taceta;
        session.pending_handoff = binding.pending_handoff;
        if matches!(session.status, GptRunStatus::Idle | GptRunStatus::Running | GptRunStatus::AwaitingApproval) {
            session.status = GptRunStatus::Interrupted;
        }
    }
    let provider = state.active_conversation().provider.unwrap_or(state.inference_provider);
    state.remember_provider_model(provider);
    Ok(count)
}

/// Explicit local deletion keeps both this binding and Codex's raw history
/// recoverable, while excluding the archived binding from startup recovery.
pub fn archive_gpt_binding(root: &std::path::Path, id: Uuid) -> Result<(), String> {
    let source = root.join(format!("{id}.json"));
    if !source.exists() { return Ok(()); }
    let archive = root.join("archived");
    std::fs::create_dir_all(&archive).map_err(|error| error.to_string())?;
    std::fs::rename(source, archive.join(format!("{id}-{}.json", Uuid::new_v4())))
        .map_err(|error| format!("Could not archive GPT conversation binding: {error}"))?;
    std::fs::File::open(&archive).and_then(|file| file.sync_all()).map_err(|error| error.to_string())?;
    std::fs::File::open(root).and_then(|file| file.sync_all()).map_err(|error| error.to_string())?;
    Ok(())
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
    fn gpt_saved_owner_thread_and_controls_round_trip_without_changing_old_defaults() {
        let legacy: PersistedAppState = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(legacy.inference_provider, InferenceProvider::Ollama);
        assert_eq!(legacy.agent_max_steps, 100);
        assert_eq!(legacy.gpt_max_tool_actions, 100);
        assert!(!legacy.active_conversation().agent_enabled);
        let mut state = legacy;
        state.switch_provider(InferenceProvider::Gpt);
        state.selected_model = Some("account-model".into());
        state.gpt_max_tool_actions = 45;
        state.agent_max_duration_secs = 2_400;
        state.show_thinking_trace = true;
        state.active_conversation_mut().workspace = Some("/fixture/workspace".into());
        let session = state.active_conversation_mut().gpt.as_mut().unwrap();
        session.thread_id = Some("codex-thread".into());
        session.status = GptRunStatus::Running;
        let restored: PersistedAppState = serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        let restored = restored.normalized();
        assert_eq!(restored.inference_provider, InferenceProvider::Gpt);
        assert!(restored.active_conversation().agent_enabled);
        assert_eq!(restored.active_conversation().gpt.as_ref().unwrap().thread_id.as_deref(), Some("codex-thread"));
        assert_eq!(restored.active_conversation().gpt.as_ref().unwrap().status, GptRunStatus::Interrupted);
        assert_eq!(restored.gpt_max_tool_actions, 45);
        assert_eq!(restored.agent_max_duration_secs, 2_400);
        assert_eq!(restored.selected_model.as_deref(), Some("account-model"));
        assert!(restored.show_thinking_trace);
    }

    #[test]
    fn gpt_chat_provider_crossing_preserves_histories_and_restores_the_selected_owner() {
        let mut state = PersistedAppState::default();
        state.active_conversation_mut().messages.push(ChatMessage::new_user("local conversation"));
        let local_id = state.active_conversation_id;
        state.switch_provider(InferenceProvider::Grok);
        assert_eq!(state.active_conversation_id, local_id);
        state.switch_provider(InferenceProvider::Gpt);
        let gpt_id = state.active_conversation_id;
        assert_ne!(gpt_id, local_id);
        assert!(state.active_conversation().agent_enabled);
        assert!(state.active_conversation().messages.is_empty());
        state.active_conversation_mut().gpt.as_mut().unwrap().thread_id = Some("thread-1".into());
        state.switch_provider(InferenceProvider::Ollama);
        assert_ne!(state.active_conversation_id, gpt_id);
        assert_eq!(state.conversations.len(), 3);
        assert!(state.select_conversation(local_id));
        assert_eq!(state.inference_provider, InferenceProvider::Grok);
        assert_eq!(state.active_conversation().messages[0].content, "local conversation");
        assert!(state.select_conversation(gpt_id));
        assert_eq!(state.inference_provider, InferenceProvider::Gpt);
        state.configure_gpt_environment(true, Some("/fixture/other".into()));
        assert_ne!(state.active_conversation_id, gpt_id);
        assert_eq!(state.conversations.iter().find(|chat| chat.id == gpt_id).unwrap().gpt.as_ref().unwrap().thread_id.as_deref(), Some("thread-1"));
    }

    #[test]
    fn gpt_work_handoff_keeps_identity_workspace_and_original_messages() {
        let mut state = PersistedAppState::default();
        state.switch_provider(InferenceProvider::Grok);
        let original_id = state.active_conversation_id;
        let conversation = state.active_conversation_mut();
        conversation.agent_enabled = true;
        conversation.workspace = Some("/fixture/workspace".into());
        conversation.messages = vec![ChatMessage::new_user("Implement the feature"), ChatMessage::new_assistant("Saved progress")];
        state.switch_provider(InferenceProvider::Gpt);
        assert_eq!(state.active_conversation_id, original_id);
        assert_eq!(state.conversations.len(), 1);
        assert_eq!(state.active_conversation().workspace.as_deref(), Some(std::path::Path::new("/fixture/workspace")));
        assert_eq!(state.active_conversation().messages.len(), 2);
        let session = state.active_conversation().gpt.as_ref().unwrap();
        assert!(session.handoff_from_taceta && session.pending_handoff);
        assert!(session.thread_id.is_none());
        // No account action is performed by a persisted-state transition.
        state.switch_provider(InferenceProvider::Grok);
        assert_ne!(state.active_conversation_id, original_id);
        assert_eq!(state.conversations.iter().find(|chat| chat.id == original_id).unwrap().messages.len(), 2);
    }

    #[test]
    fn gpt_durable_binding_recovers_a_thread_without_eframe_state_and_archives_locally() {
        let root = std::env::temp_dir().join(format!("taceta-binding-test-{}", Uuid::new_v4()));
        let mut conversation = Conversation::for_provider(InferenceProvider::Gpt);
        conversation.title = "Saved coding work".into();
        conversation.workspace = Some("/fixture/project".into());
        conversation.gpt.as_mut().unwrap().thread_id = Some("durable-codex-thread".into());
        save_gpt_binding(&root, &conversation).unwrap();
        let mut state = PersistedAppState::default();
        assert_eq!(recover_gpt_bindings(&mut state, &root).unwrap(), 1);
        assert!(state.select_conversation(conversation.id));
        assert_eq!(state.inference_provider, InferenceProvider::Gpt);
        assert_eq!(state.active_conversation().workspace, conversation.workspace);
        assert_eq!(state.active_conversation().gpt.as_ref().unwrap().thread_id.as_deref(), Some("durable-codex-thread"));
        archive_gpt_binding(&root, conversation.id).unwrap();
        let mut fresh = PersistedAppState::default();
        assert_eq!(recover_gpt_bindings(&mut fresh, &root).unwrap(), 0);
        let archived = std::fs::read_dir(root.join("archived")).unwrap().next().unwrap().unwrap().path();
        assert!(!std::fs::read(&archived).unwrap().is_empty());
        std::fs::remove_file(archived).unwrap();
        std::fs::remove_dir(root.join("archived")).unwrap();
        std::fs::remove_dir(root).unwrap();
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
}
