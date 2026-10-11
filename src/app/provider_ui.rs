use super::*;
use taceta::backend::{GrokClient, GrokLoginEvent};

pub(super) struct LoginTask {
    pub task: JoinHandle<()>,
    events: mpsc::UnboundedReceiver<GrokLoginEvent>,
    result: std_mpsc::Receiver<Result<(), String>>,
}

pub(super) struct ProviderUiState {
    pub grok: Arc<GrokClient>,
    pub login: Option<LoginTask>,
}

impl Default for ProviderUiState {
    fn default() -> Self {
        Self {
            grok: Arc::new(GrokClient::new()),
            login: None,
        }
    }
}

impl TacetaApp {
    pub(super) fn is_generating(&self) -> bool {
        self.generation.is_some() || self.agent_ui.active.is_some() || self.gpt_ui.active.is_some()
            || self.gpt_ui.is_transferring()
    }

    fn can_change_provider(&self) -> bool {
        !self.is_generating()
            && self.model_pull.is_none()
            && self.model_unload_result.is_none()
            && self.provider_ui.login.is_none()
            && !self.gpt_ui.auth_busy()
    }

    pub(super) fn select_provider(&mut self, provider: InferenceProvider) {
        if provider == self.state.inference_provider || !self.can_change_provider() {
            return;
        }
        if provider == InferenceProvider::Grok && self.state.active_conversation().requires_gpt_reverse_transfer() {
            self.start_gpt_reverse_transfer();
            return;
        }
        self.state.switch_provider(provider);
        self.bind_selected_provider();
    }

    pub(super) fn bind_selected_provider(&mut self) {
        self.backend = match self.state.inference_provider {
            InferenceProvider::Ollama => Some(Arc::new(
                OllamaClient::new(self.ollama_endpoint.clone())
                    .with_link_service(Arc::clone(&self.link_service)),
            ) as Arc<dyn InferenceBackend>),
            InferenceProvider::Grok => {
                Some(Arc::clone(&self.provider_ui.grok) as Arc<dyn InferenceBackend>)
            }
            InferenceProvider::Gpt => None,
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
                    for provider in [InferenceProvider::Ollama, InferenceProvider::Grok, InferenceProvider::Gpt] {
                        ui.selectable_value(&mut selected, provider, provider.label());
                    }
                });
        });
        self.select_provider(selected);
    }

    pub(super) fn show_provider_settings(&mut self, ui: &mut Ui) {
        let language = self.language();
        ui.heading(text(language, "推論の接続先", "Inference provider"));
        self.show_provider_selector(ui);
        ui.label(text(language,
            "Ollamaは設定したサーバーを使います。GrokはxAI、GPTはOpenAIへ会話と作業に必要な内容を送ります。GPTはCodexの会話履歴を使います。",
            "Ollama uses your configured server. Grok sends task context to xAI; GPT sends it to OpenAI. GPT conversations use Codex history."));
        ui.add_space(8.0);
        if self.state.inference_provider == InferenceProvider::Gpt {
            self.show_gpt_settings(ui);
            ui.add_space(20.0);
            return;
        }
        if self.state.inference_provider == InferenceProvider::Ollama {
            ui.add_space(20.0);
            return;
        }
        ui.horizontal(|ui| {
            if self.provider_ui.login.is_some() {
                ui.spinner();
                ui.label(text(
                    language,
                    "ブラウザーでGrokのログインを完了してください。",
                    "Complete Grok sign-in in your browser.",
                ));
                if ui.button(text(language, "中止", "Cancel")).clicked() {
                    if let Some(login) = self.provider_ui.login.take() {
                        login.task.abort();
                    }
                    self.notice = Some(Notice {
                        kind: NoticeKind::Info,
                        text: text(
                            language,
                            "Grok接続を中止しました。",
                            "Grok sign-in cancelled.",
                        )
                        .into(),
                    });
                }
            } else {
                if ui
                    .add_enabled(
                        !self.is_generating(),
                        Button::new(text(
                            language,
                            "Grokに接続（OAuth）",
                            "Connect Grok (OAuth)",
                        )),
                    )
                    .clicked()
                {
                    self.start_grok_login();
                }
                if ui
                    .add_enabled(
                        !self.is_generating(),
                        Button::new(text(language, "Grok接続を解除", "Disconnect Grok")),
                    )
                    .clicked()
                {
                    match self.provider_ui.grok.sign_out() {
                        Ok(()) => {
                            if self.state.inference_provider == InferenceProvider::Grok {
                                self.invalidate_model_list();
                                self.state.selected_model = None;
                                self.connection = ConnectionState::Unavailable(
                                    text(
                                        language,
                                        "Grokへの再接続が必要です。",
                                        "Reconnect Grok to continue.",
                                    )
                                    .into(),
                                );
                            }
                            self.notice = Some(Notice {
                                kind: NoticeKind::Info,
                                text: text(
                                    language,
                                    "このTacetaのGrok認証を削除しました。",
                                    "Removed this Taceta's Grok credentials.",
                                )
                                .into(),
                            });
                        }
                        Err(error) => {
                            self.notice = Some(Notice {
                                kind: NoticeKind::Error,
                                text: error,
                            })
                        }
                    }
                }
            }
        });
        ui.label(RichText::new(text(language,
            "認証には公式Grok CLIを使います。認証情報はこのMacのTaceta専用フォルダーに保存します。",
            "Authentication uses the official Grok CLI. Credentials are stored in Taceta's own folder on this Mac.")).small().weak());
        ui.add_space(20.0);
    }

    fn start_grok_login(&mut self) {
        if self.provider_ui.login.is_some() || self.is_generating() {
            return;
        }
        let grok = Arc::clone(&self.provider_ui.grok);
        let (tx, events) = mpsc::unbounded_channel();
        let (result_tx, result) = std_mpsc::channel();
        let task = self.runtime.spawn(async move {
            let outcome = grok.sign_in(tx).await;
            let _ = result_tx.send(outcome);
        });
        self.provider_ui.login = Some(LoginTask {
            task,
            events,
            result,
        });
        self.notice = None;
    }

    pub(super) fn drain_provider_work(&mut self) {
        let mut events = Vec::new();
        let mut outcome = None;
        if let Some(login) = self.provider_ui.login.as_mut() {
            while let Ok(event) = login.events.try_recv() {
                events.push(event);
            }
            outcome = match login.result.try_recv() {
                Ok(result) => Some(result),
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Grok sign-in stopped before completion".into()))
                }
                Err(std_mpsc::TryRecvError::Empty) => None,
            };
        }
        for event in events {
            match event {
                GrokLoginEvent::OpenBrowser(url) => {
                    if let Err(error) = Command::new("/usr/bin/open").arg(url).spawn() {
                        if let Some(login) = self.provider_ui.login.take() {
                            login.task.abort();
                        }
                        outcome = Some(Err(format!(
                            "Could not open the Grok sign-in page: {error}"
                        )));
                    }
                }
                GrokLoginEvent::Progress(progress) => {
                    self.notice = Some(Notice {
                        kind: NoticeKind::Info,
                        text: progress,
                    });
                }
            }
        }
        if let Some(outcome) = outcome {
            self.provider_ui.login = None;
            match outcome {
                Ok(()) => {
                    if self.state.inference_provider == InferenceProvider::Grok {
                        self.invalidate_model_list();
                        self.refresh_models();
                    } else {
                        self.select_provider(InferenceProvider::Grok);
                    }
                    self.notice = Some(Notice {
                        kind: NoticeKind::Info,
                        text: text(
                            self.language(),
                            "Grok認証を保存しました。利用可能なモデルを取得しています。",
                            "Grok credentials saved. Loading available models.",
                        )
                        .into(),
                    });
                }
                Err(error) => {
                    self.notice = Some(Notice {
                        kind: NoticeKind::Error,
                        text: error,
                    })
                }
            }
        }
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
            }])))
            .unwrap();
        app.drain_background_work();
        assert!(app.models.is_empty());
    }
}
