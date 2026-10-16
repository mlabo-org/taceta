use super::*;

impl TacetaApp {
    pub(super) fn is_generating(&self) -> bool {
        self.generation.is_some() || self.agent_ui.active.is_some()
            || self.cli_ui.active.is_some() || self.compaction.is_some()
    }

    pub(super) fn can_change_provider(&self) -> bool {
        !self.is_generating()
            && self.model_pull.is_none()
            && self.model_unload_result.is_none()
    }

    pub(super) fn select_provider(&mut self, provider: InferenceProvider) {
        if provider == self.state.inference_provider || !self.can_change_provider() {
            return;
        }
        // Each engine keeps Chat or Code in a field of its own, so the
        // choice is carried over. A conversation with a task record stays Code.
        let code = self.code_mode();
        self.state.switch_provider(provider);
        let fixed = provider == InferenceProvider::Ollama && self.agent_ui.has_record(self.state.active_conversation_id);
        if !fixed {
            self.set_code_mode(code);
        }
        self.bind_selected_provider();
    }

    pub(super) fn bind_selected_provider(&mut self) {
        self.backend = match self.state.inference_provider {
            InferenceProvider::Ollama => Some(Arc::new(
                OllamaClient::new(self.ollama_endpoint.clone())
                    .with_link_service(Arc::clone(&self.link_service)),
            ) as Arc<dyn InferenceBackend>),
            InferenceProvider::Grok | InferenceProvider::Gpt | InferenceProvider::Claude => None,
        };
        self.invalidate_model_list();
        self.notice = None;
        self.refresh_models();
    }

    pub(super) fn select_saved_conversation(&mut self, id: Uuid) {
        if self.is_generating() {
            return;
        }
        let previous = self.state.inference_provider;
        if self.state.select_conversation(id) && previous != self.state.inference_provider {
            self.bind_selected_provider();
        }
    }

    pub(super) fn show_provider_selector(&mut self, ui: &mut Ui) {
        let mut selected = self.state.inference_provider;
        ui.add_enabled_ui(self.can_change_provider(), |ui| {
            egui::ComboBox::from_id_salt("taceta-inference-provider")
                .selected_text(selected.label())
                .width(126.0)
                .show_ui(ui, |ui| {
                    for provider in [InferenceProvider::Ollama, InferenceProvider::Grok, InferenceProvider::Gpt, InferenceProvider::Claude] {
                        ui.selectable_value(&mut selected, provider, provider.label());
                    }
                });
        });
        self.select_provider(selected);
    }

    pub(super) fn show_provider_settings(&mut self, ui: &mut Ui) {
        let language = self.language();
        let palette = theme::palette(ui);
        theme::card(ui.visuals().faint_bg_color, palette.border, 14, 16).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.strong(text(language, "推論の接続先", "Inference provider"));
            ui.add_space(4.0);
            ui.label(RichText::new(text(language,
                "会話に使うエンジンです。Ollamaは下で設定したローカルのサーバー、GPT・Grok・Claudeは各社の公式CLIを使います。",
                "The engine conversations use. Ollama uses the local server set below; GPT, Grok and Claude use each vendor's official CLI.")).weak());
            ui.add_space(10.0);
            self.show_provider_selector(ui);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::model_manager_tests::{RecordingServices, test_app};

    #[derive(Default)]
    struct MemoryStorage(std::collections::HashMap<String, String>);
    impl eframe::Storage for MemoryStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.into(), value);
        }
        fn flush(&mut self) {}
    }

    #[test]
    fn undecodable_saved_state_is_kept_verbatim_when_a_new_state_replaces_it() {
        use eframe::Storage;
        const KEY: &str = "taceta.application-state.v1";
        let mut storage = MemoryStorage::default();
        assert!(load_app_state(Some(&storage)).unreadable.is_none());

        let mut state = load_app_state(Some(&storage)).state;
        state.ollama_custom_endpoint = "http://127.0.0.1:23456".into();
        save_app_state(&mut storage, &state, &mut None);
        let readable = storage.get_string(KEY).unwrap();
        assert!(load_app_state(Some(&storage)).unreadable.is_none());
        assert_eq!(storage.0.len(), 1);

        let from_newer_build = readable.replace("Ollama", "ProviderFromANewerBuild");
        assert_ne!(from_newer_build, readable);
        storage.set_string(KEY, from_newer_build.clone());
        let loaded = load_app_state(Some(&storage));
        assert_eq!(loaded.unreadable.as_deref(), Some(from_newer_build.as_str()));
        assert_eq!(loaded.state.ollama_custom_endpoint, PersistedAppState::default().ollama_custom_endpoint);

        let mut unreadable = loaded.unreadable;
        save_app_state(&mut storage, &loaded.state, &mut unreadable);
        assert!(unreadable.is_none());
        let kept: Vec<_> = storage
            .0
            .iter()
            .filter(|(key, _)| key.starts_with(crate::persistence::UNREADABLE_APP_STATE_KEY_PREFIX))
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].1, &from_newer_build);
        assert!(load_app_state(Some(&storage)).unreadable.is_none());
    }

    #[test]
    fn provider_and_workspace_survive_storage_without_changing_old_defaults() {
        let mut storage = MemoryStorage::default();
        let mut state = load_app_state(Some(&storage)).state;
        assert_eq!(state.inference_provider, InferenceProvider::Ollama);
        assert!(!state.active_conversation().agent_enabled);
        state.switch_provider(InferenceProvider::Grok);
        state
            .provider_models
            .insert(InferenceProvider::Ollama, "local-model".into());
        state
            .provider_models
            .insert(InferenceProvider::Grok, "grok-model".into());
        state.selected_model = Some("grok-model".into());
        state.active_conversation_mut().agent_enabled = true;
        state.active_conversation_mut().workspace = Some(PathBuf::from("/fixture/workspace"));
        state.ollama_custom_endpoint = "http://127.0.0.1:23456".into();
        state.agent_max_steps = 120;
        state.agent_max_duration_secs = 7_200;
        state.show_thinking_trace = true;
        assert!(state.local_system_prompt.is_none());
        state.local_system_prompt = Some(String::new());
        save_app_state(&mut storage, &state, &mut None);
        let restored = load_app_state(Some(&storage)).state;
        assert_eq!(restored.inference_provider, InferenceProvider::Grok);
        assert_eq!(
            restored.provider_models[&InferenceProvider::Ollama],
            "local-model"
        );
        assert_eq!(restored.selected_model.as_deref(), Some("grok-model"));
        assert_eq!(
            restored.active_conversation().workspace,
            state.active_conversation().workspace
        );
        assert_eq!(restored.ollama_custom_endpoint, "http://127.0.0.1:23456");
        assert_eq!(restored.agent_max_steps, 120);
        assert_eq!(restored.agent_max_duration_secs, 7_200);
        assert!(restored.show_thinking_trace);
        // An emptied prompt stays empty rather than returning to the default.
        assert_eq!(restored.local_system_prompt.as_deref(), Some(""));
    }

    #[test]
    fn stale_model_reply_cannot_overwrite_new_provider_or_enable_local_model_mutations() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        app.state.inference_provider = InferenceProvider::Grok;
        app.model_list_epoch = 9;
        app.model_refresh_pending = true;
        app.model_result_tx
            .send((8, Err("old Ollama failure".into())))
            .unwrap();
        app.model_result_tx
            .send((
                9,
                Ok(vec![ModelDescriptor {
                    name: "grok-test".into(),
                    size: 0,
                    thinking: ThinkingCapability::Unverified,
                    vision: false,
                    tools: true,
                    context_length: Some(131_072),
                    remote_host: None,
                }]),
            ))
            .unwrap();
        app.drain_background_work();
        assert_eq!(app.state.selected_model.as_deref(), Some("grok-test"));
        assert_eq!(app.models.len(), 1);
        assert!(!app.model_refresh_pending);
        assert!(matches!(app.connection, ConnectionState::Ready));
        assert!(!app.can_unload_model());
    }

    #[test]
    fn changing_the_ollama_endpoint_discards_the_list_request_still_in_flight() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        app.model_list_epoch = 4;
        app.model_refresh_pending = true;
        let endpoint = OllamaEndpoint::resolve_from_values(OllamaEndpointMode::Custom, "http://127.0.0.1:23456", None, None).unwrap();
        assert!(app.bind_ollama_endpoint(endpoint));
        assert!(!app.model_refresh_pending);
        app.model_result_tx
            .send((4, Ok(vec![ModelDescriptor {
                name: "from-the-old-endpoint".into(),
                size: 0,
                thinking: ThinkingCapability::Unverified,
                vision: false,
                tools: true,
                context_length: None,
                remote_host: None,
            }])))
            .unwrap();
        app.drain_background_work();
        assert!(app.models.is_empty());
    }
}
