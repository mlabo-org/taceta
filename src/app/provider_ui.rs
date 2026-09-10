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
        self.generation.is_some() || self.agent_ui.active.is_some()
    }

    fn can_change_provider(&self) -> bool {
        !self.is_generating()
            && self.model_pull.is_none()
            && self.model_unload_result.is_none()
            && self.provider_ui.login.is_none()
    }

    pub(super) fn select_provider(&mut self, provider: InferenceProvider) {
        if provider == self.state.inference_provider || !self.can_change_provider() {
            return;
        }
        if let Some(model) = self.state.selected_model.take() {
            self.state
                .provider_models
                .insert(self.state.inference_provider, model);
        }
        self.state.inference_provider = provider;
        self.state.selected_model = self.state.provider_models.get(&provider).cloned();
        self.backend = match provider {
            InferenceProvider::Ollama => Arc::new(
                OllamaClient::new(self.ollama_endpoint.clone())
                    .with_link_service(Arc::clone(&self.link_service)),
            ),
            InferenceProvider::Grok => {
                Arc::clone(&self.provider_ui.grok) as Arc<dyn InferenceBackend>
            }
        };
        // A model list belongs to the request that produced it. A late reply
        // from the previous endpoint cannot replace this provider's models.
        self.model_list_epoch = self.model_list_epoch.wrapping_add(1);
        self.model_refresh_pending = false;
        self.models.clear();
        self.notice = None;
        self.refresh_models();
    }

    pub(super) fn show_provider_selector(&mut self, ui: &mut Ui) {
        let mut selected = self.state.inference_provider;
        ui.add_enabled_ui(self.can_change_provider(), |ui| {
            egui::ComboBox::from_id_salt("taceta-inference-provider")
                .selected_text(selected.label())
                .width(126.0)
                .show_ui(ui, |ui| {
                    for provider in [InferenceProvider::Ollama, InferenceProvider::Grok] {
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
            "Ollamaは設定したサーバー、GrokはxAIのサーバーで生成します。Grokを選ぶと、会話と作業に必要なファイル内容がxAIへ送られます。",
            "Ollama uses your configured server. Grok sends conversation context and the file content needed for your task to xAI."));
        ui.add_space(8.0);
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
                                self.model_list_epoch = self.model_list_epoch.wrapping_add(1);
                                self.model_refresh_pending = false;
                                self.models.clear();
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
            "公開されたGrok BuildのOAuth方式を利用する非公式の接続です。認証情報はTaceta専用のmacOSキーチェーンに保存します。",
            "An unofficial connection using Grok Build's public OAuth flow. Credentials are kept in Taceta's own macOS Keychain entry.")).small().weak());
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
    fn provider_and_workspace_survive_storage_without_changing_old_defaults() {
        let mut storage = MemoryStorage::default();
        let mut state = load_app_state(Some(&storage));
        assert_eq!(state.inference_provider, InferenceProvider::Ollama);
        assert!(!state.active_conversation().agent_enabled);
        state.inference_provider = InferenceProvider::Grok;
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
        save_app_state(&mut storage, &state);
        let restored = load_app_state(Some(&storage));
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
}
