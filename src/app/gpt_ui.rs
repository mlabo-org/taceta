use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use taceta::agent::{AgentSession, ExternalWorkHandoff, SessionSnapshot};
use taceta::gpt::{
    GptAction, GptApproval, GptClient, GptControl, GptEvent, GptLoginEvent, GptMode,
    GptQuestion, GptRunOutcome, GptRunRequest, GptRunStatus,
};
use tokio::sync::watch;

const TRANSFER_ACTIVE: u8 = 0;
const TRANSFER_CANCELLED: u8 = 1;
const TRANSFER_COMMITTING: u8 = 2;
const TRANSFER_COMMITTED: u8 = 3;

struct ReverseTransferReceipt {
    conversation: crate::persistence::Conversation,
    snapshot: SessionSnapshot,
}

struct GptReverseTransfer {
    task: JoinHandle<()>,
    result: std_mpsc::Receiver<Result<ReverseTransferReceipt, String>>,
    cancel: watch::Sender<bool>,
    phase: Arc<AtomicU8>,
}

pub(super) struct ActiveGpt {
    task: JoinHandle<()>,
    conversation_id: Uuid,
    events: mpsc::UnboundedReceiver<GptEvent>,
    result: std_mpsc::Receiver<Result<GptRunOutcome, String>>,
    cancel: watch::Sender<bool>,
    controls: mpsc::UnboundedSender<GptControl>,
    approvals: VecDeque<GptApproval>,
    questions: VecDeque<PendingQuestions>,
    progress: String,
    pending_thread_save: Option<String>,
    binding_needs_save: bool,
    save_error: Option<String>,
    submitted_prompt: Option<(String, Vec<Attachment>)>,
}

struct PendingQuestions {
    id: Uuid,
    questions: Vec<GptQuestion>,
    answers: Vec<String>,
}

struct GptLoginTask {
    task: JoinHandle<()>,
    events: mpsc::UnboundedReceiver<GptLoginEvent>,
    result: std_mpsc::Receiver<Result<(), String>>,
    browser_opened: bool,
}

pub(super) struct GptUiState {
    pub client: GptClient,
    pub active: Option<ActiveGpt>,
    login: Option<GptLoginTask>,
    logout: Option<std_mpsc::Receiver<Result<(), String>>>,
    bindings_root: Result<PathBuf, String>,
    reverse_transfer: Option<GptReverseTransfer>,
}

impl Default for GptUiState {
    fn default() -> Self {
        #[cfg(not(test))]
        let bindings_root = crate::persistence::gpt_bindings_root();
        #[cfg(test)]
        let bindings_root = Ok(std::env::temp_dir().join(format!("taceta-gpt-ui-{}", Uuid::new_v4())));
        Self {
            client: GptClient::new(),
            active: None,
            login: None,
            logout: None,
            bindings_root,
            reverse_transfer: None,
        }
    }
}

impl GptUiState {
    pub(super) fn auth_busy(&self) -> bool {
        self.login.is_some() || self.logout.is_some()
    }

    pub(super) fn is_transferring(&self) -> bool { self.reverse_transfer.is_some() }
}

impl TacetaApp {
    pub(super) fn start_gpt_reverse_transfer(&mut self) {
        if self.is_generating() || self.gpt_ui.auth_busy() { return; }
        let conversation = self.state.active_conversation().clone();
        if let Err(error) = conversation.returned_to_grok() {
            self.notice = Some(Notice { kind: NoticeKind::Error, text: error });
            return;
        }
        let Some(workspace) = conversation.workspace.clone() else { return; };
        let roots = self.agent_ui.data_root.clone().and_then(|agent_root| {
            self.gpt_ui.bindings_root.clone().map(|binding_root| (agent_root, binding_root))
        });
        let (agent_root, binding_root) = match roots {
            Ok(roots) => roots,
            Err(error) => {
                self.notice = Some(Notice { kind: NoticeKind::Error, text: error });
                return;
            }
        };
        let thread_id = conversation.gpt.as_ref().and_then(|session| session.thread_id.clone());
        let client = self.gpt_ui.client.clone();
        let (cancel, mut cancelled) = watch::channel(false);
        let phase = Arc::new(AtomicU8::new(TRANSFER_ACTIVE));
        let worker_phase = Arc::clone(&phase);
        let (tx, result) = std_mpsc::channel();
        let task = self.runtime.spawn(async move {
            let exported = if let Some(thread_id) = thread_id {
                tokio::select! {
                    result = client.export_handoff(&thread_id, &workspace) => result.map(Some),
                    _ = cancelled.changed() => Err("GPT work transfer cancelled; the current owner is unchanged".into()),
                }
            } else { Ok(None) };
            let handoff = match exported {
                Ok(handoff) => handoff,
                Err(error) => { let _ = tx.send(Err(error)); return; }
            };
            let prepared = tokio::task::spawn_blocking(move || {
                if worker_phase.load(Ordering::Acquire) != TRANSFER_ACTIVE {
                    return Err("GPT work transfer cancelled; the current owner is unchanged".into());
                }
                let snapshot = import_gpt_reverse_work(&agent_root, &conversation, handoff.as_ref())?;
                commit_gpt_reverse_work(&binding_root, &conversation, snapshot, &worker_phase)
            }).await.map_err(|error| format!("Could not transfer the saved GPT work: {error}"))
                .and_then(|result| result);
            let _ = tx.send(prepared);
        });
        self.gpt_ui.reverse_transfer = Some(GptReverseTransfer { task, result, cancel, phase });
        self.notice = Some(Notice { kind: NoticeKind::Info,
            text: text(self.language(), "保存済みのGPT作業をGrokへ引き継いでいます。", "Transferring saved GPT work to Grok.").into() });
    }

    fn drain_gpt_reverse_transfer(&mut self) {
        let result = self.gpt_ui.reverse_transfer.as_ref().and_then(|transfer| match transfer.result.try_recv() {
            Ok(result) => Some(result),
            Err(std_mpsc::TryRecvError::Empty) => None,
            Err(std_mpsc::TryRecvError::Disconnected) => Some(Err("GPT work transfer stopped before its ownership result was returned".into())),
        });
        let Some(result) = result else { return; };
        let transfer = self.gpt_ui.reverse_transfer.take().unwrap();
        let cancelled = transfer.phase.load(Ordering::Acquire) == TRANSFER_CANCELLED;
        match result {
            Ok(receipt) => {
                if let Err(error) = self.accept_gpt_reverse_transfer(receipt) {
                    self.notice = Some(Notice { kind: NoticeKind::Error, text: error });
                    return;
                }
                self.bind_selected_provider();
                self.notice = Some(Notice { kind: NoticeKind::Info,
                    text: text(self.language(),
                        "同じ作業をGrokへ引き継ぎました。「再開」または次の送信で続けられます。GPTの原文履歴は残っています。",
                        "The same task is now available in Grok. Use Resume or send a message to continue. Original GPT history is preserved.").into() });
            }
            Err(error) => self.notice = Some(Notice {
                kind: if cancelled { NoticeKind::Info } else { NoticeKind::Error },
                text: if cancelled {
                    text(self.language(), "引き継ぎを中止しました。GPTの会話と作業フォルダーを維持しています。", "Transfer cancelled. The GPT conversation and workspace are unchanged.").into()
                } else { error },
            }),
        }
    }

    fn accept_gpt_reverse_transfer(&mut self, receipt: ReverseTransferReceipt) -> Result<(), String> {
        self.state.commit_gpt_reverse_transfer(receipt.conversation)?;
        self.remember_agent_snapshot(receipt.snapshot);
        self.scroll_to_bottom = true;
        Ok(())
    }

    pub(super) fn cancel_gpt_reverse_transfer(&mut self) {
        if let Some(transfer) = &self.gpt_ui.reverse_transfer {
            if transfer.phase.compare_exchange(TRANSFER_ACTIVE, TRANSFER_CANCELLED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                let _ = transfer.cancel.send(true);
            }
        }
    }

    pub(super) fn show_gpt_transfer(&mut self, ctx: &Context) {
        let Some(transfer) = &self.gpt_ui.reverse_transfer else { return; };
        let phase = transfer.phase.load(Ordering::Acquire);
        let language = self.language();
        let mut cancel = false;
        egui::Window::new(text(language, "Grokへ作業を引き継ぐ", "Transfer work to Grok"))
            .id(egui::Id::new("gpt-reverse-transfer")).collapsible(false).resizable(false).show(ctx, |ui| {
                ui.spinner();
                ui.label(match phase {
                    TRANSFER_COMMITTING | TRANSFER_COMMITTED => text(language, "会話の切り替えを保存しています…", "Saving conversation ownership…"),
                    TRANSFER_CANCELLED => text(language, "中止を処理しています…", "Cancelling transfer…"),
                    _ => text(language, "保存済みの指示と作業結果を読み込み、同じフォルダーの作業へ引き継いでいます。", "Reading saved instructions and results into the task in the same workspace."),
                });
                ui.label(text(language, "モデルの生成やツール実行は行いません。", "This does not generate a response or run tools."));
                cancel = ui.add_enabled(phase == TRANSFER_ACTIVE, Button::new(text(language, "中止", "Cancel"))).clicked();
            });
        if cancel { self.cancel_gpt_reverse_transfer(); }
    }

    pub(super) fn show_gpt_settings(&mut self, ui: &mut Ui) {
        let language = self.language();
        ui.label(text(language,
            "GPTは公式Codex CLIのOAuth認証と実行機能を使います。APIキーは使いません。認証とCodexの原文履歴はTaceta専用の保存先を使います。",
            "GPT uses the official Codex CLI for OAuth and execution. No API key is used. Authentication and original Codex history use Taceta's own storage."));
        self.show_gpt_login_controls(ui, true);
    }

    fn show_gpt_login_controls(&mut self, ui: &mut Ui, account_settings: bool) {
        let language = self.language();
        ui.horizontal_wrapped(|ui| {
            if let Some(login) = &self.gpt_ui.login {
                ui.spinner();
                ui.label(if login.browser_opened {
                    text(language, "ブラウザーでChatGPTへのログインを完了してください。", "Complete ChatGPT sign-in in your browser.")
                } else {
                    text(language, "ChatGPTのログイン画面を開いています…", "Opening ChatGPT sign-in…")
                });
                if ui.button(text(language, "中止", "Cancel")).clicked() {
                    if let Some(login) = self.gpt_ui.login.take() {
                        login.task.abort();
                    }
                    self.notice = Some(Notice { kind: NoticeKind::Info,
                        text: text(language, "GPT接続を中止しました。", "GPT sign-in cancelled.").into() });
                }
            } else if self.gpt_ui.logout.is_some() {
                ui.spinner();
                ui.label(text(language, "GPT接続を解除しています…", "Disconnecting GPT…"));
            } else if !account_settings && matches!(self.connection, ConnectionState::Ready) {
                ui.label(text(language, "ChatGPT接続済み", "ChatGPT connected"));
            } else {
                if ui.add_enabled(!self.is_generating(), Button::new(text(language,
                    "ChatGPTにログイン", "Sign in with ChatGPT"))).clicked() {
                    self.start_gpt_login();
                }
                if !account_settings {
                    ui.label(text(language,
                        "ChatGPTアカウントで接続すると、利用できるモデルが表示されます。",
                        "Connect your ChatGPT account to see available models."));
                }
                if account_settings && ui.add_enabled(!self.is_generating(), Button::new(text(language,
                    "GPT接続を解除", "Disconnect GPT"))).clicked() {
                    let client = self.gpt_ui.client.clone();
                    let (tx, rx) = std_mpsc::channel();
                    self.runtime.spawn(async move { let _ = tx.send(client.sign_out().await); });
                    self.gpt_ui.logout = Some(rx);
                }
            }
        });
    }

    fn start_gpt_login(&mut self) {
        if self.is_generating() || self.gpt_ui.auth_busy() { return; }
        let client = self.gpt_ui.client.clone();
        let (tx, events) = mpsc::unbounded_channel();
        let (result_tx, result) = std_mpsc::channel();
        let task = self.runtime.spawn(async move {
            let outcome = client.sign_in(tx).await;
            let _ = result_tx.send(outcome);
        });
        self.gpt_ui.login = Some(GptLoginTask { task, events, result, browser_opened: false });
        self.notice = None;
    }

    pub(super) fn show_gpt_run_settings(&mut self, ui: &mut Ui) {
        let language = self.language();
        ui.heading(text(language, "GPTの作業とコンパクション", "GPT work and compaction"));
        ui.label(text(language,
            "GPTの会話はCodexの履歴から再開します。モデルや思考レベルを変えても同じ会話を続けます。コンテキストの管理と自動整理はCodexが行います。",
            "GPT resumes Codex history, including after changing model or reasoning effort. Codex manages context and automatic compaction."));
        ui.add_enabled_ui(!self.is_generating(), |ui| {
            ui.horizontal(|ui| {
                ui.label(text(language, "ツール操作数の停止目安", "Tool-action stop threshold"));
                ui.add(egui::DragValue::new(&mut self.state.gpt_max_tool_actions).range(1..=1_000));
            });
            ui.horizontal(|ui| {
                ui.label(text(language, "1回の実行時間（分）", "Run duration (minutes)"));
                let mut minutes = self.state.agent_max_duration_secs / 60;
                if ui.add(egui::DragValue::new(&mut minutes).range(1..=720)).changed() {
                    self.state.agent_max_duration_secs = minutes * 60;
                }
            });
        });
        ui.label(RichText::new(text(language,
            "ツールの通知数で停止を要求します。並列の操作はすでに開始されている場合があります。確認待ちの時間も実行時間に含みます。中断や上限到達は完了として扱いません。",
            "The threshold requests a stop based on tool notifications; parallel actions may already have started. Approval and question waiting count toward the time limit. Interruptions and limits are not completion.")).small().weak());
        ui.add_space(20.0);
    }

    pub(super) fn show_gpt_controls(&mut self, root_ui: &mut Ui) {
        let language = self.language();
        Panel::top("taceta-agent-controls").show_inside(root_ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                self.show_provider_selector(ui);
                if self.state.inference_provider != InferenceProvider::Gpt { return; }
                let coding = self.state.active_conversation().agent_enabled;
                let mut selected = coding;
                ui.add_enabled_ui(!self.is_generating(), |ui| {
                    ui.selectable_value(&mut selected, false, text(language, "チャット", "Chat"));
                    ui.selectable_value(&mut selected, true, text(language, "作業", "Coding"));
                });
                if selected != coding {
                    let workspace = selected.then(|| self.state.active_conversation().workspace.clone()).flatten();
                    self.state.configure_gpt_environment(selected, workspace);
                }
                if selected {
                    let workspace = self.state.active_conversation().workspace.clone();
                    let label = workspace.as_ref().and_then(|path| path.file_name())
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| text(language, "作業フォルダーを選ぶ", "Choose workspace").into());
                    if ui.add_enabled(!self.is_generating(), Button::new(label))
                        .on_hover_text(workspace.map(|path| path.display().to_string()).unwrap_or_default()).clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.state.configure_gpt_environment(true, Some(path));
                        }
                    }
                }
                let session = self.state.active_conversation().gpt.as_ref()
                    .map(|session| (session.status, session.thread_id.is_some(), session.pending_handoff));
                if let Some((status, has_thread, pending_handoff)) = session {
                    ui.label(gpt_status_label(language, status));
                    if has_thread || pending_handoff {
                        if ui.add_enabled(!self.is_generating(), Button::new(text(language, "続行", "Continue"))).clicked() {
                            self.start_gpt_action(GptAction::Continue);
                        }
                        if ui.add_enabled(!self.is_generating() && !pending_handoff, Button::new(text(language, "コンテキストを整理", "Compact context"))).clicked() {
                            self.start_gpt_action(GptAction::Compact);
                        }
                    }
                }
            });
            if self.state.inference_provider != InferenceProvider::Gpt { return; }
            self.show_gpt_login_controls(ui, false);
            ui.label(RichText::new(text(language,
                "GPTの会話はCodexの履歴を使います。モードや作業フォルダーを変えると新しい会話を作ります。",
                "GPT conversations use Codex history. Changing mode or workspace starts a new conversation.")).small().weak());
            if self.state.active_conversation().gpt.as_ref().is_some_and(|session| session.pending_handoff) {
                ui.label(RichText::new(text(language,
                    "次の送信・続行で、保存した目的・指示・作業状態をGPTへ引き継ぎます。元の作業記録も残します。",
                    "The next Send or Continue hands the saved goal, instructions and work state to GPT. Original task records are preserved.")).small());
            }
            ui.label(RichText::new(if self.state.active_conversation().agent_enabled {
                text(language,
                    "Codexのworkspace-writeで書き込みを制限し、コマンドの通信を無効にします。承認の要否はCodexのポリシーに従います。",
                    "Codex workspace-write limits writes and disables command network access. Codex's policy determines when approval is required.")
            } else {
                text(language,
                    "チャットでは作業環境へのアクセスを無効にします。Taceta Linkは使いません。",
                    "Chat disables access to the working environment. Taceta Link is not used.")
            }).small().weak());
            self.show_gpt_usage(ui);
            if let Some(active) = &self.gpt_ui.active {
                if active.conversation_id == self.state.active_conversation_id {
                    ui.horizontal_wrapped(|ui| { ui.spinner(); ui.label(&active.progress); });
                }
            }
        });
    }

    pub(super) fn show_gpt_usage(&self, ui: &mut Ui) {
        let language = self.language();
        if let Some(usage) = self.state.active_conversation().gpt.as_ref().and_then(|gpt| gpt.usage.as_ref()) {
            ui.label(RichText::new(format!("{}: {} / {} · {}: {}",
                text(language, "入力 / 出力トークン", "Input / output tokens"), usage.input_tokens, usage.output_tokens,
                text(language, "コンテキスト上限", "Context capacity"),
                usage.context_window.map(|value| value.to_string()).unwrap_or_else(|| text(language, "未取得", "Unavailable").into())))
                .small().weak());
        } else {
            ui.label(RichText::new(text(language,
                "コンテキストはCodexが管理します。使用量はまだ取得していません。",
                "Context is managed by Codex. Usage is not available yet.")).small().weak());
        }
    }

    pub(super) fn start_gpt_send(&mut self) {
        if self.state.draft.trim().is_empty() && self.state.pending_attachments.is_empty() { return; }
        let action = GptAction::Send {
            text: self.state.draft.clone(),
            attachments: self.state.pending_attachments.clone(),
        };
        if self.start_gpt_action(action) {
            self.state.draft.clear();
            self.state.pending_attachments.clear();
        }
    }

    fn start_gpt_action(&mut self, action: GptAction) -> bool {
        if self.is_generating() || self.gpt_ui.auth_busy() || self.model_refresh_pending
            || self.state.inference_provider != InferenceProvider::Gpt {
            return false;
        }
        let language = self.language();
        let Some(model) = self.selected_model().cloned() else {
            self.notice = Some(Notice { kind: NoticeKind::Warning,
                text: text(language, "「ChatGPTにログイン」から接続し、モデルを選んでください。", "Use Sign in with ChatGPT, then select a model.").into() });
            return false;
        };
        let conversation = self.state.active_conversation();
        let mode = if conversation.agent_enabled { GptMode::Coding } else { GptMode::Chat };
        if mode == GptMode::Coding && conversation.workspace.is_none() {
            self.notice = Some(Notice { kind: NoticeKind::Warning,
                text: text(language, "作業フォルダーを選んでください。", "Choose a workspace.").into() });
            return false;
        }
        if let GptAction::Send { attachments, .. } = &action {
            if !model.vision && attachments.iter().any(|attachment| matches!(attachment.payload, AttachmentPayload::Image { .. })) {
                self.notice = Some(Notice { kind: NoticeKind::Warning,
                    text: text(language, "このモデルは画像入力に対応していません。画像は送信されませんでした。", "This model does not support image input. Nothing was sent.").into() });
                return false;
            }
        }
        let pending_handoff = conversation.gpt.as_ref().is_some_and(|session| session.pending_handoff);
        let mut request = GptRunRequest {
            thread_id: conversation.gpt.as_ref().and_then(|gpt| gpt.thread_id.clone()),
            workspace: if mode == GptMode::Coding { conversation.workspace.clone() } else { None },
            mode,
            model: model.name,
            thinking: self.selected_thinking_mode(),
            action: action.clone(),
            max_duration_secs: self.state.agent_max_duration_secs,
            max_tool_actions: self.state.gpt_max_tool_actions,
            handoff: None,
        };
        if !matches!(action, GptAction::Send { .. }) && request.thread_id.is_none() && !pending_handoff { return false; }
        if matches!(action, GptAction::Compact) && request.thread_id.is_none() { return false; }
        if matches!(action, GptAction::Compact) && pending_handoff { return false; }
        let submitted_prompt = match &action {
            GptAction::Send { text, attachments } => Some((text.clone(), attachments.clone())),
            _ => None,
        };
        let handoff_root = if pending_handoff {
            match self.agent_ui.data_root.clone() {
                Ok(root) => Some(root),
                Err(error) => {
                    self.notice = Some(Notice { kind: NoticeKind::Error, text: error });
                    return false;
                }
            }
        } else { None };
        let conversation = self.state.active_conversation_mut();
        conversation.provider = Some(InferenceProvider::Gpt);
        if let GptAction::Send { text: prompt, attachments } = action {
            if conversation.should_generate_title() {
                conversation.title = conversation_title(&prompt, "Taceta");
            }
            let mut message = ChatMessage::new_user(prompt);
            message.attachments = attachments;
            conversation.messages.push(message);
        }
        let session = conversation.gpt.get_or_insert_with(Default::default);
        session.status = GptRunStatus::Running;
        session.error = None;
        session.thinking.clear();
        let conversation_id = conversation.id;
        let (event_tx, events) = mpsc::unbounded_channel();
        let (controls, control_rx) = mpsc::unbounded_channel();
        let (cancel, cancel_rx) = watch::channel(false);
        let (result_tx, result) = std_mpsc::channel();
        let client = self.gpt_ui.client.clone();
        let task = self.runtime.spawn(async move {
            if let Some(root) = handoff_root {
                let workspace = request.workspace.clone();
                let prepared = tokio::task::spawn_blocking(move || {
                    let workspace = workspace.ok_or("The source task workspace is missing")?;
                    let mut session = taceta::agent::AgentSession::open(&root, conversation_id)?;
                    session.export_handoff(&workspace)
                }).await;
                match prepared {
                    Ok(Ok(handoff)) => request.handoff = Some(handoff),
                    Ok(Err(error)) => { let _ = result_tx.send(Err(error)); return; }
                    Err(error) => { let _ = result_tx.send(Err(format!("Could not prepare the saved work for GPT: {error}"))); return; }
                }
            }
            let outcome = client.run(request, event_tx, control_rx, cancel_rx).await;
            let _ = result_tx.send(outcome);
        });
        self.gpt_ui.active = Some(ActiveGpt {
            task, conversation_id, events, result, cancel, controls,
            approvals: VecDeque::new(), questions: VecDeque::new(),
            progress: text(language, "GPTを開始しています…", "Starting GPT…").into(),
            pending_thread_save: None,
            binding_needs_save: false, save_error: None, submitted_prompt,
        });
        self.notice = None;
        self.scroll_to_bottom = true;
        true
    }

    pub(super) fn drain_gpt_work(&mut self) {
        self.drain_gpt_auth();
        self.drain_gpt_reverse_transfer();
        let mut events = Vec::new();
        let mut outcome = None;
        if let Some(active) = &mut self.gpt_ui.active {
            while let Ok(event) = active.events.try_recv() { events.push(event); }
            outcome = match active.result.try_recv() {
                Ok(result) => Some(result),
                Err(std_mpsc::TryRecvError::Empty) => None,
                Err(std_mpsc::TryRecvError::Disconnected) => Some(Err("GPT stopped before a final outcome was recorded. The Codex conversation can be resumed.".into())),
            };
        }
        for event in events { self.apply_gpt_event(event); }
        if let Some(outcome) = outcome { self.finish_gpt_run(outcome); }
    }

    fn drain_gpt_auth(&mut self) {
        let mut events = Vec::new();
        let mut outcome = None;
        if let Some(login) = &mut self.gpt_ui.login {
            while let Ok(event) = login.events.try_recv() { events.push(event); }
            outcome = match login.result.try_recv() {
                Ok(result) => Some(result),
                Err(std_mpsc::TryRecvError::Empty) => None,
                Err(std_mpsc::TryRecvError::Disconnected) => Some(Err("GPT sign-in stopped before completion".into())),
            };
        }
        for event in events {
            match event {
                GptLoginEvent::OpenBrowser(url) => {
                    if let Err(error) = Command::new("/usr/bin/open").arg(url).spawn() {
                        if let Some(login) = self.gpt_ui.login.take() { login.task.abort(); }
                        outcome = Some(Err(format!("Could not open the GPT sign-in page: {error}")));
                    } else if let Some(login) = &mut self.gpt_ui.login {
                        login.browser_opened = true;
                    }
                }
                GptLoginEvent::Progress(progress) => self.notice = Some(Notice { kind: NoticeKind::Info, text: progress }),
            }
        }
        if let Some(outcome) = outcome {
            self.gpt_ui.login = None;
            match outcome {
                Ok(()) => {
                    if self.state.inference_provider == InferenceProvider::Gpt { self.refresh_models(); }
                    else { self.select_provider(InferenceProvider::Gpt); }
                    self.notice = Some(Notice { kind: NoticeKind::Info,
                        text: text(self.language(), "GPTに接続しました。利用可能なモデルを取得しています。", "GPT connected. Loading available models.").into() });
                }
                Err(error) => self.notice = Some(Notice { kind: NoticeKind::Error, text: error }),
            }
        }
        let logout = self.gpt_ui.logout.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(result) => Some(result),
            Err(std_mpsc::TryRecvError::Empty) => None,
            Err(std_mpsc::TryRecvError::Disconnected) => Some(Err("GPT sign-out stopped before completion".into())),
        });
        if let Some(result) = logout {
            self.gpt_ui.logout = None;
            self.notice = Some(match result {
                Ok(()) => {
                    if self.state.inference_provider == InferenceProvider::Gpt {
                        self.model_list_epoch = self.model_list_epoch.wrapping_add(1);
                        self.model_refresh_pending = false;
                        self.models.clear();
                        self.connection = ConnectionState::Unavailable(text(self.language(), "GPTへの再接続が必要です。", "Reconnect GPT to continue.").into());
                    }
                    Notice { kind: NoticeKind::Info,
                        text: text(self.language(), "このTacetaのGPT認証を削除しました。", "Removed this Taceta's GPT credentials.").into() }
                }
                Err(error) => Notice { kind: NoticeKind::Error, text: error },
            });
        }
    }

    fn apply_gpt_event(&mut self, event: GptEvent) {
        let Some(active) = self.gpt_ui.active.as_mut() else { return; };
        let Some(conversation) = self.state.conversations.iter_mut()
            .find(|conversation| conversation.id == active.conversation_id && conversation.is_gpt()) else { return; };
        let session = conversation.gpt.get_or_insert_with(Default::default);
        match event {
            GptEvent::ThreadReady { thread_id } => {
                session.thread_id = Some(thread_id.clone());
                active.pending_thread_save = Some(thread_id);
            }
            GptEvent::HandoffAccepted => {
                session.pending_handoff = false;
                active.binding_needs_save = true;
            }
            GptEvent::MessageStarted { id } => { gpt_message(conversation, id); }
            GptEvent::ContentDelta { id, text } => { gpt_message(conversation, id).content.push_str(&text); }
            GptEvent::MessageCompleted { id, text } => { gpt_message(conversation, id).content = text; }
            GptEvent::ThinkingDelta(text) => session.thinking.push_str(&text),
            GptEvent::Tool(item) => {
                if let Some(existing) = session.tools.iter_mut().find(|tool| tool.id == item.id) { *existing = item; }
                else { session.tools.push(item); }
            }
            GptEvent::Approval(request) => {
                if !*active.cancel.borrow() {
                    session.status = GptRunStatus::AwaitingApproval;
                    active.approvals.push_back(request);
                }
            }
            GptEvent::Questions { id, questions } => {
                if !*active.cancel.borrow() {
                    session.status = GptRunStatus::AwaitingApproval;
                    let answers = vec![String::new(); questions.len()];
                    active.questions.push_back(PendingQuestions { id, questions, answers });
                }
            }
            GptEvent::RequestResolved { id } => {
                active.approvals.retain(|request| request.id != id);
                active.questions.retain(|request| request.id != id);
                if active.approvals.is_empty() && active.questions.is_empty() { session.status = GptRunStatus::Running; }
            }
            GptEvent::Progress(progress) => active.progress = progress,
            GptEvent::Diff(diff) => session.diff = diff,
            GptEvent::Usage { input_tokens, output_tokens, context_window } => {
                session.usage = Some(crate::persistence::GptUsage { input_tokens, output_tokens, context_window });
            }
        }
        self.scroll_to_bottom = true;
    }

    pub(super) fn persist_gpt_thread(&mut self, storage: Option<&mut (dyn eframe::Storage + 'static)>) {
        let Some(active) = self.gpt_ui.active.as_mut() else { return; };
        if active.pending_thread_save.is_none() && !active.binding_needs_save { return; }
        let thread_id = active.pending_thread_save.take();
        active.binding_needs_save = false;
        let saved = self.gpt_ui.bindings_root.as_ref().map_err(Clone::clone).and_then(|root| {
            let conversation = self.state.conversations.iter().find(|conversation| conversation.id == active.conversation_id)
                .ok_or_else(|| "The GPT conversation to save is missing".to_owned())?;
            crate::persistence::save_gpt_binding(root, conversation)
        });
        if let Err(error) = saved {
            let _ = active.cancel.send(true);
            active.approvals.clear();
            active.questions.clear();
            active.save_error = Some(error.clone());
            self.notice = Some(Notice { kind: NoticeKind::Error,
                text: error });
            return;
        }
        if let Some(storage) = storage {
            save_app_state(storage, &self.state);
            storage.flush();
        }
        if let Some(thread_id) = thread_id.filter(|_| !*active.cancel.borrow()) {
            let _ = active.controls.send(GptControl::ThreadSaved { thread_id });
        }
    }

    fn finish_gpt_run(&mut self, outcome: Result<GptRunOutcome, String>) {
        let Some(mut active) = self.gpt_ui.active.take() else { return; };
        active.approvals.clear();
        active.questions.clear();
        active.pending_thread_save = None;
        let (thread_id, mut status, mut error) = match outcome {
            Ok(outcome) => (Some(outcome.thread_id), outcome.status, outcome.error),
            Err(error) => (None, if *active.cancel.borrow() { GptRunStatus::Interrupted } else { GptRunStatus::Failed }, Some(error)),
        };
        if let Some(save_error) = active.save_error {
            status = GptRunStatus::Failed;
            error = Some(save_error);
        }
        if let Some(conversation) = self.state.conversations.iter_mut().find(|conversation| conversation.id == active.conversation_id) {
            let session = conversation.gpt.get_or_insert_with(Default::default);
            if let Some(thread_id) = thread_id.filter(|id| !id.is_empty()) { session.thread_id = Some(thread_id); }
            session.status = status;
            session.error = error.clone();
            if session.pending_handoff && matches!(status, GptRunStatus::Failed | GptRunStatus::Interrupted)
                && active.conversation_id == self.state.active_conversation_id && self.state.draft.is_empty() {
                if let Some((prompt, attachments)) = active.submitted_prompt {
                    self.state.draft = prompt;
                    self.state.pending_attachments = attachments;
                }
            }
            if session.thread_id.is_some() {
                let saved = self.gpt_ui.bindings_root.as_ref().map_err(Clone::clone)
                    .and_then(|root| crate::persistence::save_gpt_binding(root, conversation));
                if let Err(save_error) = saved { error = Some(save_error); }
            }
        }
        self.notice = Some(Notice {
            kind: if status == GptRunStatus::Failed { NoticeKind::Error } else { NoticeKind::Info },
            text: error.unwrap_or_else(|| gpt_status_label(self.language(), status).into()),
        });
    }

    pub(super) fn stop_gpt_run(&mut self) {
        let language = self.language();
        if let Some(active) = &mut self.gpt_ui.active {
            let _ = active.cancel.send(true);
            active.approvals.clear();
            active.questions.clear();
            active.pending_thread_save = None;
            active.progress = text(language, "GPTを停止しています…", "Stopping GPT…").into();
        }
    }

    pub(super) fn abort_gpt_on_exit(&mut self) {
        self.cancel_gpt_reverse_transfer();
        if let Some(transfer) = self.gpt_ui.reverse_transfer.take() { transfer.task.abort(); }
        if let Some(active) = self.gpt_ui.active.take() {
            let _ = active.cancel.send(true);
            active.task.abort();
        }
        if let Some(login) = self.gpt_ui.login.take() { login.task.abort(); }
    }

    pub(super) fn show_gpt_chat(&mut self, root_ui: &mut Ui) {
        let language = self.language();
        CentralPanel::default().show_inside(root_ui, |ui| {
            self.show_notice(ui);
            ScrollArea::vertical().id_salt("gpt-transcript")
                .stick_to_bottom(self.scroll_to_bottom || self.is_generating()).show(ui, |ui| {
                let conversation = self.state.active_conversation();
                if conversation.messages.is_empty() {
                    ui.label(text(language,
                        "作業フォルダーとモデルを選び、実行したいことを入力してください。普通の会話には「チャット」を選べます。",
                        "Choose a workspace and model, then describe the work. Select Chat for an ordinary conversation."));
                }
                for message in &conversation.messages { self.show_message(ui, message, self.is_generating()); }
                if let Some(session) = &conversation.gpt {
                    if self.state.show_thinking_trace && !session.thinking.is_empty() {
                        egui::CollapsingHeader::new(text(language, "思考過程", "Thinking trace"))
                            .id_salt(("gpt-thinking", conversation.id)).default_open(true).show(ui, |ui| { ui.label(&session.thinking); });
                    }
                    for item in &session.tools {
                        egui::CollapsingHeader::new(format!("{} {}", if item.completed { "✓" } else { "…" }, item.title))
                            .id_salt(("gpt-tool", conversation.id, &item.id)).show(ui, |ui| {
                                ScrollArea::both().id_salt(("gpt-output", &item.id)).max_height(480.0)
                                    .show(ui, |ui| { ui.monospace(&item.detail); });
                            });
                    }
                    if !session.diff.is_empty() {
                        egui::CollapsingHeader::new(text(language, "ファイルの変更差分", "File changes"))
                            .id_salt(("gpt-diff", conversation.id)).show(ui, |ui| {
                                ScrollArea::both().max_height(480.0).show(ui, |ui| { ui.monospace(&session.diff); });
                            });
                    }
                    if let Some(error) = &session.error { ui.colored_label(theme::palette(ui).error, error); }
                }
            });
        });
        self.scroll_to_bottom = false;
    }

    pub(super) fn show_gpt_requests(&mut self, ctx: &Context) {
        let language = self.language();
        let mut approval_decision = None;
        let mut send_answers = false;
        let mut stop = false;
        let Some(active) = self.gpt_ui.active.as_mut() else { return; };
        if let Some(request) = active.approvals.front() {
            egui::Window::new(text(language, "GPTの実行内容を確認", "Approve GPT action"))
                .id(egui::Id::new("gpt-approval")).collapsible(false).default_width(680.0).show(ctx, |ui| {
                ui.label(&request.description);
                ScrollArea::both().max_height(440.0).show(ui, |ui| { ui.monospace(&request.details); });
                ui.horizontal(|ui| {
                    if ui.button(text(language, "この1回を許可", "Approve once")).clicked() { approval_decision = Some(true); }
                    if ui.button(text(language, "拒否", "Deny")).clicked() { approval_decision = Some(false); }
                    if ui.button(text(language, "実行を停止", "Stop run")).clicked() { stop = true; }
                });
            });
        } else if let Some(request) = active.questions.front_mut() {
            egui::Window::new(text(language, "GPTからの質問", "Question from GPT"))
                .id(egui::Id::new("gpt-questions")).collapsible(false).default_width(680.0).show(ctx, |ui| {
                ScrollArea::vertical().max_height(500.0).show(ui, |ui| {
                    for (index, question) in request.questions.iter().enumerate() {
                        ui.push_id(&question.id, |ui| {
                            ui.strong(&question.header);
                            ui.label(&question.question);
                            for option in &question.options {
                                if ui.selectable_label(request.answers[index] == option.label, &option.label)
                                    .on_hover_text(&option.description).clicked() {
                                    request.answers[index] = option.label.clone();
                                }
                            }
                            ui.add(TextEdit::singleline(&mut request.answers[index]).password(question.is_secret)
                                .hint_text(text(language, "回答を入力", "Type an answer")));
                            ui.add_space(12.0);
                        });
                    }
                });
                ui.horizontal(|ui| {
                    if ui.add_enabled(request.answers.iter().all(|answer| !answer.trim().is_empty()),
                        Button::new(text(language, "回答を送る", "Send answers"))).clicked() { send_answers = true; }
                    if ui.button(text(language, "実行を停止", "Stop run")).clicked() { stop = true; }
                });
            });
        }
        if let Some(approved) = approval_decision { self.decide_gpt_approval(approved); }
        if send_answers { self.send_gpt_answers(); }
        if stop { self.stop_gpt_run(); }
    }

    fn decide_gpt_approval(&mut self, approved: bool) {
        if let Some(active) = &mut self.gpt_ui.active {
            if let Some(request) = active.approvals.pop_front() {
                let _ = active.controls.send(GptControl::Approval { id: request.id, approved });
            }
        }
    }

    fn send_gpt_answers(&mut self) {
        if let Some(active) = &mut self.gpt_ui.active {
            if let Some(request) = active.questions.pop_front() {
                let answers = request.questions.into_iter().zip(request.answers)
                    .map(|(question, answer)| (question.id, answer)).collect();
                let _ = active.controls.send(GptControl::Answers { id: request.id, answers });
            }
        }
    }

    pub(super) fn archive_gpt_chats(&mut self, ids: &[Uuid]) -> bool {
        for id in ids {
            if !self.state.conversations.iter().any(|conversation| conversation.id == *id && conversation.has_gpt_history()) { continue; }
            let result = self.gpt_ui.bindings_root.as_ref().map_err(Clone::clone)
                .and_then(|root| crate::persistence::archive_gpt_binding(root, *id));
            if let Err(error) = result {
                self.notice = Some(Notice { kind: NoticeKind::Error, text: error });
                return false;
            }
        }
        true
    }
}

fn import_gpt_reverse_work(
    agent_root: &std::path::Path,
    conversation: &crate::persistence::Conversation,
    handoff: Option<&ExternalWorkHandoff>,
) -> Result<SessionSnapshot, String> {
    let gpt = conversation.gpt.as_ref().ok_or("The GPT task metadata is missing")?;
    let workspace = conversation.workspace.as_deref().ok_or("The GPT task workspace is missing")?;
    let mut session = if gpt.handoff_from_taceta {
        // Returning to an original task must never silently replace missing
        // original instructions with a newly created, empty journal.
        AgentSession::open(agent_root, conversation.id)?
    } else {
        AgentSession::open_or_create(agent_root, conversation.id, workspace)?.0
    };
    let snapshot = session.snapshot();
    let selected_workspace = fs::canonicalize(workspace).map_err(|error| format!("Could not resolve the task workspace: {error}"))?;
    if snapshot.workspace != selected_workspace {
        return Err("The original task belongs to another workspace; GPT ownership is unchanged".into());
    }
    match handoff {
        None if gpt.handoff_from_taceta && gpt.pending_handoff && gpt.thread_id.is_none() => Ok(snapshot),
        Some(handoff) if gpt.thread_id.as_deref() == Some(handoff.source_session_id.as_str()) => {
            if handoff.records.is_empty() && gpt.handoff_from_taceta && gpt.pending_handoff {
                if fs::canonicalize(&handoff.workspace).map_err(|error| error.to_string())? != selected_workspace {
                    return Err("The empty GPT history belongs to another workspace".into());
                }
                Ok(snapshot)
            } else {
                session.import_external_handoff(handoff)
            }
        }
        _ => Err("The saved GPT records do not match this task; ownership is unchanged".into()),
    }
}

fn commit_gpt_reverse_work(
    binding_root: &std::path::Path,
    previous: &crate::persistence::Conversation,
    snapshot: SessionSnapshot,
    phase: &AtomicU8,
) -> Result<ReverseTransferReceipt, String> {
    if phase.compare_exchange(TRANSFER_ACTIVE, TRANSFER_COMMITTING, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return Err("GPT work transfer cancelled; imported records remain saved without changing the owner".into());
    }
    let conversation = previous.returned_to_grok()?;
    crate::persistence::save_gpt_reverse_ownership(binding_root, previous, &conversation)?;
    phase.store(TRANSFER_COMMITTED, Ordering::Release);
    Ok(ReverseTransferReceipt { conversation, snapshot })
}

fn gpt_message(conversation: &mut crate::persistence::Conversation, id: String) -> &mut ChatMessage {
    let session = conversation.gpt.get_or_insert_with(Default::default);
    let message_id = *session.item_messages.entry(id).or_insert_with(Uuid::new_v4);
    let index = conversation.messages.iter().position(|message| message.id == message_id)
        .unwrap_or_else(|| {
            let mut message = ChatMessage::new_assistant("");
            message.id = message_id;
            conversation.messages.push(message);
            conversation.messages.len() - 1
        });
    &mut conversation.messages[index]
}

fn gpt_status_label(language: AppShellLanguage, status: GptRunStatus) -> &'static str {
    match status {
        GptRunStatus::Idle => text(language, "準備完了", "Ready"),
        GptRunStatus::Running => text(language, "実行中", "Running"),
        GptRunStatus::AwaitingApproval => text(language, "確認・回答待ち", "Awaiting approval or answer"),
        GptRunStatus::Completed => text(language, "完了", "Completed"),
        GptRunStatus::Interrupted => text(language, "中断・続行できます", "Interrupted · resumable"),
        GptRunStatus::LimitReached => text(language, "上限到達・続行できます", "Limit reached · resumable"),
        GptRunStatus::Failed => text(language, "失敗・内容を確認してください", "Failed · review the error"),
        GptRunStatus::Compacted => text(language, "コンテキストを整理しました", "Context compacted"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::model_manager_tests::{RecordingServices, test_app};
    use taceta::gpt::GptQuestionOption;
    use taceta::agent::{AgentMessage, AgentRole, ExternalWorkRecord, ExternalWorkRole, SessionStatus};

    fn reverse_conversation(workspace: &std::path::Path, from_taceta: bool) -> crate::persistence::Conversation {
        let mut conversation = crate::persistence::Conversation::for_provider(InferenceProvider::Gpt);
        conversation.workspace = Some(workspace.to_owned());
        conversation.messages = vec![ChatMessage::new_user("Preserve the original goal"), ChatMessage::new_assistant("GPT progress")];
        let session = conversation.gpt.as_mut().unwrap();
        session.thread_id = Some("saved-gpt-work".into());
        session.handoff_from_taceta = from_taceta;
        conversation
    }

    fn reverse_records(workspace: &std::path::Path) -> ExternalWorkHandoff {
        ExternalWorkHandoff {
            source: "codex".into(), source_session_id: "saved-gpt-work".into(), workspace: workspace.to_owned(),
            records: vec![
                ExternalWorkRecord { id: "user-1".into(), role: ExternalWorkRole::User, content: "Preserve the original goal".into() },
                ExternalWorkRecord { id: "answer-1".into(), role: ExternalWorkRole::Assistant, content: "GPT progress".into() },
                ExternalWorkRecord { id: "tool-1".into(), role: ExternalWorkRole::ToolResult, content: "Saved test output".into() },
            ],
        }
    }

    fn attach_reverse_conversation(app: &mut TacetaApp, conversation: crate::persistence::Conversation) {
        app.state.inference_provider = InferenceProvider::Gpt;
        app.state.active_conversation_id = conversation.id;
        app.state.conversations = vec![conversation];
        app.backend = None;
    }

    #[test]
    fn gpt_reverse_fresh_task_keeps_identity_visible_history_and_durable_local_owner() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("workspace")).unwrap();
        let workspace = fs::canonicalize(temp.path().join("workspace")).unwrap();
        fs::write(workspace.join("current.rs"), "current workspace contents").unwrap();
        let agent_root = temp.path().join("AgentSessions");
        let binding_root = temp.path().join("GptBindings");
        let conversation = reverse_conversation(&workspace, false);
        let id = conversation.id;
        crate::persistence::save_gpt_binding(&binding_root, &conversation).unwrap();
        let snapshot = import_gpt_reverse_work(&agent_root, &conversation, Some(&reverse_records(&workspace))).unwrap();
        assert_eq!(snapshot.status, SessionStatus::Interrupted);
        let receipt = commit_gpt_reverse_work(&binding_root, &conversation, snapshot, &AtomicU8::new(TRANSFER_ACTIVE)).unwrap();
        let mut app = test_app(Arc::new(RecordingServices::default()));
        attach_reverse_conversation(&mut app, conversation);
        let stale = app.state.clone();
        app.accept_gpt_reverse_transfer(receipt).unwrap();
        assert_eq!(app.state.active_conversation_id, id);
        assert_eq!(app.state.inference_provider, InferenceProvider::Grok);
        assert_eq!(app.state.active_conversation().workspace.as_ref(), Some(&workspace));
        assert!(!app.state.active_conversation().is_gpt());
        assert!(app.state.active_conversation().gpt.is_none());
        assert_eq!(app.state.active_conversation().archived_gpt_thread_ids, vec!["saved-gpt-work"]);
        assert_eq!(app.state.active_conversation().messages[0].content, "Preserve the original goal");
        assert_eq!(app.state.active_conversation().messages[1].content, "GPT progress");
        assert!(!app.is_generating());
        assert_eq!(fs::read_to_string(workspace.join("current.rs")).unwrap(), "current workspace contents");
        let mut recovered = stale;
        crate::persistence::recover_gpt_bindings(&mut recovered, &binding_root).unwrap();
        assert_eq!(recovered.inference_provider, InferenceProvider::Grok);
        assert!(!recovered.active_conversation().is_gpt());
        assert!(recovered.active_conversation().gpt.is_none());
        assert_eq!(AgentSession::open(&agent_root, id).unwrap().snapshot().status, SessionStatus::Interrupted);
    }

    #[test]
    fn gpt_reverse_return_to_original_survives_stale_recovery_and_roundtrip_starts_fresh_gpt_context() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("workspace")).unwrap();
        let workspace = fs::canonicalize(temp.path().join("workspace")).unwrap();
        let agent_root = temp.path().join("AgentSessions");
        let binding_root = temp.path().join("GptBindings");
        let conversation = reverse_conversation(&workspace, true);
        let id = conversation.id;
        let mut original = AgentSession::create(&agent_root, id, &workspace).unwrap();
        original.import_messages(vec![AgentMessage::text(AgentRole::User, "Original goal before GPT")]).unwrap();
        let stale_snapshot = original.snapshot();
        drop(original);
        let mut handoff = reverse_records(&workspace);
        handoff.records[0].content = "New correction from GPT work".into();
        let snapshot = import_gpt_reverse_work(&agent_root, &conversation, Some(&handoff)).unwrap();
        assert_eq!(snapshot.goal, "Original goal before GPT");
        let receipt = commit_gpt_reverse_work(&binding_root, &conversation, snapshot, &AtomicU8::new(TRANSFER_ACTIVE)).unwrap();
        let mut app = test_app(Arc::new(RecordingServices::default()));
        attach_reverse_conversation(&mut app, conversation);
        app.accept_gpt_reverse_transfer(receipt).unwrap();
        app.remember_agent_snapshot(stale_snapshot);
        assert!(app.state.active_conversation().messages.iter().any(|message| message.content == "GPT progress"));
        app.state.switch_provider(InferenceProvider::Gpt);
        assert_eq!(app.state.active_conversation_id, id);
        let gpt = app.state.active_conversation().gpt.as_ref().unwrap();
        assert!(gpt.pending_handoff && gpt.handoff_from_taceta);
        assert!(gpt.thread_id.is_none());
        let updated = AgentSession::open(&agent_root, id).unwrap().export_handoff(&workspace).unwrap();
        assert!(updated.contains("Original goal before GPT"));
        assert!(updated.contains("New correction from GPT work"));
        assert!(updated.contains("GPT progress"));
        assert!(updated.contains("Saved test output"));
        // An earlier ownership checkpoint cannot undo a later explicit
        // forward selection that eframe has already saved.
        crate::persistence::recover_gpt_bindings(&mut app.state, &binding_root).unwrap();
        assert_eq!(app.state.inference_provider, InferenceProvider::Gpt);
        assert!(app.state.active_conversation().gpt.as_ref().unwrap().thread_id.is_none());
    }

    #[test]
    fn gpt_reverse_unstarted_forward_restores_original_without_export_or_new_journal() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("workspace")).unwrap();
        let workspace = fs::canonicalize(temp.path().join("workspace")).unwrap();
        let agent_root = temp.path().join("AgentSessions");
        let binding_root = temp.path().join("GptBindings");
        let mut conversation = reverse_conversation(&workspace, true);
        let mut original = AgentSession::create(&agent_root, conversation.id, &workspace).unwrap();
        original.import_messages(vec![AgentMessage::text(AgentRole::User, "Original task")]).unwrap();
        let before = original.snapshot();
        drop(original);
        let gpt = conversation.gpt.as_mut().unwrap();
        gpt.thread_id = None;
        gpt.pending_handoff = true;
        let snapshot = import_gpt_reverse_work(&agent_root, &conversation, None).unwrap();
        assert_eq!(snapshot.event_count, before.event_count);
        assert_eq!(snapshot.status, before.status);
        let receipt = commit_gpt_reverse_work(&binding_root, &conversation, snapshot, &AtomicU8::new(TRANSFER_ACTIVE)).unwrap();
        assert_eq!(receipt.conversation.id, conversation.id);
        assert_eq!(receipt.conversation.provider, Some(InferenceProvider::Grok));
        assert!(receipt.conversation.gpt.is_none());
        assert_eq!(AgentSession::list_ids(&agent_root).unwrap(), vec![conversation.id]);

        // A created but unused Codex thread needs an authoritative empty
        // export before the same direct restoration is accepted.
        conversation.gpt.as_mut().unwrap().thread_id = Some("saved-gpt-work".into());
        assert!(import_gpt_reverse_work(&agent_root, &conversation, None).is_err());
        let mut empty = reverse_records(&workspace);
        empty.records.clear();
        assert_eq!(import_gpt_reverse_work(&agent_root, &conversation, Some(&empty)).unwrap().event_count, before.event_count);
    }

    #[test]
    fn gpt_reverse_missing_original_failed_import_and_cancelled_commit_preserve_gpt() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("workspace")).unwrap();
        let workspace = fs::canonicalize(temp.path().join("workspace")).unwrap();
        let agent_root = temp.path().join("AgentSessions");
        let binding_root = temp.path().join("GptBindings");
        let conversation = reverse_conversation(&workspace, true);
        let handoff = reverse_records(&workspace);
        assert!(import_gpt_reverse_work(&agent_root, &conversation, Some(&handoff)).is_err());
        assert!(!agent_root.join(conversation.id.to_string()).exists());
        let mut original = AgentSession::create(&agent_root, conversation.id, &workspace).unwrap();
        original.import_messages(vec![AgentMessage::text(AgentRole::User, "Original task")]).unwrap();
        drop(original);
        crate::persistence::save_gpt_binding(&binding_root, &conversation).unwrap();
        let mut invalid = handoff.clone();
        invalid.records[0].id.clear();
        assert!(import_gpt_reverse_work(&agent_root, &conversation, Some(&invalid)).is_err());
        let snapshot = import_gpt_reverse_work(&agent_root, &conversation, Some(&handoff)).unwrap();
        assert!(commit_gpt_reverse_work(&binding_root, &conversation, snapshot, &AtomicU8::new(TRANSFER_CANCELLED)).is_err());
        assert!(conversation.is_gpt());
        assert_eq!(conversation.gpt.as_ref().unwrap().thread_id.as_deref(), Some("saved-gpt-work"));
        let mut state = PersistedAppState::default();
        state.active_conversation_id = conversation.id;
        state.conversations = vec![conversation];
        crate::persistence::recover_gpt_bindings(&mut state, &binding_root).unwrap();
        assert_eq!(state.inference_provider, InferenceProvider::Gpt);
        assert!(state.active_conversation().is_gpt());
        assert!(AgentSession::open(&agent_root, state.active_conversation_id).unwrap().snapshot().messages.iter().any(|message| message.content.contains("GPT progress")));
    }

    #[test]
    fn gpt_reverse_failed_ownership_save_keeps_gpt_and_retry_reuses_durable_import() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("workspace")).unwrap();
        let workspace = fs::canonicalize(temp.path().join("workspace")).unwrap();
        let agent_root = temp.path().join("AgentSessions");
        let binding_root = temp.path().join("GptBindings");
        let blocked_root = temp.path().join("blocked-binding-root");
        fs::write(&blocked_root, "not a directory").unwrap();
        let conversation = reverse_conversation(&workspace, false);
        let handoff = reverse_records(&workspace);
        crate::persistence::save_gpt_binding(&binding_root, &conversation).unwrap();
        let snapshot = import_gpt_reverse_work(&agent_root, &conversation, Some(&handoff)).unwrap();
        let imported_count = snapshot.event_count;
        assert!(commit_gpt_reverse_work(&blocked_root, &conversation, snapshot, &AtomicU8::new(TRANSFER_ACTIVE)).is_err());
        assert!(conversation.is_gpt());
        let retry = import_gpt_reverse_work(&agent_root, &conversation, Some(&handoff)).unwrap();
        assert_eq!(retry.event_count, imported_count);
        let receipt = commit_gpt_reverse_work(&binding_root, &conversation, retry, &AtomicU8::new(TRANSFER_ACTIVE)).unwrap();
        assert_eq!(receipt.conversation.id, conversation.id);
        assert_eq!(receipt.conversation.provider, Some(InferenceProvider::Grok));
    }

    #[test]
    fn gpt_reverse_transfer_blocks_owner_changes_and_send_until_cancel_is_recorded() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        attach_reverse_conversation(&mut app, reverse_conversation(std::path::Path::new("/fixture/workspace"), false));
        let id = app.state.active_conversation_id;
        let (tx, result) = std_mpsc::channel();
        let (cancel, cancelled) = watch::channel(false);
        let phase = Arc::new(AtomicU8::new(TRANSFER_ACTIVE));
        app.gpt_ui.reverse_transfer = Some(GptReverseTransfer {
            task: app.runtime.spawn(std::future::pending()), result, cancel, phase: Arc::clone(&phase),
        });
        app.select_provider(InferenceProvider::Ollama);
        app.state.draft = "must not start yet".into();
        app.start_generation();
        assert_eq!(app.state.inference_provider, InferenceProvider::Gpt);
        assert_eq!(app.state.active_conversation_id, id);
        assert!(app.gpt_ui.active.is_none());
        assert!(app.is_generating());
        app.stop_generation();
        assert_eq!(phase.load(Ordering::Acquire), TRANSFER_CANCELLED);
        assert!(*cancelled.borrow());
        assert!(app.is_generating());
        tx.send(Err("cancelled before ownership commit".into())).unwrap();
        app.drain_gpt_reverse_transfer();
        assert!(!app.is_generating());
        assert_eq!(app.state.inference_provider, InferenceProvider::Gpt);
        assert_eq!(app.state.draft, "must not start yet");
    }

    fn active_fixture(app: &mut TacetaApp) -> (
        std_mpsc::Sender<Result<GptRunOutcome, String>>,
        mpsc::UnboundedReceiver<GptControl>, watch::Receiver<bool>,
    ) {
        app.state.switch_provider(InferenceProvider::Gpt);
        app.backend = None;
        let (_event_tx, events) = mpsc::unbounded_channel();
        let (tx, result) = std_mpsc::channel();
        let (controls, control_rx) = mpsc::unbounded_channel();
        let (cancel, cancel_rx) = watch::channel(false);
        app.gpt_ui.active = Some(ActiveGpt {
            task: app.runtime.spawn(std::future::pending()),
            conversation_id: app.state.active_conversation_id,
            events, result, cancel, controls,
            approvals: VecDeque::new(), questions: VecDeque::new(), progress: String::new(),
            pending_thread_save: None, binding_needs_save: false, save_error: None,
            submitted_prompt: None,
        });
        (tx, control_rx, cancel_rx)
    }

    fn approval(id: Uuid) -> GptEvent {
        GptEvent::Approval(GptApproval { id, description: "Edit the selected file".into(), details: "old -> new".into() })
    }

    fn questions(id: Uuid) -> GptEvent {
        GptEvent::Questions { id, questions: vec![GptQuestion {
            id: "choice".into(), header: "Scope".into(), question: "Choose the scope".into(),
            options: vec![GptQuestionOption { label: "Selected file".into(), description: "Keep the task focused".into() }],
            is_secret: false,
        }] }
    }

    #[test]
    fn gpt_dynamic_effort_controls_use_only_advertised_options_and_preserve_ollama_levels() {
        let capability = ThinkingCapability::Efforts {
            supported: vec![ReasoningEffort::Low, ReasoningEffort::High, ReasoningEffort::XHigh],
            default: Some(ReasoningEffort::High),
        };
        assert_eq!(TacetaApp::default_thinking_mode(&capability), ThinkingMode::Effort(ReasoningEffort::High));
        assert_eq!(TacetaApp::normalized_thinking_mode(&capability, ThinkingMode::Effort(ReasoningEffort::XHigh)), ThinkingMode::Effort(ReasoningEffort::XHigh));
        assert_eq!(TacetaApp::normalized_thinking_mode(&capability, ThinkingMode::Effort(ReasoningEffort::Ultra)), ThinkingMode::Effort(ReasoningEffort::High));
        assert_eq!(TacetaApp::normalized_thinking_mode(&capability, ThinkingMode::Off), ThinkingMode::Effort(ReasoningEffort::High));
        assert_eq!(TacetaApp::normalized_thinking_mode(&ThinkingCapability::Levels, ThinkingMode::Off), ThinkingMode::Level(ThinkingLevel::Low));
        assert_eq!(TacetaApp::normalized_thinking_mode(&ThinkingCapability::Toggle, ThinkingMode::Off), ThinkingMode::Off);
        assert_eq!(TacetaApp::default_thinking_mode(&ThinkingCapability::Efforts { supported: vec![], default: Some(ReasoningEffort::Ultra) }), ThinkingMode::Default);
    }

    #[test]
    fn gpt_streamed_items_replace_final_text_and_trace_visibility_does_not_change_content() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let (_tx, _controls, _cancel) = active_fixture(&mut app);
        app.state.show_thinking_trace = false;
        app.apply_gpt_event(GptEvent::MessageStarted { id: "first".into() });
        app.apply_gpt_event(GptEvent::ContentDelta { id: "first".into(), text: "draft".into() });
        app.apply_gpt_event(GptEvent::ThinkingDelta("private reasoning".into()));
        app.apply_gpt_event(GptEvent::MessageCompleted { id: "first".into(), text: "Final answer".into() });
        app.apply_gpt_event(GptEvent::ContentDelta { id: "second".into(), text: "Another item".into() });
        app.apply_gpt_event(GptEvent::MessageCompleted { id: "first".into(), text: "Final answer".into() });
        let conversation = app.state.active_conversation();
        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(conversation.messages[0].content, "Final answer");
        assert_eq!(conversation.messages[1].content, "Another item");
        assert_eq!(conversation.gpt.as_ref().unwrap().thinking, "private reasoning");
        assert!(conversation.messages.iter().all(|message| message.thinking.is_empty()));
    }

    #[test]
    fn gpt_approval_question_resolution_and_terminal_exits_clear_all_pending_input() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let (_tx, mut controls, cancel) = active_fixture(&mut app);
        let approval_id = Uuid::new_v4();
        let question_id = Uuid::new_v4();
        app.apply_gpt_event(approval(approval_id));
        app.apply_gpt_event(questions(question_id));
        app.decide_gpt_approval(true);
        app.decide_gpt_approval(false);
        assert!(matches!(controls.try_recv().unwrap(), GptControl::Approval { id, approved: true } if id == approval_id));
        assert!(controls.try_recv().is_err());
        app.apply_gpt_event(GptEvent::RequestResolved { id: question_id });
        assert!(app.gpt_ui.active.as_ref().unwrap().questions.is_empty());
        app.apply_gpt_event(approval(Uuid::new_v4()));
        app.apply_gpt_event(questions(Uuid::new_v4()));
        app.state.show_thinking_trace = false;
        app.stop_generation();
        assert!(*cancel.borrow());
        assert!(app.is_generating());
        assert!(app.gpt_ui.active.as_ref().unwrap().approvals.is_empty());
        assert!(app.gpt_ui.active.as_ref().unwrap().questions.is_empty());
        app.finish_gpt_run(Err("Interrupted transport".into()));
        assert!(!app.is_generating());
        assert_eq!(app.state.active_conversation().gpt.as_ref().unwrap().status, GptRunStatus::Interrupted);

        let (tx, _controls, _cancel) = active_fixture(&mut app);
        app.apply_gpt_event(approval(Uuid::new_v4()));
        app.apply_gpt_event(questions(Uuid::new_v4()));
        drop(tx);
        app.drain_gpt_work();
        assert!(app.gpt_ui.active.is_none());
        assert_eq!(app.state.active_conversation().gpt.as_ref().unwrap().status, GptRunStatus::Failed);
        assert!(app.state.active_conversation().gpt.as_ref().unwrap().error.as_ref().unwrap().contains("before a final outcome"));
    }

    #[test]
    fn gpt_question_answers_keep_typed_request_and_question_ids() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let (_tx, mut controls, _cancel) = active_fixture(&mut app);
        let request_id = Uuid::new_v4();
        app.apply_gpt_event(questions(request_id));
        app.gpt_ui.active.as_mut().unwrap().questions.front_mut().unwrap().answers[0] = "Selected file".into();
        app.send_gpt_answers();
        match controls.try_recv().unwrap() {
            GptControl::Answers { id, answers } => {
                assert_eq!(id, request_id);
                assert_eq!(answers, vec![("choice".into(), "Selected file".into())]);
            }
            _ => panic!("Expected typed answers"),
        }
        assert!(app.gpt_ui.active.as_ref().unwrap().questions.is_empty());
        assert!(app.state.active_conversation().messages.is_empty());
    }

    #[test]
    fn gpt_thread_ack_requires_durable_checkpoint_and_storage_failure_cancels_without_ack() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let (_tx, mut controls, _cancel) = active_fixture(&mut app);
        app.apply_gpt_event(GptEvent::ThreadReady { thread_id: "saved-before-turn".into() });
        assert!(controls.try_recv().is_err());
        app.persist_gpt_thread(None);
        assert!(matches!(controls.try_recv().unwrap(), GptControl::ThreadSaved { thread_id } if thread_id == "saved-before-turn"));
        let root = app.gpt_ui.bindings_root.as_ref().unwrap();
        let mut recovered = PersistedAppState::default();
        crate::persistence::recover_gpt_bindings(&mut recovered, root).unwrap();
        assert!(recovered.conversations.iter().any(|conversation| conversation.gpt.as_ref().is_some_and(|gpt| gpt.thread_id.as_deref() == Some("saved-before-turn"))));
        fs::remove_file(root.join(format!("{}.json", app.state.active_conversation_id))).unwrap();
        fs::remove_dir(root).unwrap();

        let mut blocked = test_app(Arc::new(RecordingServices::default()));
        let (_tx, mut controls, cancel) = active_fixture(&mut blocked);
        let path = blocked.gpt_ui.bindings_root.as_ref().unwrap().clone();
        fs::write(&path, "a file cannot be a history directory").unwrap();
        blocked.apply_gpt_event(GptEvent::ThreadReady { thread_id: "must-not-start".into() });
        blocked.persist_gpt_thread(None);
        assert!(controls.try_recv().is_err());
        assert!(*cancel.borrow());
        assert!(blocked.gpt_ui.active.as_ref().unwrap().save_error.is_some());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn gpt_handoff_failure_keeps_pending_context_and_submission_until_explicit_acceptance() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let (_tx, _controls, _cancel) = active_fixture(&mut app);
        let session = app.state.active_conversation_mut().gpt.as_mut().unwrap();
        session.handoff_from_taceta = true;
        session.pending_handoff = true;
        app.gpt_ui.active.as_mut().unwrap().submitted_prompt = Some(("Continue the saved work".into(), vec![]));
        app.finish_gpt_run(Err("The source task could not be opened".into()));
        assert_eq!(app.state.draft, "Continue the saved work");
        assert!(app.state.active_conversation().gpt.as_ref().unwrap().pending_handoff);
        let (_tx, _controls, _cancel) = active_fixture(&mut app);
        app.apply_gpt_event(GptEvent::HandoffAccepted);
        let session = app.state.active_conversation().gpt.as_ref().unwrap();
        assert!(session.handoff_from_taceta);
        assert!(!session.pending_handoff);
        assert!(app.gpt_ui.active.as_ref().unwrap().binding_needs_save);
    }
}
