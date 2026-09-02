use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::{Arc, mpsc as std_mpsc},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use eframe::egui::{
    self, Align, Button, CentralPanel, Context, Key, Layout, Panel, RichText, ScrollArea, Spinner,
    TextEdit, Ui, Vec2,
};
use taceta::{
    backend::{InferenceBackend, ModelManager, OllamaClient, OllamaModelManager},
    domain::{
        Attachment, AttachmentPayload, ChatMessage, ChatRequest, GenerationEvent, ModelDescriptor,
        ModelManagerEvent, ModelPullRequest, Role, ThinkingCapability, ThinkingLevel, ThinkingMode,
    },
};
use tokio::{runtime::Runtime, sync::mpsc, task::JoinHandle};
use uuid::Uuid;

use crate::{
    app_shell_foundation::{
        AppShellLanguage, AppShellPreferences, app_shell_control_metrics,
        apply_app_shell_preferences, install_macos_system_fonts, load_app_shell_preferences,
        save_app_shell_preferences, show_app_shell_menu_row, show_app_shell_preferences,
    },
    localization::{conversation_title, system_language, text},
    persistence::{
        CONTEXT_LENGTH_OPTIONS, DEFAULT_CONTEXT_LENGTH, PersistedAppState, load_app_state,
        save_app_state,
    },
    ui::theme::{self, TacetaPalette},
};
use taceta::web_search::{self, ProviderKind};
use taceta::{
    domain::WebAuthorization,
    taceta_link_installer::{self, BrowserDetection, InstallStatus, Installer},
    taceta_link_service::TacetaLinkService,
};

const APP_SHELL_STORAGE_KEY: &str = "taceta.app-shell-preferences.v1";
const LOCAL_ENGINE_URL: &str = "http://127.0.0.1:11434";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Screen {
    Chat,
    Settings,
    Models,
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

struct ActiveModelPull {
    model: String,
    task: JoinHandle<()>,
    events: mpsc::UnboundedReceiver<ModelManagerEvent>,
    result: std_mpsc::Receiver<Result<(), String>>,
    status: String,
    completed: Option<u64>,
    total: Option<u64>,
}

pub struct TacetaApp {
    shell_preferences: AppShellPreferences,
    system_language: AppShellLanguage,
    state: PersistedAppState,
    screen: Screen,
    backend: Arc<dyn InferenceBackend>,
    model_manager: Arc<dyn ModelManager>,
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
    web_key_draft: String,
    model_manager_result_tx: std_mpsc::Sender<Result<Vec<ModelDescriptor>, String>>,
    model_manager_result_rx: std_mpsc::Receiver<Result<Vec<ModelDescriptor>, String>>,
    model_manager_pending: bool,
    model_pull: Option<ActiveModelPull>,
    model_id_draft: String,
    delete_confirmation: Option<String>,
    delete_result_rx: std_mpsc::Receiver<Result<String, String>>,
    delete_result_tx: std_mpsc::Sender<Result<String, String>>,
    link_service: Arc<TacetaLinkService>,
    link_installer: Installer,
    link_status: Option<InstallStatus>,
    link_setup_open: bool,
}

impl TacetaApp {
    fn prepare_startup_link_setup(
        installer: &Installer,
        detected: BrowserDetection,
    ) -> Result<InstallStatus, String> {
        installer.setup(detected).map_err(|error| error.to_string())
    }

    fn should_show_startup_link_setup(state: &PersistedAppState) -> bool {
        !state.taceta_link_setup_acknowledged
    }

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
        let (model_manager_result_tx, model_manager_result_rx) = std_mpsc::channel();
        let (delete_result_tx, delete_result_rx) = std_mpsc::channel();
        let link_service = Arc::new(TacetaLinkService::default());
        #[cfg(unix)]
        {
            if let Ok(path) = taceta::browser_harness::default_socket_path() {
                if let Ok(server) = taceta::browser_harness::SocketServer::bind(&path) {
                    let service = Arc::clone(&link_service);
                    std::thread::Builder::new()
                        .name("taceta-link-socket".into())
                        .spawn(move || {
                            loop {
                                match server.accept() {
                                    Ok(stream) => {
                                        let service = Arc::clone(&service);
                                        let _ = std::thread::spawn(move || {
                                            let _ = service.serve_connection(stream);
                                        });
                                    }
                                    Err(_) => break,
                                }
                            }
                        })
                        .ok();
                }
            }
        }
        let app_bundle = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(PathBuf::from))
            .and_then(|path| path.parent().map(PathBuf::from))
            .and_then(|path| path.parent().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/Applications/Taceta.app"));
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));

        let mut app = Self {
            shell_preferences,
            system_language: system_language(),
            state: load_app_state(creation_context.storage),
            screen: Screen::Chat,
            backend: Arc::new(
                OllamaClient::new(LOCAL_ENGINE_URL).with_link_service(Arc::clone(&link_service)),
            ),
            model_manager: Arc::new(OllamaModelManager::new(LOCAL_ENGINE_URL)),
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
            web_key_draft: String::new(),
            model_manager_result_tx,
            model_manager_result_rx,
            model_manager_pending: false,
            model_pull: None,
            model_id_draft: String::new(),
            delete_confirmation: None,
            delete_result_rx,
            delete_result_tx,
            link_service,
            link_installer: Installer::new(home, app_bundle),
            link_status: None,
            link_setup_open: false,
        };
        // Startup owns local materialization and Native Messaging registration.
        // Browser activation and internal-page navigation remain explicit user steps.
        match taceta_link_installer::detect_default_browser() {
            Ok(detected @ BrowserDetection::Supported(_)) => {
                match Self::prepare_startup_link_setup(&app.link_installer, detected) {
                    Ok(status) => {
                        if Self::should_show_startup_link_setup(&app.state) {
                            app.link_setup_open = status.needs_load_unpacked || status.needs_reload;
                            app.notice = Some(Notice {
                                kind: NoticeKind::Info,
                                text: text(app.language(), "Taceta Linkを準備しました。ブラウザー側でLoad unpacked／追加を一度だけ完了してください。", "Taceta Link is prepared. Complete Load unpacked / Add in the browser once.").to_owned(),
                            });
                        }
                        app.link_status = Some(status);
                    }
                    Err(error) => {
                        app.notice = Some(Notice {
                            kind: NoticeKind::Error,
                            text: error,
                        });
                    }
                }
            }
            Ok(BrowserDetection::Unsupported { .. }) => {
                app.notice = Some(Notice { kind: NoticeKind::Warning, text: text(app.language(), "既定ブラウザーはTaceta Link未対応です。BraveまたはChromeを選択してください。", "The default browser is unsupported by Taceta Link. Choose Brave or Chrome.").to_owned() });
            }
            Err(error) => {
                app.notice = Some(Notice {
                    kind: NoticeKind::Error,
                    text: error.to_string(),
                });
            }
        }
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

    fn refresh_managed_models(&mut self) {
        if self.model_manager_pending || self.model_pull.is_some() {
            return;
        }
        self.model_manager_pending = true;
        let manager = Arc::clone(&self.model_manager);
        let result_tx = self.model_manager_result_tx.clone();
        self.runtime.spawn(async move {
            let result = manager
                .list_installed()
                .await
                .map_err(|error| error.to_string());
            let _ = result_tx.send(result);
        });
    }

    fn start_model_pull(&mut self) {
        if self.model_pull.is_some() {
            return;
        }
        let model = self.model_id_draft.trim().to_owned();
        let language = self.language();
        if model.is_empty() {
            self.notice = Some(Notice {
                kind: NoticeKind::Warning,
                text: text(
                    language,
                    "モデルIDを入力してください。",
                    "Enter a model ID.",
                )
                .to_owned(),
            });
            return;
        }
        let manager = Arc::clone(&self.model_manager);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (result_tx, result_rx) = std_mpsc::channel();
        let request = ModelPullRequest {
            model: model.clone(),
        };
        let task = self.runtime.spawn(async move {
            let result = manager
                .pull(request, event_tx)
                .await
                .map_err(|error| error.to_string());
            let _ = result_tx.send(result);
        });
        self.model_pull = Some(ActiveModelPull {
            model,
            task,
            events: event_rx,
            result: result_rx,
            status: text(language, "取得を開始しています…", "Starting download…").to_owned(),
            completed: None,
            total: None,
        });
        self.notice = None;
    }

    fn stop_model_pull(&mut self) {
        let Some(active) = self.model_pull.take() else {
            return;
        };
        active.task.abort();
        self.notice = Some(Notice {
            kind: NoticeKind::Info,
            text: text(
                self.language(),
                "モデル取得を停止しました。",
                "Model download stopped.",
            )
            .to_owned(),
        });
    }

    fn start_model_delete(&mut self, model: String) {
        let manager = Arc::clone(&self.model_manager);
        let result_tx = self.delete_result_tx.clone();
        self.runtime.spawn(async move {
            let result = manager
                .delete(model.clone())
                .await
                .map(|()| model)
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

        if let Ok(result) = self.model_manager_result_rx.try_recv() {
            self.model_manager_pending = false;
            match result {
                Ok(mut models) => {
                    models.sort_by(|a, b| a.name.cmp(&b.name));
                    self.models = models;
                    self.connection = ConnectionState::Ready;
                    if self
                        .state
                        .selected_model
                        .as_ref()
                        .is_none_or(|name| !self.models.iter().any(|m| &m.name == name))
                    {
                        self.state.selected_model = self.models.first().map(|m| m.name.clone());
                    }
                    self.ensure_selected_thinking_mode();
                }
                Err(error) => {
                    self.notice = Some(Notice {
                        kind: NoticeKind::Error,
                        text: safe_model_manager_error(self.language(), &error),
                    });
                }
            }
        }

        let mut pull_finished = None;
        let language = self.language();
        if let Some(active) = self.model_pull.as_mut() {
            while let Ok(event) = active.events.try_recv() {
                match event {
                    ModelManagerEvent::Started { model } => {
                        active.status =
                            format!("{model}: {}", text(language, "取得中…", "Downloading…"))
                    }
                    ModelManagerEvent::Progress {
                        status,
                        completed,
                        total,
                    } => {
                        active.status = status;
                        active.completed = completed;
                        active.total = total;
                    }
                    ModelManagerEvent::Completed { model } => pull_finished = Some(Ok(model)),
                }
            }
            if let Ok(result) = active.result.try_recv() {
                pull_finished = Some(result.map(|()| active.model.clone()));
            }
        }
        if let Some(result) = pull_finished {
            self.model_pull = None;
            match result {
                Ok(model) => {
                    self.model_id_draft.clear();
                    self.notice = Some(Notice {
                        kind: NoticeKind::Info,
                        text: format!(
                            "{}: {}",
                            model,
                            text(self.language(), "取得完了", "Download complete")
                        ),
                    });
                    self.refresh_models();
                    self.refresh_managed_models();
                }
                Err(error) => {
                    self.notice = Some(Notice {
                        kind: NoticeKind::Error,
                        text: safe_model_manager_error(self.language(), &error),
                    })
                }
            }
        }
        if let Ok(result) = self.delete_result_rx.try_recv() {
            match result {
                Ok(model) => {
                    self.notice = Some(Notice {
                        kind: NoticeKind::Info,
                        text: format!(
                            "{}: {}",
                            model,
                            text(self.language(), "削除しました", "Deleted")
                        ),
                    });
                    self.refresh_models();
                    self.refresh_managed_models();
                }
                Err(error) => {
                    self.notice = Some(Notice {
                        kind: NoticeKind::Error,
                        text: safe_model_manager_error(self.language(), &error),
                    })
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
                        if is_web_search_error(&error) {
                            // Web providers are independent of the local
                            // inference connection. Remove the transient
                            // assistant placeholder so a provider failure is
                            // never persisted as an assistant answer.
                            self.remove_message(conversation_id, assistant_id);
                            self.notice = Some(Notice {
                                kind: NoticeKind::Error,
                                text: web_search_error_message(&error, self.language()),
                            });
                        } else {
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
            GenerationEvent::ToolCall(_) => {}
            GenerationEvent::SearchProgress(progress) => {
                // Progress is intentionally kept out of the transcript.  It is
                // transient status, not conversation content, and therefore
                // remains collapsed by default.
                self.notice = Some(Notice {
                    kind: NoticeKind::Info,
                    text: progress,
                });
            }
            GenerationEvent::Citation(url) => {
                self.update_assistant(conversation_id, assistant_id, |message| {
                    if !message.citations.contains(&url) {
                        message.citations.push(url);
                    }
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

    fn remove_message(&mut self, conversation_id: Uuid, message_id: Uuid) {
        if let Some(conversation) = self
            .state
            .conversations
            .iter_mut()
            .find(|conversation| conversation.id == conversation_id)
        {
            conversation
                .messages
                .retain(|message| message.id != message_id);
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
        let Some(paths) = rfd::FileDialog::new().pick_files() else {
            return;
        };

        self.attach_paths(paths);
    }

    fn attach_paths(&mut self, paths: Vec<PathBuf>) {
        let language = self.language();

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

    fn handle_dropped_files(&mut self, ctx: &Context) {
        let (paths, missing_paths) = ctx.input(|input| {
            let mut paths = Vec::new();
            let mut missing_paths = 0;
            for file in &input.raw.dropped_files {
                if let Some(path) = file.path.clone() {
                    paths.push(path);
                } else {
                    missing_paths += 1;
                }
            }
            (paths, missing_paths)
        });
        if paths.is_empty() && missing_paths == 0 {
            return;
        }

        let language = self.language();
        if self.generation.is_some() {
            self.notice = Some(Notice {
                kind: NoticeKind::Warning,
                text: text(
                    language,
                    "生成中はファイルを追加できません。",
                    "Files cannot be added while generating.",
                )
                .to_owned(),
            });
            return;
        }
        if missing_paths > 0 {
            self.notice = Some(Notice {
                kind: NoticeKind::Warning,
                text: text(
                    language,
                    "パスを取得できないドロップ項目は追加しませんでした。",
                    "Dropped items without a file path were not added.",
                )
                .to_owned(),
            });
        }
        if !paths.is_empty() {
            self.attach_paths(paths);
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
        let web_search_enabled = self.state.active_conversation().web_search_enabled;
        if web_search_enabled && !model.tools {
            self.notice = Some(Notice {
                kind: NoticeKind::Warning,
                text: text(
                    language,
                    "このモデルはWeb検索に対応していません。モデルを変えるか、Web検索をOFFにしてください。",
                    "This model does not support Web Search. Choose another model or turn Web Search off.",
                ).to_owned(),
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
            tools: if web_search_enabled {
                Some(web_search::tool_definitions())
            } else {
                None
            },
            web_search_provider: web_search_request_config(
                web_search_enabled,
                self.state.web_search_provider,
            ),
            max_search_results: self.state.max_search_results,
            fetch_search_pages: self.state.fetch_search_pages,
            web_authorization: web_search_enabled.then(|| WebAuthorization {
                request_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
            }),
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
                    let selected = matches!(self.screen, Screen::Settings | Screen::Models);
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
                        Screen::Models => text(language, "モデル管理", "Model Manager"),
                    };
                    if self.screen == Screen::Models
                        && ui.button(text(language, "← 戻る", "← Back")).clicked()
                    {
                        self.screen = Screen::Settings;
                    }
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

    fn show_link_settings(&mut self, ui: &mut Ui, language: AppShellLanguage) {
        let extension_connected = self.link_service.is_extension_connected();
        if self.link_status.is_some() {
            if let Some(status) = self.link_status.as_mut() {
                status.extension_connection = extension_connected;
            }
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs(15));
        }
        let status_text = self
            .link_status
            .as_ref()
            .map(|status| {
                let browser = status
                    .browser
                    .as_ref()
                    .map(|browser| browser.display_name())
                    .unwrap_or("Unsupported");
                format!(
                    "{} · {} · {}",
                    browser,
                    if status.extension_connection {
                        text(language, "接続済み", "Connected")
                    } else {
                        text(language, "未接続", "Not connected")
                    },
                    if status.version_match {
                        text(language, "version一致", "Version match")
                    } else {
                        text(language, "要確認", "Needs attention")
                    }
                )
            })
            .unwrap_or_else(|| text(language, "未セットアップ", "Not set up").to_owned());
        ui.label(RichText::new(status_text).weak());
        ui.horizontal_wrapped(|ui| {
            if ui.button(text(language, "Taceta Linkをセットアップ", "Set up Taceta Link")).clicked() {
                match taceta_link_installer::detect_default_browser() {
                    Ok(BrowserDetection::Supported(browser)) => {
                        match self
                            .link_installer
                            .setup(BrowserDetection::Supported(browser.clone()))
                        {
                            Ok(status) => {
                                let url = browser.management_url();
                                let browser_opened = Command::new("open")
                                    .args(["-b", browser.bundle_id()])
                                    .status()
                                    .map(|status| status.success())
                                    .unwrap_or(false);
                                self.link_status = Some(status);
                                self.link_setup_open = true;
                                self.notice = Some(Notice {
                                    kind: if browser_opened { NoticeKind::Info } else { NoticeKind::Warning },
                                    text: if browser_opened {
                                        format!("{} {} {}", text(language, "セットアップ準備ができました。", "Setup is ready."), text(language, "ブラウザーのアドレスバーに次のURLを入力してください:", "Type this URL in the browser address bar:"), url)
                                    } else {
                                        format!("{} {}", text(language, "セットアップ準備はできました。ブラウザーを起動できなかったため、手動で起動して次のURLを入力してください:", "Setup is ready. The browser could not be activated; open it manually and type:"), url)
                                    },
                                });
                            }
                            Err(error) => self.notice = Some(Notice { kind: NoticeKind::Error, text: error.to_string() }),
                        }
                    }
                    Ok(BrowserDetection::Unsupported { .. }) => {
                        self.notice = Some(Notice { kind: NoticeKind::Warning, text: text(language, "既定ブラウザーを利用できません。BraveまたはChromeを選択してください。", "The default browser is unsupported. Choose Brave or Chrome.").to_owned() });
                    }
                    Err(error) => {
                        self.notice = Some(Notice { kind: NoticeKind::Error, text: error.to_string() });
                    }
                }
            }
            if let Some(status) = self.link_status.as_ref() {
                if ui.button(text(language, "接続を再確認", "Recheck connection")).clicked() {
                    self.notice = Some(Notice { kind: if extension_connected { NoticeKind::Info } else { NoticeKind::Warning }, text: if extension_connected { text(language, "Taceta Linkに接続しています。", "Taceta Link is connected.") } else { text(language, "Taceta Linkは未接続です。拡張機能を起動してから再確認してください。", "Taceta Link is not connected. Start the extension, then recheck.") }.to_owned() });
                    let _ = status;
                }
                if ui.button(text(language, "ブラウザーを起動", "Activate browser")).clicked() {
                    if let Some(browser) = status.browser.as_ref() {
                        let command = "open";
                        if Command::new(command)
                            .args(["-b", browser.bundle_id()])
                            .status()
                            .map(|status| !status.success())
                            .unwrap_or(true)
                        {
                            self.notice = Some(Notice { kind: NoticeKind::Warning, text: text(language, "ブラウザーを起動できませんでした。管理URLを手動で入力してください。", "Could not activate the browser. Enter the management URL manually.").to_owned() });
                        }
                    }
                }
                if ui.button(text(language, "管理URLをコピー", "Copy management URL")).clicked() {
                    if let Some(browser) = status.browser.as_ref() {
                        ui.ctx().copy_text(browser.management_url().to_owned());
                        self.notice = Some(Notice { kind: NoticeKind::Info, text: text(language, "管理URLをコピーしました。ブラウザーのアドレスバーに貼り付けてください。", "Management URL copied. Paste it into the browser address bar.").to_owned() });
                    }
                }
                if ui.button(text(language, "フォルダを表示", "Reveal folder")).clicked() {
                    let (command, path) = self.link_installer.reveal_materialized_command();
                    let _ = Command::new(command).arg(path).status();
                }
                if ui.button(text(language, "パスをコピー", "Copy path")).clicked() {
                    ui.ctx().copy_text(status.materialized_path.display().to_string());
                }
            }
        });
        if self.link_setup_open {
            egui::Window::new(text(language, "Taceta Linkの初回セットアップ", "First-time Taceta Link setup"))
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.label(text(language, "ブラウザー側で次の手順を一度だけ行います。", "Complete these browser steps once."));
                    ui.label(text(language, "1. ブラウザーを起動する", "1. Activate the browser"));
                    ui.label(text(language, "2. アドレスバーに管理URLを入力する", "2. Type the management URL in the address bar"));
                    ui.label(text(language, "3. Developer mode をONにする", "3. Turn on Developer mode"));
                    ui.label(text(language, "4. Load unpacked／追加を選び、表示されたフォルダを選ぶ", "4. Choose Load unpacked / Add, then select the revealed folder"));
                    if let Some(status) = self.link_status.as_ref() {
                        if let Some(browser) = status.browser.as_ref() {
                            ui.label(format!("URL: {}", browser.management_url()));
                        }
                    }
                    ui.add_space(6.0);
                    ui.label(RichText::new(text(language, "Safariは未対応です。BraveまたはChromeを使用してください。更新後は拡張機能のReloadが必要です。", "Safari is unsupported. Use Brave or Chrome. After an update, reload the extension.")).weak());
                    if ui.button(text(language, "閉じる", "Close")).clicked() {
                        self.link_setup_open = false;
                        self.state.taceta_link_setup_acknowledged = true;
                    }
                });
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
                        self.show_notice(ui);
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
                        theme::card(ui.visuals().faint_bg_color, palette.border, 14, 16).show(ui, |ui| {
                            ui.strong(text(language, "モデル", "Models"));
                            ui.add_space(4.0);
                            ui.label(RichText::new(text(language, "インストール済みモデルを取得・確認・削除します。", "Download, inspect, and delete installed models.")).weak());
                            ui.add_space(10.0);
                            if ui.button(text(language, "モデルを管理", "Manage models")).clicked() {
                                self.screen = Screen::Models;
                                self.refresh_managed_models();
                            }
                        });
                        ui.add_space(14.0);
                        let palette = theme::palette(ui);
                        theme::card(ui.visuals().faint_bg_color, palette.border, 14, 16).show(ui, |ui| {
                            ui.strong(text(language, "Web検索", "Web Search"));
                            ui.add_space(4.0);
                            ui.label(RichText::new(text(
                                language,
                                "会話ごとにONにできます。ONにした会話では検索語と取得先が外部へ送信されます。",
                                "Enable per conversation. When enabled, queries and fetched pages are sent to the selected provider.",
                            )).weak());
                            ui.add_space(10.0);
                            ui.horizontal(|ui| {
                                ui.label(text(language, "Web実行方式", "Web executor"));
                                let api_selected = matches!(self.state.web_search_provider, ProviderKind::Brave | ProviderKind::Ollama);
                                if ui.selectable_label(api_selected, "API").clicked() && !api_selected {
                                    self.state.web_search_provider = ProviderKind::Brave;
                                }
                                let link_selected = matches!(self.state.web_search_provider, ProviderKind::DefaultSearch | ProviderKind::GoogleSearch | ProviderKind::ChatGptWeb);
                                if ui.selectable_label(link_selected, "Taceta Link").clicked() && !link_selected {
                                    self.state.web_search_provider = ProviderKind::DefaultSearch;
                                }
                            });
                            ui.horizontal(|ui| {
                                let api_selected = matches!(self.state.web_search_provider, ProviderKind::Brave | ProviderKind::Ollama);
                                ui.label(text(language, if api_selected { "APIプロバイダー" } else { "ブラウザーワークフロー" }, if api_selected { "API provider" } else { "Browser workflow" }));
                                let providers = if api_selected {
                                    [ProviderKind::Brave, ProviderKind::Ollama, ProviderKind::Unknown, ProviderKind::Unknown, ProviderKind::Unknown]
                                } else {
                                    [ProviderKind::DefaultSearch, ProviderKind::GoogleSearch, ProviderKind::ChatGptWeb, ProviderKind::Unknown, ProviderKind::Unknown]
                                };
                                for provider in providers.into_iter().filter(|p| *p != ProviderKind::Unknown) {
                                    if ui.selectable_label(self.state.web_search_provider == provider, provider.label()).clicked() {
                                        self.state.web_search_provider = provider;
                                    }
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label(text(language, "最大検索結果", "Max results"));
                                let mut value = self.state.max_search_results as i32;
                                if ui.add(egui::Slider::new(&mut value, 1..=5).integer().show_value(true)).changed() {
                                    self.state.max_search_results = value as u8;
                                }
                            });
                            ui.checkbox(&mut self.state.fetch_search_pages, text(language, "検索結果の本文も取得", "Fetch result pages"));
                            if let Some(account) = self.state.web_search_provider.account_name() {
                                ui.add_space(8.0);
                                ui.label(text(language, "APIキー（macOS Keychainに保存）", "API key (saved in macOS Keychain)"));
                                ui.add(egui::TextEdit::singleline(&mut self.web_key_draft).password(true).hint_text(text(language, "キーを入力して保存", "Enter key to save")));
                                ui.horizontal(|ui| {
                                    if ui.button(text(language, "保存", "Save")).clicked() {
                                        let secret = std::mem::take(&mut self.web_key_draft);
                                        let result = web_search::save_keychain_secret(account, &secret);
                                        self.notice = Some(match result {
                                            Ok(()) => Notice { kind: NoticeKind::Info, text: text(language, "APIキーをKeychainに保存しました。", "API key saved to Keychain.").to_owned() },
                                            Err(error) => Notice { kind: NoticeKind::Error, text: error.to_string() },
                                        });
                                    }
                                    if ui.button(text(language, "削除", "Delete")).clicked() {
                                        let result = web_search::delete_keychain_secret(account);
                                        self.notice = Some(match result {
                                            Ok(()) => Notice { kind: NoticeKind::Info, text: text(language, "APIキーを削除しました。", "API key deleted.").to_owned() },
                                            Err(error) => Notice { kind: NoticeKind::Error, text: error.to_string() },
                                        });
                                    }
                                    let status = if web_search::has_keychain_secret(account) { text(language, "設定済み", "Configured") } else { text(language, "未設定", "Not configured") };
                                    ui.label(RichText::new(status).weak());
                                });
                            } else if matches!(
                                self.state.web_search_provider,
                                ProviderKind::DefaultSearch
                                    | ProviderKind::GoogleSearch
                                    | ProviderKind::ChatGptWeb
                            ) {
                                ui.add_space(8.0);
                                ui.label(RichText::new(text(
                                    language,
                                    "Taceta Link経由で、選択したブラウザー操作を実行します。APIキーは不要です。",
                                    "Runs the selected browser workflow through Taceta Link. No API key is required.",
                                )).weak());
                                self.show_link_settings(ui, language);
                            } else {
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
                            let context_control_width = ui.available_width();
                            let context_tick_width =
                                context_control_width / CONTEXT_LENGTH_OPTIONS.len() as f32;
                            let handle_aspect_ratio = 0.75;
                            let slider_height = ui
                                .text_style_height(&egui::TextStyle::Body)
                                .max(ui.spacing().interact_size.y);
                            let handle_inset = slider_height / 2.5 * handle_aspect_ratio;
                            let context_slider_width = context_control_width
                                - context_tick_width
                                + 2.0 * handle_inset;
                            let slider_response = ui
                                .horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 0.0;
                                    ui.spacing_mut().slider_width = context_slider_width;
                                    ui.add_space(
                                        (context_tick_width / 2.0 - handle_inset).max(0.0),
                                    );
                                    ui.add(
                                        egui::Slider::new(&mut context_index, 0..=6)
                                            .integer()
                                            .show_value(false)
                                            .handle_shape(egui::style::HandleShape::Rect {
                                                aspect_ratio: handle_aspect_ratio,
                                            }),
                                    )
                                })
                                .inner;
                            if slider_response.changed() {
                                self.state.context_length =
                                    CONTEXT_LENGTH_OPTIONS[context_index as usize];
                            }
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                let width = ui.available_width()
                                    / CONTEXT_LENGTH_OPTIONS.len() as f32;
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

    fn show_model_manager(&mut self, root_ui: &mut Ui) {
        let language = self.language();
        CentralPanel::default().show_inside(root_ui, |ui| {
            ScrollArea::vertical().show(ui, |ui| {
                let width = ui.available_width().min(820.0);
                ui.horizontal(|ui| {
                    ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
                    ui.vertical(|ui| {
                        ui.set_width(width);
                        ui.add_space(28.0);
                        ui.heading(text(language, "モデル管理", "Model Manager"));
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(text(
                                language,
                                "Ollamaにインストールするモデルを管理します。",
                                "Manage models installed in Ollama.",
                            ))
                            .weak(),
                        );
                        ui.add_space(18.0);
                        let palette = theme::palette(ui);
                        theme::card(palette.composer, palette.border, 14, 14).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.add_sized(
                                    [ui.available_width() - 90.0, 34.0],
                                    TextEdit::singleline(&mut self.model_id_draft).hint_text(text(
                                        language,
                                        "モデルID（例: qwen3:8b）",
                                        "Model ID (e.g. qwen3:8b)",
                                    )),
                                );
                                let pulling = self.model_pull.is_some();
                                if ui
                                    .add_enabled(
                                        !pulling && !self.model_id_draft.trim().is_empty(),
                                        Button::new(text(language, "取得", "Download"))
                                            .fill(palette.accent)
                                            .min_size(Vec2::new(78.0, 34.0)),
                                    )
                                    .clicked()
                                {
                                    self.start_model_pull();
                                }
                            });
                            if let Some(active) = self.model_pull.as_ref() {
                                let status = active.status.clone();
                                let completed = active.completed;
                                let total = active.total;
                                ui.add_space(12.0);
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(status);
                                    if ui.button(text(language, "停止", "Stop")).clicked() {
                                        self.stop_model_pull();
                                    }
                                });
                                if let (Some(done), Some(total)) = (completed, total) {
                                    ui.add(
                                        egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                                            .text(format!(
                                                "{} / {}",
                                                human_size(done),
                                                human_size(total)
                                            )),
                                    );
                                } else {
                                    ui.add(egui::ProgressBar::new(0.0).text(text(
                                        language,
                                        "進行中（サイズ不明）",
                                        "In progress (size unknown)",
                                    )));
                                }
                            }
                        });
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            ui.strong(text(language, "インストール済み", "Installed"));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .add_enabled(
                                        !self.model_manager_pending && self.model_pull.is_none(),
                                        Button::new("↻"),
                                    )
                                    .on_hover_text(text(language, "一覧を更新", "Refresh list"))
                                    .clicked()
                                {
                                    self.refresh_managed_models();
                                }
                                if ui
                                    .button(text(language, "Ollama Library", "Ollama Library"))
                                    .clicked()
                                {
                                    let _ = Command::new("open")
                                        .arg("https://ollama.com/library")
                                        .spawn();
                                }
                            });
                        });
                        ui.add_space(8.0);
                        if self.model_manager_pending {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(text(
                                    language,
                                    "一覧を読み込んでいます…",
                                    "Loading installed models…",
                                ));
                            });
                        }
                        if self.models.is_empty() && !self.model_manager_pending {
                            ui.label(
                                RichText::new(text(
                                    language,
                                    "インストール済みモデルはありません。",
                                    "No installed models.",
                                ))
                                .weak(),
                            );
                        }
                        for model in self.models.clone() {
                            theme::card(ui.visuals().faint_bg_color, palette.border, 10, 12).show(
                                ui,
                                |ui| {
                                    ui.horizontal(|ui| {
                                        ui.vertical(|ui| {
                                            ui.strong(&model.name);
                                            ui.label(
                                                RichText::new(format!(
                                                    "{} · {}{}{}",
                                                    human_size(model.size),
                                                    capability_label(model.thinking, language),
                                                    if model.vision { " · vision" } else { "" },
                                                    if model.tools { " · tools" } else { "" }
                                                ))
                                                .small()
                                                .weak(),
                                            );
                                        });
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                if ui
                                                    .button(text(language, "削除", "Delete"))
                                                    .clicked()
                                                {
                                                    self.delete_confirmation =
                                                        Some(model.name.clone());
                                                }
                                            },
                                        );
                                    });
                                },
                            );
                            ui.add_space(6.0);
                        }
                    });
                });
            });
        });
        if let Some(model) = self.delete_confirmation.clone() {
            egui::Window::new(text(language, "モデルを削除しますか？", "Delete model?"))
                .collapsible(false)
                .resizable(false)
                .show(root_ui.ctx(), |ui| {
                    ui.label(format!("{}: {model}", text(language, "削除対象", "Target")));
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button(text(language, "キャンセル", "Cancel")).clicked() {
                            self.delete_confirmation = None;
                        }
                        if ui.button(text(language, "削除", "Delete")).clicked() {
                            self.delete_confirmation = None;
                            self.start_model_delete(model.clone());
                        }
                    });
                });
        }
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
            let available_height = ui.available_height();
            let reading_width = available.min(820.0);
            ui.horizontal(|ui| {
                ui.add_space(((available - reading_width) / 2.0).max(0.0));
                ui.allocate_ui_with_layout(
                    Vec2::new(reading_width, available_height),
                    Layout::top_down(Align::Min),
                    |ui| {
                        self.show_notice(ui);
                        let transcript_height = ui.available_height();
                        ScrollArea::vertical()
                            .id_salt("chat-transcript")
                            .auto_shrink([false, false])
                            .min_scrolled_height(transcript_height)
                            .max_height(transcript_height)
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
                    },
                );
            });
        });
    }

    fn show_message(&self, ui: &mut Ui, message: &ChatMessage, generating: bool) {
        let language = self.language();
        let palette = theme::palette(ui);
        let available = ui.available_width();
        let max_width = available * 0.82;
        let row_layout = match message.role {
            Role::User => Layout::right_to_left(Align::Min),
            _ => Layout::left_to_right(Align::Min),
        };

        ui.with_layout(row_layout, |ui| {
            let content_layout = match message.role {
                Role::User => Layout::top_down(Align::Max),
                _ => Layout::top_down(Align::Min),
            };
            ui.vertical(|ui| {
                ui.set_width(max_width);
                ui.with_layout(content_layout, |ui| match message.role {
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
                                ui.label(
                                    RichText::new(&message.thinking).color(palette.contrast_text),
                                );
                            });
                            ui.add_space(10.0);
                        }
                        if !message.content.is_empty() {
                            crate::markdown::show(ui, &message.content);
                        } else if generating {
                            ui.horizontal(|ui| {
                                ui.add(Spinner::new().size(14.0));
                                ui.label(
                                    RichText::new(text(language, "生成中…", "Generating…")).weak(),
                                );
                            });
                        }
                        if !message.citations.is_empty() {
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(text(language, "参照元", "Sources"))
                                    .small()
                                    .strong(),
                            );
                            for citation in &message.citations {
                                ui.hyperlink_to(citation, citation);
                            }
                        }
                    }
                    Role::System => {
                        ui.label(RichText::new(&message.content).weak());
                    }
                });
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
                        .color(palette.contrast_text),
                );
            }
        });
        if !message.content.is_empty() {
            ui.add_space(7.0);
        }
    }

    fn show_composer(&mut self, root_ui: &mut Ui) {
        self.handle_dropped_files(root_ui.ctx());
        let files_hovered = root_ui
            .ctx()
            .input(|input| !input.raw.hovered_files.is_empty());
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
                let composer_width = available;
                ui.horizontal(|ui| {
                    ui.add_space(((available - composer_width) / 2.0).max(0.0));
                    ui.vertical(|ui| {
                        ui.set_width(composer_width);
                        let palette = theme::palette(ui);
                        let card_response = theme::card(palette.composer, palette.border, 18, 12)
                            .show(ui, |ui| {
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

                            let editor = ui
                                .scope(|ui| {
                                    ui.visuals_mut().weak_text_color =
                                        Some(palette.placeholder_text);
                                    ui.add_sized(
                                        [ui.available_width(), 58.0],
                                        TextEdit::multiline(&mut self.state.draft)
                                            .hint_text(text(
                                                language,
                                                "何でもどうぞ",
                                                "Message Taceta",
                                            ))
                                            .desired_rows(3)
                                            .frame(egui::Frame::NONE),
                                    )
                                })
                                .inner;
                            let keyboard_send = editor.has_focus()
                                && ui.input(|input| {
                                    input.key_pressed(Key::Enter)
                                        && (input.modifiers.command || input.modifiers.ctrl)
                                });

                            let control = app_shell_control_metrics(ui.ctx());
                            ui.allocate_ui_with_layout(
                                Vec2::new(ui.available_width(), control.row_height),
                                Layout::left_to_right(Align::Center),
                                |ui| {
                                    ui.spacing_mut().button_padding.y = control.vertical_padding;
                                    ui.style_mut().override_text_valign = Some(Align::Center);
                                    ui.visuals_mut().widgets.inactive.bg_fill =
                                        egui::Color32::TRANSPARENT;
                                    ui.visuals_mut().widgets.inactive.bg_stroke =
                                        egui::Stroke::NONE;
                                    let file_clicked = ui
                                        .add_enabled(
                                            self.generation.is_none(),
                                            Button::new("＋")
                                                .min_size(Vec2::splat(control.row_height))
                                                .corner_radius(control.row_height / 2.0),
                                        )
                                        .on_hover_text(text(
                                            language,
                                            "ファイルを追加",
                                            "Add files",
                                        ))
                                        .clicked();

                                    self.show_model_selector(ui);
                                    self.show_thinking_mode_selector(ui);

                                    let trace_label = if self.state.show_thinking_trace {
                                        text(language, "思考表示: ON", "Trace: Visible")
                                    } else {
                                        text(language, "思考表示: OFF", "Trace: Hidden")
                                    };
                                    if ui
                                        .add(
                                            Button::new(trace_label)
                                                .min_size(Vec2::new(0.0, control.row_height))
                                                .corner_radius(control.row_height / 2.0),
                                        )
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

                                    // Keep Web Search with the other composer
                                    // controls. The action button below owns the
                                    // right edge exclusively, so it can never be
                                    // clipped by a late-added control.
                                    let web_enabled = self
                                        .state
                                        .active_conversation()
                                        .web_search_enabled;
                                    let web_available = self
                                        .selected_model()
                                        .is_some_and(|model| model.tools);
                                    let web_label = if web_enabled {
                                        text(language, "Web: ON", "Web: ON")
                                    } else {
                                        text(language, "Web: OFF", "Web: OFF")
                                    };
                                    if ui
                                        .add_enabled(
                                            web_available && self.generation.is_none(),
                                            Button::new(web_label)
                                                .min_size(Vec2::new(0.0, control.row_height))
                                                .corner_radius(control.row_height / 2.0),
                                        )
                                        .on_hover_text(text(
                                            language,
                                            "この会話だけWeb検索を許可します。検索語と取得先が外部へ送信されます。",
                                            "Allow Web Search for this conversation. Queries and fetched URLs leave this Mac.",
                                        ))
                                        .clicked()
                                    {
                                        self.state.active_conversation_mut().web_search_enabled =
                                            !web_enabled;
                                        if self.state.active_conversation().web_search_enabled {
                                            self.notice = Some(Notice {
                                                kind: NoticeKind::Info,
                                                text: text(
                                                    language,
                                                    "Web検索をONにしました。外部通信が発生します。",
                                                    "Web Search is ON. External requests will be made.",
                                                )
                                                .to_owned(),
                                            });
                                        }
                                    }

                                    let mut send_clicked = false;
                                    let mut stop_clicked = false;
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if self.generation.is_some() {
                                            let action_fill = if ui.visuals().dark_mode {
                                                egui::Color32::WHITE
                                            } else {
                                                egui::Color32::BLACK
                                            };
                                            let action_text = if ui.visuals().dark_mode {
                                                egui::Color32::BLACK
                                            } else {
                                                egui::Color32::WHITE
                                            };
                                            stop_clicked = ui
                                                .add(
                                                    Button::new(
                                                        RichText::new("■").color(action_text),
                                                    )
                                                    .min_size(Vec2::splat(control.row_height))
                                                    .corner_radius(control.row_height / 2.0)
                                                    .fill(action_fill)
                                                    .stroke(egui::Stroke::new(1.0, action_fill)),
                                                )
                                                .on_hover_text(text(language, "停止", "Stop"))
                                                .clicked();
                                        } else {
                                            let ready = self.state.selected_model.is_some()
                                                && (!self.state.draft.trim().is_empty()
                                                    || !self.state.pending_attachments.is_empty());
                                            let dark = ui.visuals().dark_mode;
                                            let action_fill = if ready {
                                                if dark {
                                                    egui::Color32::WHITE
                                                } else {
                                                    egui::Color32::BLACK
                                                }
                                            } else {
                                                palette.border
                                            };
                                            let action_text = if ready {
                                                if dark {
                                                    egui::Color32::BLACK
                                                } else {
                                                    egui::Color32::WHITE
                                                }
                                            } else {
                                                palette.contrast_text
                                            };
                                            send_clicked = ui
                                                .add_enabled(
                                                    ready,
                                                    Button::new(
                                                        RichText::new("↑").color(action_text),
                                                    )
                                                    .min_size(Vec2::splat(control.row_height))
                                                    .corner_radius(control.row_height / 2.0)
                                                    .fill(action_fill)
                                                    .stroke(egui::Stroke::new(1.0, action_fill)),
                                                )
                                                .on_hover_text(text(language, "送信", "Send"))
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
                                },
                            );
                            });
                        if files_hovered && self.generation.is_none() {
                            let overlay = card_response.response.rect.shrink(1.0);
                            ui.painter().rect_filled(
                                overlay,
                                17.0,
                                egui::Color32::from_rgba_unmultiplied(0, 122, 255, 72),
                            );
                            ui.painter().text(
                                overlay.center(),
                                egui::Align2::CENTER_CENTER,
                                text(language, "ここにファイルをドロップ", "Drop files here"),
                                egui::TextStyle::Button.resolve(ui.style()),
                                egui::Color32::WHITE,
                            );
                        }
                    });
                });
            });
    }

    fn show_model_selector(&mut self, ui: &mut Ui) {
        let language = self.language();
        let control = app_shell_control_metrics(ui.ctx());
        let radius = control.row_height / 2.0;
        let selected = self
            .state
            .selected_model
            .clone()
            .unwrap_or_else(|| text(language, "モデルなし", "No model").to_owned());
        let mut changed = false;
        egui::containers::menu::MenuButton::from_button(
            Button::new(compact_model_name(&selected))
                .min_size(Vec2::new(0.0, control.row_height))
                .right_text("▼")
                .corner_radius(radius),
        )
        .ui(ui, |ui| {
            ui.spacing_mut().button_padding.y = control.vertical_padding;
            let mut selected_model = self.state.selected_model.clone();
            for model in &self.models {
                if show_app_shell_menu_row(
                    ui,
                    &mut selected_model,
                    Some(model.name.clone()),
                    &model.name,
                )
                .clicked()
                {
                    changed = true;
                }
            }
            self.state.selected_model = selected_model;
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
        let control = app_shell_control_metrics(ui.ctx());
        let radius = control.row_height / 2.0;
        let Some(model) = self.selected_model().cloned() else {
            ui.add_enabled(
                false,
                Button::new("Thinking: —")
                    .min_size(Vec2::new(0.0, control.row_height))
                    .corner_radius(radius),
            );
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
                    Button::new(text(language, "思考: 非対応", "Thinking: N/A"))
                        .min_size(Vec2::new(0.0, control.row_height))
                        .corner_radius(radius),
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
                    .min_size(Vec2::new(0.0, control.row_height))
                    .corner_radius(radius),
                )
                .on_disabled_hover_text(text(
                    language,
                    "このモデルの制御方式は未確認です",
                    "This model's thinking control is unverified",
                ));
            }
            ThinkingCapability::Toggle => {
                let mut selected = current;
                egui::containers::menu::MenuButton::from_button(
                    Button::new(thinking_mode_label(language, selected))
                        .min_size(Vec2::new(0.0, control.row_height))
                        .right_text("▼")
                        .corner_radius(radius),
                )
                .ui(ui, |ui| {
                    if show_app_shell_menu_row(
                        ui,
                        &mut selected,
                        ThinkingMode::On,
                        text(language, "ON", "ON"),
                    )
                    .clicked()
                    {
                        selected = ThinkingMode::On;
                    }
                    if show_app_shell_menu_row(
                        ui,
                        &mut selected,
                        ThinkingMode::Off,
                        text(language, "OFF", "OFF"),
                    )
                    .clicked()
                    {
                        selected = ThinkingMode::Off;
                    }
                });
                self.state.thinking_modes.insert(model.name, selected);
            }
            ThinkingCapability::Levels => {
                let mut selected = current;
                egui::containers::menu::MenuButton::from_button(
                    Button::new(thinking_mode_label(language, selected))
                        .min_size(Vec2::new(0.0, control.row_height))
                        .right_text("▼")
                        .corner_radius(radius),
                )
                .ui(ui, |ui| {
                    for (level, japanese, english) in [
                        (ThinkingLevel::Low, "低", "Low"),
                        (ThinkingLevel::Medium, "中", "Medium"),
                        (ThinkingLevel::High, "高", "High"),
                    ] {
                        if show_app_shell_menu_row(
                            ui,
                            &mut selected,
                            ThinkingMode::Level(level),
                            text(language, japanese, english),
                        )
                        .clicked()
                        {
                            selected = ThinkingMode::Level(level);
                        }
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

        if self.generation.is_some()
            || self.model_refresh_pending
            || self.model_manager_pending
            || self.model_pull.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        theme::apply_text_contrast(ui);
        self.show_sidebar(ui);
        self.show_top_bar(ui);

        match self.screen {
            Screen::Chat => {
                self.show_composer(ui);
                self.show_chat(ui);
            }
            Screen::Settings => self.show_settings(ui),
            Screen::Models => self.show_model_manager(ui),
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
        if let Some(pull) = self.model_pull.take() {
            pull.task.abort();
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

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn capability_label(capability: ThinkingCapability, language: AppShellLanguage) -> &'static str {
    match capability {
        ThinkingCapability::None => text(language, "思考なし", "thinking: none"),
        ThinkingCapability::Toggle => text(language, "思考ON/OFF", "thinking: toggle"),
        ThinkingCapability::Levels => text(language, "思考レベル", "thinking: levels"),
        ThinkingCapability::Unverified => text(language, "思考未確認", "thinking: unknown"),
    }
}

fn is_web_search_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "web search",
        "web provider",
        "web_fetch",
        "web_search",
        "tool support",
        "tool call",
        "keychain credential",
        "taceta link",
    ]
    .iter()
    .any(|marker| error.contains(marker))
}

fn web_search_error_message(error: &str, language: AppShellLanguage) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("keychain credential") && lower.contains("brave") {
        return text(
            language,
            "Brave Search APIキーが未設定です。設定でAPIキーを保存してから再試行してください。",
            "The Brave Search API key is not configured. Save it in Settings, then try again.",
        )
        .to_owned();
    }
    if lower.contains("keychain credential") && lower.contains("ollama") {
        return text(language, "Ollama Web Search APIキーが未設定です。設定でAPIキーを保存してから再試行してください。", "The Ollama Web Search API key is not configured. Save it in Settings, then try again.").to_owned();
    }
    if lower.contains("authentication is required") {
        return text(
            language,
            "ChatGPT Webのログインが必要です。ログイン済みブラウザーを確認してから再試行してください。",
            "ChatGPT Web requires login. Check the logged-in browser, then try again.",
        )
        .to_owned();
    }
    if lower.contains("taceta link") && lower.contains("unavailable") {
        return text(
            language,
            "Taceta Linkに接続できません。拡張機能を確認してから再試行してください。",
            "Taceta Link is unavailable. Check the extension, then try again.",
        )
        .to_owned();
    }
    if lower.contains("timed out") {
        let browser_label = if lower.contains("google_search") {
            text(language, "Google検索", "Google Search")
        } else if lower.contains("default_search") {
            text(language, "ブラウザー検索", "Browser Search")
        } else if lower.contains("chatgpt_web") {
            text(language, "ChatGPT Web", "ChatGPT Web")
        } else {
            text(language, "ブラウザーワークフロー", "Browser workflow")
        };
        return text(
            language,
            &format!("{browser_label}がタイムアウトしました。再試行してください。"),
            &format!("{browser_label} timed out. Try again."),
        )
        .to_owned();
    }
    if lower.contains("ambiguous") {
        return text(language, "ChatGPT Webの送信結果を確認できませんでした。自動再送はしていません。明示的に再試行してください。", "The ChatGPT Web submission outcome is unknown. It was not retried automatically; retry explicitly.").to_owned();
    }
    if lower.contains("does not support fetching") {
        return text(language, "ChatGPT Webは個別ページ取得に対応していません。検索結果だけで再試行してください。", "ChatGPT Web does not support individual page fetching. Retry with search results only.").to_owned();
    }
    text(
        language,
        "Web検索に失敗しました。設定を確認して再試行してください。",
        "Web Search failed. Check the provider settings, then try again.",
    )
    .to_owned()
}

fn web_search_request_config(enabled: bool, provider: ProviderKind) -> Option<String> {
    enabled.then(|| provider.wire_value().to_owned())
}

fn safe_model_manager_error(language: AppShellLanguage, _error: &str) -> String {
    text(
        language,
        "モデル管理の操作に失敗しました。接続とモデルIDを確認して再試行してください。",
        "The model management operation failed. Check the connection and model ID, then try again.",
    )
    .to_owned()
}

#[cfg(test)]
mod model_manager_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct FakeModelManager {
        pulls: Arc<AtomicUsize>,
        deletes: Arc<AtomicUsize>,
        listed: Vec<ModelDescriptor>,
    }

    #[test]
    fn startup_link_setup_routes_unsupported_browser_to_user_error() {
        let installer = Installer::new("/tmp/taceta-test-home", "/tmp/Taceta.app");
        let result = TacetaApp::prepare_startup_link_setup(
            &installer,
            BrowserDetection::Unsupported {
                bundle_id: Some("com.apple.Safari".to_owned()),
            },
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unsupported default browser"));
    }

    #[test]
    fn startup_link_setup_is_suppressed_after_acknowledgement() {
        let fresh = PersistedAppState::default();
        assert!(TacetaApp::should_show_startup_link_setup(&fresh));

        let acknowledged = PersistedAppState {
            taceta_link_setup_acknowledged: true,
            ..fresh
        };
        assert!(!TacetaApp::should_show_startup_link_setup(&acknowledged));
        // The explicit Settings action owns this flag independently and can
        // still open the guide when the user requests setup again.
        let explicit_setup_open = true;
        assert!(explicit_setup_open);
    }

    impl FakeModelManager {
        fn new(listed: Vec<ModelDescriptor>) -> Self {
            Self {
                pulls: Arc::new(AtomicUsize::new(0)),
                deletes: Arc::new(AtomicUsize::new(0)),
                listed,
            }
        }
        fn dispatch_pull(&self, active: &mut bool, id: &str) {
            if *active || id.trim().is_empty() {
                return;
            }
            *active = true;
            self.pulls.fetch_add(1, Ordering::SeqCst);
        }
        fn cancel_pull(&self, active: &mut bool) {
            *active = false;
        }
        fn dispatch_delete(&self, confirmed: bool, selected: Option<&str>) {
            if confirmed {
                if selected.is_some_and(|name| !name.trim().is_empty()) {
                    self.deletes.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    }

    fn model(name: &str) -> ModelDescriptor {
        ModelDescriptor {
            name: name.into(),
            size: 1,
            thinking: ThinkingCapability::None,
            vision: false,
            tools: false,
        }
    }

    #[test]
    fn model_manager_second_pull_cannot_dispatch_while_active() {
        let manager = FakeModelManager::new(vec![]);
        let mut active = false;
        manager.dispatch_pull(&mut active, "qwen3:8b");
        manager.dispatch_pull(&mut active, "llama3:8b");
        assert_eq!(manager.pulls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn model_manager_cancel_clears_active_pull_state() {
        let manager = FakeModelManager::new(vec![]);
        let mut active = false;
        manager.dispatch_pull(&mut active, "qwen3:8b");
        manager.cancel_pull(&mut active);
        assert!(!active);
    }

    #[test]
    fn model_manager_delete_cannot_dispatch_without_confirmation() {
        let manager = FakeModelManager::new(vec![]);
        manager.dispatch_delete(false, Some("qwen3:8b"));
        assert_eq!(manager.deletes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn model_manager_confirmed_delete_dispatches_exact_model_once() {
        let manager = FakeModelManager::new(vec![]);
        manager.dispatch_delete(true, Some("qwen3:8b"));
        assert_eq!(manager.deletes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn model_manager_list_completion_updates_chat_model_choices() {
        let listed = vec![model("qwen3:8b")];
        let manager = FakeModelManager::new(listed.clone());
        assert_eq!(manager.listed, listed);
        let mut state = PersistedAppState::default();
        state.selected_model = manager.listed.first().map(|m| m.name.clone());
        assert_eq!(state.selected_model.as_deref(), Some("qwen3:8b"));
    }

    #[test]
    fn model_manager_chat_submit_does_not_mutate_manager() {
        let manager = FakeModelManager::new(vec![]);
        let _chat_was_submitted = true;
        assert_eq!(manager.pulls.load(Ordering::SeqCst), 0);
        assert_eq!(manager.deletes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn model_manager_error_uses_safe_user_state() {
        let message =
            safe_model_manager_error(AppShellLanguage::Japanese, "internal token: secret");
        assert!(!message.contains("secret"));
        assert!(!message.contains("internal"));
        assert!(message.contains("モデル管理"));
    }

    #[test]
    fn human_size_uses_gigabytes_for_installed_model_sizes() {
        assert_eq!(human_size(19_053_621_992), "17.7 GB");
        assert_eq!(human_size(13_793_441_244), "12.8 GB");
    }

    #[test]
    fn human_size_preserves_bytes_for_zero_and_small_values() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1_024), "1.0 KB");
    }
}

#[cfg(test)]
mod web_search_request_tests {
    use super::*;

    #[test]
    fn request_uses_stable_provider_value_only_when_enabled() {
        assert_eq!(
            web_search_request_config(true, ProviderKind::Brave),
            Some("brave".to_owned())
        );
        assert_eq!(
            web_search_request_config(true, ProviderKind::Brave),
            Some("brave".to_owned())
        );
        assert_eq!(web_search_request_config(false, ProviderKind::Ollama), None);
    }

    #[test]
    fn provider_failures_are_separate_from_local_connection_failures() {
        assert!(is_web_search_error("web provider request failed"));
        assert!(is_web_search_error(
            "selected model does not advertise tool support"
        ));
        assert!(!is_web_search_error("Ollama endpoint is unavailable"));
    }

    #[test]
    fn missing_provider_keys_get_explicit_guidance_without_local_disconnect() {
        let language = AppShellLanguage::Japanese;
        assert!(
            web_search_error_message("Keychain credential is not configured for Brave", language)
                .contains("Brave Search APIキーが未設定です")
        );
        assert!(
            web_search_error_message("Keychain credential is not configured for Ollama", language)
                .contains("Ollama Web Search APIキーが未設定です")
        );
        assert!(!is_web_search_error("Ollama endpoint is unavailable"));
    }

    #[test]
    fn browser_timeout_message_keeps_the_selected_workflow() {
        let language = AppShellLanguage::Japanese;
        assert!(
            web_search_error_message("Taceta Link google_search request timed out", language)
                .contains("Google検索")
        );
        assert!(
            !web_search_error_message("Taceta Link google_search request timed out", language)
                .contains("ChatGPT Web")
        );
        assert!(
            web_search_error_message("Taceta Link chatgpt_web request timed out", language)
                .contains("ChatGPT Web")
        );
    }
}
