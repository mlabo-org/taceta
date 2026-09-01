use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::{Arc, mpsc as std_mpsc},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use eframe::egui::{
    self, Align, Button, CentralPanel, ComboBox, Context, Key, Layout, Panel, RichText, ScrollArea,
    Spinner, TextEdit, Ui, Vec2,
};
use taceta::{
    backend::{InferenceBackend, OllamaClient},
    domain::{
        Attachment, AttachmentPayload, ChatMessage, ChatRequest, GenerationEvent, ModelDescriptor,
        Role, ThinkingCapability, ThinkingLevel, ThinkingMode,
    },
};
use tokio::{runtime::Runtime, sync::mpsc, task::JoinHandle};
use uuid::Uuid;

use crate::{
    app_shell_foundation::{
        AppShellLanguage, AppShellPreferences, apply_app_shell_preferences,
        install_macos_system_fonts, load_app_shell_preferences, save_app_shell_preferences,
        show_app_shell_preferences,
    },
    localization::{conversation_title, system_language, text},
    persistence::{
        CONTEXT_LENGTH_OPTIONS, DEFAULT_CONTEXT_LENGTH, PersistedAppState, load_app_state,
        save_app_state,
    },
    ui::theme::{self, TacetaPalette},
};

const APP_SHELL_STORAGE_KEY: &str = "taceta.app-shell-preferences.v1";
const LOCAL_ENGINE_URL: &str = "http://127.0.0.1:11434";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Screen {
    Chat,
    Settings,
}

enum ConnectionState {
    Connecting,
    Ready,
    Unavailable(String),
}

#[derive(Clone, Copy)]
enum NoticeKind {
    Info,
    Warning,
    Error,
}

struct Notice {
    kind: NoticeKind,
    text: String,
}

struct ActiveGeneration {
    conversation_id: Uuid,
    assistant_id: Uuid,
    task: JoinHandle<()>,
    events: mpsc::UnboundedReceiver<GenerationEvent>,
    result: std_mpsc::Receiver<Result<(), String>>,
}

pub struct TacetaApp {
    shell_preferences: AppShellPreferences,
    system_language: AppShellLanguage,
    state: PersistedAppState,
    screen: Screen,
    backend: Arc<dyn InferenceBackend>,
    runtime: Runtime,
    model_result_tx: std_mpsc::Sender<Result<Vec<ModelDescriptor>, String>>,
    model_result_rx: std_mpsc::Receiver<Result<Vec<ModelDescriptor>, String>>,
    model_refresh_pending: bool,
    models: Vec<ModelDescriptor>,
    connection: ConnectionState,
    model_storage_path: PathBuf,
    generation: Option<ActiveGeneration>,
    notice: Option<Notice>,
    scroll_to_bottom: bool,
}

impl TacetaApp {
    pub fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        install_macos_system_fonts(&creation_context.egui_ctx)
            .expect("Taceta could not install its managed macOS UI fonts");
        let shell_preferences =
            load_app_shell_preferences(creation_context.storage, APP_SHELL_STORAGE_KEY);
        apply_app_shell_preferences(&creation_context.egui_ctx, shell_preferences);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("taceta-runtime")
            .enable_all()
            .build()
            .expect("Taceta could not start its local async runtime");
        let (model_result_tx, model_result_rx) = std_mpsc::channel();

        let mut app = Self {
            shell_preferences,
            system_language: system_language(),
            state: load_app_state(creation_context.storage),
            screen: Screen::Chat,
            backend: Arc::new(OllamaClient::new(LOCAL_ENGINE_URL)),
            runtime,
            model_result_tx,
            model_result_rx,
            model_refresh_pending: false,
            models: Vec::new(),
            connection: ConnectionState::Connecting,
            model_storage_path: resolve_model_storage_path(),
            generation: None,
            notice: None,
            scroll_to_bottom: true,
        };
        app.refresh_models();
        app
    }

    fn language(&self) -> AppShellLanguage {
        self.shell_preferences
            .language
            .resolve(self.system_language)
    }

    fn refresh_models(&mut self) {
        if self.model_refresh_pending {
            return;
        }
        self.model_refresh_pending = true;
        self.connection = ConnectionState::Connecting;
        let backend = Arc::clone(&self.backend);
        let result_tx = self.model_result_tx.clone();
        self.runtime.spawn(async move {
            let result = backend
                .list_models()
                .await
                .map_err(|error| error.to_string());
            let _ = result_tx.send(result);
        });
    }

    fn drain_background_work(&mut self) {
        if let Ok(result) = self.model_result_rx.try_recv() {
            self.model_refresh_pending = false;
            match result {
                Ok(mut models) => {
                    models.sort_by(|left, right| left.name.cmp(&right.name));
                    self.models = models;
                    self.connection = ConnectionState::Ready;
                    let selected_exists =
                        self.state.selected_model.as_ref().is_some_and(|name| {
                            self.models.iter().any(|model| &model.name == name)
                        });
                    if !selected_exists {
                        self.state.selected_model =
                            self.models.first().map(|model| model.name.clone());
                    }
                    self.ensure_selected_thinking_mode();
                }
                Err(error) => {
                    let error = self.safe_backend_error(&error);
                    self.connection = ConnectionState::Unavailable(error.clone());
                    self.notice = Some(Notice {
                        kind: NoticeKind::Error,
                        text: error,
                    });
                }
            }
        }

        let mut events = Vec::new();
        let mut result = None;
        let mut target = None;
        if let Some(active) = self.generation.as_mut() {
            target = Some((active.conversation_id, active.assistant_id));
            while let Ok(event) = active.events.try_recv() {
                events.push(event);
            }
            if let Ok(generation_result) = active.result.try_recv() {
                result = Some(generation_result);
            }
        }

        if let Some((conversation_id, assistant_id)) = target {
            for event in events {
                self.apply_generation_event(conversation_id, assistant_id, event);
            }
            if let Some(generation_result) = result {
                match generation_result {
                    Ok(()) => {
                        self.connection = ConnectionState::Ready;
                    }
                    Err(error) => {
                        let error = self.safe_backend_error(&error);
                        self.connection = ConnectionState::Unavailable(error.clone());
                        self.notice = Some(Notice {
                            kind: NoticeKind::Error,
                            text: error.clone(),
                        });
                        self.update_assistant(conversation_id, assistant_id, |message| {
                            if message.content.is_empty() {
                                message.content = error;
                            }
                        });
                    }
                }
                self.generation = None;
            }
        }
    }

    fn apply_generation_event(
        &mut self,
        conversation_id: Uuid,
        assistant_id: Uuid,
        event: GenerationEvent,
    ) {
        match event {
            GenerationEvent::ThinkingDelta(delta) => {
                self.update_assistant(conversation_id, assistant_id, |message| {
                    message.thinking.push_str(&delta);
                });
            }
            GenerationEvent::ContentDelta(delta) => {
                self.update_assistant(conversation_id, assistant_id, |message| {
                    message.content.push_str(&delta);
                });
            }
            GenerationEvent::Completed(_) => {}
        }
        self.scroll_to_bottom = true;
    }

    fn update_assistant(
        &mut self,
        conversation_id: Uuid,
        assistant_id: Uuid,
        update: impl FnOnce(&mut ChatMessage),
    ) {
        if let Some(message) = self
            .state
            .conversations
            .iter_mut()
            .find(|conversation| conversation.id == conversation_id)
            .and_then(|conversation| {
                conversation
                    .messages
                    .iter_mut()
                    .find(|message| message.id == assistant_id)
            })
        {
            update(message);
        }
    }

    fn safe_backend_error(&self, error: &str) -> String {
        let language = self.language();
        error.replace("Ollama", text(language, "ローカルエンジン", "local engine"))
    }

    fn selected_model(&self) -> Option<&ModelDescriptor> {
        let selected = self.state.selected_model.as_ref()?;
        self.models.iter().find(|model| &model.name == selected)
    }

    fn default_thinking_mode(capability: ThinkingCapability) -> ThinkingMode {
        match capability {
            ThinkingCapability::None | ThinkingCapability::Unverified => ThinkingMode::Default,
            ThinkingCapability::Toggle => ThinkingMode::On,
            ThinkingCapability::Levels => ThinkingMode::Level(ThinkingLevel::Low),
        }
    }

    fn normalized_thinking_mode(
        capability: ThinkingCapability,
        mode: ThinkingMode,
    ) -> ThinkingMode {
        match (capability, mode) {
            (ThinkingCapability::Toggle, ThinkingMode::Off | ThinkingMode::On) => mode,
            (ThinkingCapability::Levels, ThinkingMode::Level(_)) => mode,
            (ThinkingCapability::None | ThinkingCapability::Unverified, ThinkingMode::Default) => {
                mode
            }
            _ => Self::default_thinking_mode(capability),
        }
    }

    fn ensure_selected_thinking_mode(&mut self) {
        let Some(model) = self.selected_model().cloned() else {
            return;
        };
        let current = self
            .state
            .thinking_modes
            .get(&model.name)
            .copied()
            .unwrap_or_else(|| Self::default_thinking_mode(model.thinking));
        self.state.thinking_modes.insert(
            model.name,
            Self::normalized_thinking_mode(model.thinking, current),
        );
    }

    fn selected_thinking_mode(&self) -> ThinkingMode {
        self.selected_model()
            .map(|model| {
                self.state
                    .thinking_modes
                    .get(&model.name)
                    .copied()
                    .map(|mode| Self::normalized_thinking_mode(model.thinking, mode))
                    .unwrap_or_else(|| Self::default_thinking_mode(model.thinking))
            })
            .unwrap_or(ThinkingMode::Default)
    }

    fn attach_files(&mut self) {
        let language = self.language();
        let Some(paths) = rfd::FileDialog::new().pick_files() else {
            return;
        };

        for path in paths {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("attachment")
                .to_owned();
            let extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();

            let payload = if let Some(media_type) = image_media_type(&extension) {
                match fs::read(&path) {
                    Ok(bytes) => AttachmentPayload::Image {
                        media_type: media_type.to_owned(),
                        base64: BASE64.encode(bytes),
                    },
                    Err(error) => {
                        self.notice = Some(Notice {
                            kind: NoticeKind::Error,
                            text: format!(
                                "{}: {name} ({error})",
                                text(
                                    language,
                                    "画像を読み込めませんでした",
                                    "Could not read image"
                                )
                            ),
                        });
                        continue;
                    }
                }
            } else {
                match fs::read_to_string(&path) {
                    Ok(contents) => AttachmentPayload::Text(contents),
                    Err(error) => {
                        self.notice = Some(Notice {
                            kind: NoticeKind::Warning,
                            text: format!(
                                "{}: {name} ({error})",
                                text(
                                    language,
                                    "UTF-8テキストとして読めないため追加しませんでした",
                                    "The file was not added because it is not readable UTF-8 text"
                                )
                            ),
                        });
                        continue;
                    }
                }
            };
            self.state
                .pending_attachments
                .push(Attachment { name, payload });
        }
    }

    fn start_generation(&mut self) {
        if self.generation.is_some() {
            return;
        }
        let language = self.language();
        let Some(model) = self.selected_model().cloned() else {
            self.notice = Some(Notice {
                kind: NoticeKind::Warning,
                text: text(
                    language,
                    "利用するモデルを選択してください",
                    "Select a model before sending",
                )
                .to_owned(),
            });
            return;
        };
        if self.state.draft.trim().is_empty() && self.state.pending_attachments.is_empty() {
            return;
        }
        if !model.vision
            && self
                .state
                .pending_attachments
                .iter()
                .any(|attachment| matches!(attachment.payload, AttachmentPayload::Image { .. }))
        {
            self.notice = Some(Notice {
                kind: NoticeKind::Warning,
                text: text(
                    language,
                    "このモデルは画像入力に対応していません。画像は送信されませんでした。",
                    "This model does not support image input. Nothing was sent.",
                )
                .to_owned(),
            });
            return;
        }

        let draft = std::mem::take(&mut self.state.draft);
        let attachments = std::mem::take(&mut self.state.pending_attachments);
        let attachment_fallback = attachments
            .first()
            .map(|attachment| attachment.name.clone())
            .unwrap_or_else(|| "Taceta".to_owned());
        let mut user_message = ChatMessage::new_user(draft.clone());
        user_message.attachments = attachments;

        let conversation = self.state.active_conversation_mut();
        if conversation.messages.is_empty() {
            conversation.title = conversation_title(&draft, &attachment_fallback);
        }
        let conversation_id = conversation.id;
        conversation.messages.push(user_message);
        let request_messages = conversation.messages.clone();
        let assistant_message = ChatMessage::new_assistant("");
        let assistant_id = assistant_message.id;
        conversation.messages.push(assistant_message);

        let request = ChatRequest {
            model: model.name,
            messages: request_messages,
            thinking: self.selected_thinking_mode(),
            context_length: self.state.context_length,
        };
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (result_tx, result_rx) = std_mpsc::channel();
        let backend = Arc::clone(&self.backend);
        let task = self.runtime.spawn(async move {
            let result = backend
                .stream_chat(request, event_tx)
                .await
                .map_err(|error| error.to_string());
            let _ = result_tx.send(result);
        });

        self.generation = Some(ActiveGeneration {
            conversation_id,
            assistant_id,
            task,
            events: event_rx,
            result: result_rx,
        });
        self.notice = None;
        self.scroll_to_bottom = true;
    }

    fn stop_generation(&mut self) {
        let Some(active) = self.generation.take() else {
            return;
        };
        active.task.abort();
        let language = self.language();
        self.update_assistant(active.conversation_id, active.assistant_id, |message| {
            if message.content.is_empty() {
                message.content =
                    text(language, "生成を停止しました。", "Generation stopped.").to_owned();
            }
        });
        self.notice = Some(Notice {
            kind: NoticeKind::Info,
            text: text(language, "生成を停止しました", "Generation stopped").to_owned(),
        });
    }

    fn reveal_model_storage(&mut self) {
        let language = self.language();
        if !self.model_storage_path.exists() {
            self.notice = Some(Notice {
                kind: NoticeKind::Warning,
                text: format!(
                    "{}: {}",
                    text(
                        language,
                        "モデル格納先が見つかりません",
                        "Model storage location was not found"
                    ),
                    self.model_storage_path.display()
                ),
            });
            return;
        }
        if let Err(error) = Command::new("open").arg(&self.model_storage_path).spawn() {
            self.notice = Some(Notice {
                kind: NoticeKind::Error,
                text: format!(
                    "{} ({error})",
                    text(
                        language,
                        "Finderで格納先を開けませんでした",
                        "Could not reveal the model storage location"
                    )
                ),
            });
        }
    }

    fn show_sidebar(&mut self, root_ui: &mut Ui) {
        let language = self.language();
        let history = self
            .state
            .conversations
            .iter()
            .map(|conversation| {
                (
                    conversation.id,
                    conversation.title.clone(),
                    conversation.messages.is_empty(),
                )
            })
            .collect::<Vec<_>>();
        let active_id = self.state.active_conversation_id;
        let generating = self.generation.is_some();

        let sidebar_fill = theme::palette(root_ui).sidebar;
        Panel::left("taceta-sidebar")
            .exact_size(238.0)
            .resizable(false)
            .frame(egui::Frame::side_top_panel(root_ui.style()).fill(sidebar_fill))
            .show_inside(root_ui, |ui| {
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.heading(RichText::new("Taceta").size(19.0));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new("tacet").small().weak());
                    });
                });
                ui.add_space(12.0);

                if ui
                    .add_enabled(
                        !generating,
                        Button::new(format!(
                            "＋ {}",
                            text(language, "新しいチャット", "New chat")
                        ))
                        .min_size(Vec2::new(ui.available_width(), 34.0)),
                    )
                    .clicked()
                {
                    self.state.start_new_conversation();
                    self.screen = Screen::Chat;
                }

                ui.add_space(14.0);
                ui.label(
                    RichText::new(text(language, "会話", "Chats"))
                        .small()
                        .weak(),
                );
                ui.add_space(5.0);
                ScrollArea::vertical()
                    .id_salt("conversation-history")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (id, title, empty) in history {
                            let title = if empty {
                                text(language, "新しいチャット", "New chat")
                            } else {
                                &title
                            };
                            if ui
                                .selectable_label(id == active_id, title)
                                .on_hover_text(title)
                                .clicked()
                            {
                                self.state.active_conversation_id = id;
                                self.screen = Screen::Chat;
                                self.scroll_to_bottom = true;
                            }
                        }
                    });

                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    let selected = self.screen == Screen::Settings;
                    if ui
                        .selectable_label(
                            selected,
                            format!("⚙ {}", text(language, "設定", "Settings")),
                        )
                        .clicked()
                    {
                        self.screen = Screen::Settings;
                    }
                    ui.add_space(8.0);
                });
            });
    }

    fn show_top_bar(&mut self, root_ui: &mut Ui) {
        let language = self.language();
        Panel::top("taceta-top-bar")
            .exact_size(44.0)
            .show_inside(root_ui, |ui| {
                ui.horizontal_centered(|ui| {
                    let title = match self.screen {
                        Screen::Chat => self.state.active_conversation().title.as_str(),
                        Screen::Settings => text(language, "設定", "Settings"),
                    };
                    ui.label(RichText::new(title).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(!self.model_refresh_pending, Button::new("↻").small())
                            .on_hover_text(text(
                                language,
                                "接続とモデル一覧を更新",
                                "Refresh connection and models",
                            ))
                            .clicked()
                        {
                            self.refresh_models();
                        }
                        let (color, label, details) = match &self.connection {
                            ConnectionState::Connecting => (
                                theme::palette(ui).warning,
                                text(language, "接続中", "Connecting"),
                                None,
                            ),
                            ConnectionState::Ready => (
                                theme::palette(ui).success,
                                text(language, "ローカル接続", "Local ready"),
                                None,
                            ),
                            ConnectionState::Unavailable(error) => (
                                theme::palette(ui).error,
                                text(language, "未接続", "Unavailable"),
                                Some(error.as_str()),
                            ),
                        };
                        let response = ui.horizontal(|ui| {
                            ui.label(RichText::new("●").color(color).size(10.0));
                            ui.label(RichText::new(label).small().weak());
                        });
                        if let Some(details) = details {
                            response.response.on_hover_text(details);
                        }
                    });
                });
            });
    }

    fn show_notice(&mut self, ui: &mut Ui) {
        let Some(notice) = self.notice.as_ref() else {
            return;
        };
        let palette = theme::palette(ui);
        let color = match notice.kind {
            NoticeKind::Info => palette.accent,
            NoticeKind::Warning => palette.warning,
            NoticeKind::Error => palette.error,
        };
        let text_value = notice.text.clone();
        let mut dismiss = false;
        theme::card(
            color.gamma_multiply(0.10),
            color.gamma_multiply(0.65),
            10,
            10,
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(text_value).color(color));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("×").clicked() {
                        dismiss = true;
                    }
                });
            });
        });
        ui.add_space(8.0);
        if dismiss {
            self.notice = None;
        }
    }

    fn show_settings(&mut self, root_ui: &mut Ui) {
        let language = self.language();
        CentralPanel::default().show_inside(root_ui, |ui| {
            ScrollArea::vertical().show(ui, |ui| {
                let width = ui.available_width().min(680.0);
                ui.horizontal(|ui| {
                    ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
                    ui.vertical(|ui| {
                        ui.set_width(width);
                        ui.add_space(28.0);
                        ui.heading(text(language, "設定", "Settings"));
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(text(
                                language,
                                "表示と言語はこのMacだけに保存されます。",
                                "Display and language preferences stay on this Mac.",
                            ))
                            .weak(),
                        );
                        ui.add_space(20.0);

                        let palette = theme::palette(ui);
                        theme::card(
                            ui.visuals().faint_bg_color,
                            palette.border,
                            14,
                            16,
                        )
                        .show(ui, |ui| {
                            ui.strong(text(language, "表示", "Appearance"));
                            ui.add_space(12.0);
                            let change = show_app_shell_preferences(
                                ui,
                                &mut self.shell_preferences,
                                self.system_language,
                                "taceta-main-settings",
                            );
                            if change.changed {
                                apply_app_shell_preferences(
                                    ui.ctx(),
                                    self.shell_preferences,
                                );
                            }
                        });
                        ui.add_space(14.0);
                        let palette = theme::palette(ui);
                        theme::card(
                            ui.visuals().faint_bg_color,
                            palette.border,
                            14,
                            16,
                        )
                        .show(ui, |ui| {
                            ui.strong(text(language, "モデル格納先", "Model location"));
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(text(
                                    language,
                                    "ローカルモデルが保存されている場所です。",
                                    "Location where local models are stored.",
                                ))
                                .weak(),
                            );
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                let mut path = self.model_storage_path.display().to_string();
                                let button_width = 108.0;
                                ui.add_sized(
                                    [
                                        (ui.available_width() - button_width - 8.0).max(160.0),
                                        30.0,
                                    ],
                                    TextEdit::singleline(&mut path).interactive(false),
                                );
                                if ui
                                    .add_sized(
                                        [button_width, 30.0],
                                        Button::new(text(
                                            language,
                                            "Finderで開く",
                                            "Reveal",
                                        )),
                                    )
                                    .clicked()
                                {
                                    self.reveal_model_storage();
                                }
                            });

                            ui.add_space(20.0);
                            ui.horizontal(|ui| {
                                ui.strong(text(language, "コンテキスト長", "Context length"));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.monospace(format_context_length(
                                        self.state.context_length,
                                    ));
                                });
                            });
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(text(
                                    language,
                                    "会話で保持・参照できる情報量です。長いほどメモリを使い、生成開始が遅くなる場合があります。",
                                    "How much of the conversation the model can retain and use. Longer contexts use more memory and may start more slowly.",
                                ))
                                .weak(),
                            );
                            ui.add_space(10.0);
                            let mut context_index = CONTEXT_LENGTH_OPTIONS
                                .iter()
                                .position(|value| *value == self.state.context_length)
                                .unwrap_or(3) as u32;
                            if ui
                                .add(
                                    egui::Slider::new(&mut context_index, 0..=6)
                                        .integer()
                                        .show_value(false),
                                )
                                .changed()
                            {
                                self.state.context_length =
                                    CONTEXT_LENGTH_OPTIONS[context_index as usize];
                            }
                            ui.horizontal(|ui| {
                                let width = ui.available_width() / 7.0;
                                for value in CONTEXT_LENGTH_OPTIONS {
                                    ui.add_sized(
                                        [width, 16.0],
                                        egui::Label::new(
                                            RichText::new(format_context_length(value))
                                                .small()
                                                .weak(),
                                        ),
                                    );
                                }
                            });
                            ui.add_space(8.0);
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .small_button(text(
                                        language,
                                        "既定値（32k）に戻す",
                                        "Reset to 32k",
                                    ))
                                    .clicked()
                                {
                                    self.state.context_length = DEFAULT_CONTEXT_LENGTH;
                                }
                            });
                        });
                        ui.add_space(14.0);
                        let palette = theme::palette(ui);
                        theme::card(
                            ui.visuals().faint_bg_color,
                            palette.border,
                            14,
                            16,
                        )
                        .show(ui, |ui| {
                            ui.strong(text(language, "ローカル接続", "Local connection"));
                            ui.add_space(8.0);
                            ui.monospace(LOCAL_ENGINE_URL);
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(text(
                                    language,
                                    "会話、添付、設定を外部サービスへ転送しません。",
                                    "Chats, attachments, and settings are not forwarded to a cloud service.",
                                ))
                                .weak(),
                            );
                        });
                    });
                });
            });
        });
    }

    fn show_chat(&mut self, root_ui: &mut Ui) {
        let language = self.language();
        let messages = self.state.active_conversation().messages.clone();
        let active_generation_target = self
            .generation
            .as_ref()
            .map(|generation| generation.assistant_id);
        CentralPanel::default().show_inside(root_ui, |ui| {
            let available = ui.available_width();
            let reading_width = available.min(820.0);
            ui.horizontal(|ui| {
                ui.add_space(((available - reading_width) / 2.0).max(0.0));
                ui.vertical(|ui| {
                    ui.set_width(reading_width);
                    self.show_notice(ui);
                    ScrollArea::vertical()
                        .id_salt("chat-transcript")
                        .auto_shrink([false, false])
                        .stick_to_bottom(self.scroll_to_bottom || self.generation.is_some())
                        .show(ui, |ui| {
                            ui.add_space(18.0);
                            if messages.is_empty() {
                                ui.add_space(110.0);
                                ui.vertical_centered(|ui| {
                                    ui.heading(RichText::new("Taceta").size(26.0));
                                    ui.add_space(8.0);
                                    ui.label(
                                        RichText::new(text(
                                            language,
                                            "深く考える。経過は、必要な時だけ。",
                                            "Think deeply. Show the trace only when you need it.",
                                        ))
                                        .weak(),
                                    );
                                });
                            }
                            for message in &messages {
                                self.show_message(
                                    ui,
                                    message,
                                    active_generation_target == Some(message.id),
                                );
                                ui.add_space(18.0);
                            }
                            ui.add_space(12.0);
                        });
                    self.scroll_to_bottom = false;
                });
            });
        });
    }

    fn show_message(&self, ui: &mut Ui, message: &ChatMessage, generating: bool) {
        let language = self.language();
        let palette = theme::palette(ui);
        let available = ui.available_width();
        let max_width = match message.role {
            Role::User => available.min(650.0),
            _ => available.min(760.0),
        };

        ui.horizontal(|ui| {
            if message.role == Role::User {
                ui.add_space((available - max_width).max(0.0));
            }
            ui.vertical(|ui| {
                ui.set_max_width(max_width);
                match message.role {
                    Role::User => {
                        theme::card(palette.user_bubble, palette.border, 14, 14).show(ui, |ui| {
                            self.show_attachment_names(ui, message, &palette);
                            if !message.content.is_empty() {
                                ui.label(&message.content);
                            }
                        });
                    }
                    Role::Assistant => {
                        if self.state.show_thinking_trace && !message.thinking.is_empty() {
                            theme::card(palette.thinking, palette.border, 10, 12).show(ui, |ui| {
                                ui.label(
                                    RichText::new(text(language, "思考過程", "Thinking trace"))
                                        .small()
                                        .color(palette.accent),
                                );
                                ui.add_space(5.0);
                                ui.label(RichText::new(&message.thinking).color(palette.muted));
                            });
                            ui.add_space(10.0);
                        }
                        if !message.content.is_empty() {
                            ui.label(&message.content);
                        } else if generating {
                            ui.horizontal(|ui| {
                                ui.add(Spinner::new().size(14.0));
                                ui.label(
                                    RichText::new(text(language, "生成中…", "Generating…")).weak(),
                                );
                            });
                        }
                    }
                    Role::System => {
                        ui.label(RichText::new(&message.content).weak());
                    }
                }
            });
        });
    }

    fn show_attachment_names(&self, ui: &mut Ui, message: &ChatMessage, palette: &TacetaPalette) {
        if message.attachments.is_empty() {
            return;
        }
        ui.horizontal_wrapped(|ui| {
            for attachment in &message.attachments {
                let icon = match attachment.payload {
                    AttachmentPayload::Text(_) => "▤",
                    AttachmentPayload::Image { .. } => "▧",
                };
                ui.label(
                    RichText::new(format!("{icon} {}", attachment.name))
                        .small()
                        .color(palette.muted),
                );
            }
        });
        if !message.content.is_empty() {
            ui.add_space(7.0);
        }
    }

    fn show_composer(&mut self, root_ui: &mut Ui) {
        let language = self.language();
        let panel_height = if self.state.pending_attachments.is_empty() {
            154.0
        } else {
            184.0
        };
        Panel::bottom("taceta-composer-panel")
            .exact_size(panel_height)
            .show_inside(root_ui, |ui| {
                let available = ui.available_width();
                let composer_width = available.min(900.0);
                ui.horizontal(|ui| {
                    ui.add_space(((available - composer_width) / 2.0).max(0.0));
                    ui.vertical(|ui| {
                        ui.set_width(composer_width);
                        let palette = theme::palette(ui);
                        theme::card(palette.composer, palette.border, 18, 12).show(ui, |ui| {
                            let mut remove_attachment = None;
                            if !self.state.pending_attachments.is_empty() {
                                ui.horizontal_wrapped(|ui| {
                                    for (index, attachment) in
                                        self.state.pending_attachments.iter().enumerate()
                                    {
                                        let label = match attachment.payload {
                                            AttachmentPayload::Text(_) => {
                                                format!("▤ {}", attachment.name)
                                            }
                                            AttachmentPayload::Image { .. } => {
                                                format!("▧ {}", attachment.name)
                                            }
                                        };
                                        if ui.small_button(format!("{label}  ×")).clicked() {
                                            remove_attachment = Some(index);
                                        }
                                    }
                                });
                                ui.add_space(4.0);
                            }
                            if let Some(index) = remove_attachment {
                                self.state.pending_attachments.remove(index);
                            }

                            let editor = ui.add_sized(
                                [ui.available_width(), 58.0],
                                TextEdit::multiline(&mut self.state.draft)
                                    .hint_text(text(language, "何でもどうぞ", "Message Taceta"))
                                    .desired_rows(3)
                                    .frame(egui::Frame::NONE),
                            );
                            let keyboard_send = editor.has_focus()
                                && ui.input(|input| {
                                    input.key_pressed(Key::Enter)
                                        && (input.modifiers.command || input.modifiers.ctrl)
                                });

                            ui.horizontal_wrapped(|ui| {
                                let file_clicked = ui
                                    .add_enabled(
                                        self.generation.is_none(),
                                        Button::new("＋").small(),
                                    )
                                    .on_hover_text(text(language, "ファイルを追加", "Add files"))
                                    .clicked();

                                ui.separator();
                                self.show_model_selector(ui);
                                self.show_thinking_mode_selector(ui);

                                let trace_label = if self.state.show_thinking_trace {
                                    text(language, "思考表示: ON", "Trace: Visible")
                                } else {
                                    text(language, "思考表示: OFF", "Trace: Hidden")
                                };
                                if ui
                                    .selectable_label(self.state.show_thinking_trace, trace_label)
                                    .on_hover_text(text(
                                        language,
                                        "推論を止めず、思考過程の表示だけを切り替えます",
                                        "Show or hide the trace without stopping inference",
                                    ))
                                    .clicked()
                                {
                                    self.state.show_thinking_trace =
                                        !self.state.show_thinking_trace;
                                }

                                let mut send_clicked = false;
                                let mut stop_clicked = false;
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if self.generation.is_some() {
                                        stop_clicked = ui
                                            .add(
                                                Button::new(text(language, "■ 停止", "■ Stop"))
                                                    .corner_radius(14),
                                            )
                                            .clicked();
                                    } else {
                                        let ready = self.state.selected_model.is_some()
                                            && (!self.state.draft.trim().is_empty()
                                                || !self.state.pending_attachments.is_empty());
                                        send_clicked = ui
                                            .add_enabled(
                                                ready,
                                                Button::new(text(language, "送信 ↑", "Send ↑"))
                                                    .corner_radius(14),
                                            )
                                            .clicked();
                                    }
                                });

                                if file_clicked {
                                    self.attach_files();
                                }
                                if stop_clicked {
                                    self.stop_generation();
                                } else if send_clicked || keyboard_send {
                                    self.start_generation();
                                }
                            });
                        });
                    });
                });
            });
    }

    fn show_model_selector(&mut self, ui: &mut Ui) {
        let language = self.language();
        let selected = self
            .state
            .selected_model
            .clone()
            .unwrap_or_else(|| text(language, "モデルなし", "No model").to_owned());
        let mut changed = false;
        ComboBox::from_id_salt("taceta-model-selector")
            .selected_text(compact_model_name(&selected))
            .width(150.0)
            .show_ui(ui, |ui| {
                for model in &self.models {
                    if ui
                        .selectable_value(
                            &mut self.state.selected_model,
                            Some(model.name.clone()),
                            &model.name,
                        )
                        .changed()
                    {
                        changed = true;
                    }
                }
                if self.models.is_empty() {
                    ui.label(text(
                        language,
                        "ローカルモデルが見つかりません",
                        "No local models found",
                    ));
                }
            });
        if changed {
            self.ensure_selected_thinking_mode();
        }
    }

    fn show_thinking_mode_selector(&mut self, ui: &mut Ui) {
        let language = self.language();
        let Some(model) = self.selected_model().cloned() else {
            ui.add_enabled(false, Button::new("Thinking: —").small());
            return;
        };
        let current = self
            .state
            .thinking_modes
            .get(&model.name)
            .copied()
            .unwrap_or_else(|| Self::default_thinking_mode(model.thinking));
        let current = Self::normalized_thinking_mode(model.thinking, current);

        match model.thinking {
            ThinkingCapability::None => {
                ui.add_enabled(
                    false,
                    Button::new(text(language, "思考: 非対応", "Thinking: N/A")).small(),
                );
            }
            ThinkingCapability::Unverified => {
                ui.add_enabled(
                    false,
                    Button::new(text(
                        language,
                        "思考: モデル既定",
                        "Thinking: Model default",
                    ))
                    .small(),
                )
                .on_disabled_hover_text(text(
                    language,
                    "このモデルの制御方式は未確認です",
                    "This model's thinking control is unverified",
                ));
            }
            ThinkingCapability::Toggle => {
                let mut selected = current;
                ComboBox::from_id_salt("taceta-thinking-toggle")
                    .selected_text(thinking_mode_label(language, selected))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut selected,
                            ThinkingMode::On,
                            text(language, "思考: ON", "Thinking: On"),
                        );
                        ui.selectable_value(
                            &mut selected,
                            ThinkingMode::Off,
                            text(language, "思考: OFF", "Thinking: Off"),
                        );
                    });
                self.state.thinking_modes.insert(model.name, selected);
            }
            ThinkingCapability::Levels => {
                let mut selected = current;
                ComboBox::from_id_salt("taceta-thinking-level")
                    .selected_text(thinking_mode_label(language, selected))
                    .show_ui(ui, |ui| {
                        for (level, japanese, english) in [
                            (ThinkingLevel::Low, "思考: 低", "Thinking: Low"),
                            (ThinkingLevel::Medium, "思考: 中", "Thinking: Medium"),
                            (ThinkingLevel::High, "思考: 高", "Thinking: High"),
                        ] {
                            ui.selectable_value(
                                &mut selected,
                                ThinkingMode::Level(level),
                                text(language, japanese, english),
                            );
                        }
                    });
                self.state.thinking_modes.insert(model.name, selected);
            }
        }
    }
}

impl eframe::App for TacetaApp {
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.drain_background_work();

        if self.generation.is_some() || self.model_refresh_pending {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.show_sidebar(ui);
        self.show_top_bar(ui);

        match self.screen {
            Screen::Chat => {
                self.show_composer(ui);
                self.show_chat(ui);
            }
            Screen::Settings => self.show_settings(ui),
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        save_app_shell_preferences(storage, APP_SHELL_STORAGE_KEY, &self.shell_preferences);
        save_app_state(storage, &self.state);
    }

    fn auto_save_interval(&self) -> Duration {
        Duration::from_secs(5)
    }
}

impl Drop for TacetaApp {
    fn drop(&mut self) {
        if let Some(generation) = self.generation.take() {
            generation.task.abort();
        }
    }
}

fn image_media_type(extension: &str) -> Option<&'static str> {
    match extension {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

fn compact_model_name(name: &str) -> String {
    let mut compact = name.chars().take(24).collect::<String>();
    if name.chars().count() > 24 {
        compact.push('…');
    }
    compact
}

fn thinking_mode_label(language: AppShellLanguage, mode: ThinkingMode) -> &'static str {
    match mode {
        ThinkingMode::Default => text(language, "思考: モデル既定", "Thinking: Model default"),
        ThinkingMode::Off => text(language, "思考: OFF", "Thinking: Off"),
        ThinkingMode::On => text(language, "思考: ON", "Thinking: On"),
        ThinkingMode::Level(ThinkingLevel::Low) => text(language, "思考: 低", "Thinking: Low"),
        ThinkingMode::Level(ThinkingLevel::Medium) => {
            text(language, "思考: 中", "Thinking: Medium")
        }
        ThinkingMode::Level(ThinkingLevel::High) => text(language, "思考: 高", "Thinking: High"),
    }
}

fn resolve_model_storage_path() -> PathBuf {
    if let Some(path) = std::env::var_os("OLLAMA_MODELS").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }

    if let Ok(output) = Command::new("/bin/launchctl")
        .args(["getenv", "OLLAMA_MODELS"])
        .output()
    {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }

    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/Users/Shared"))
        .join(".ollama")
        .join("models")
}

fn format_context_length(value: u32) -> String {
    format!("{}k", value / 1_024)
}
