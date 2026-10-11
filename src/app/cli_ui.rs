//! The task view for coding CLIs driven through `taceta::cli_agent`.
//!
//! The conversation's messages are the record. A CLI's own session is its
//! working context for that conversation: it is resumed while it is current,
//! and given the turns it missed when another engine answered in between.

use super::*;
use std::collections::{HashMap, VecDeque};
use taceta::{
    cli_agent::{
        ApprovalPolicy, ApprovalRequest, CliAgent, Control, Event, ModelOption, Readiness,
        RunOutcome, RunRequest, RunStatus, ToolItem, ToolStatus, claude::ClaudeCode, grok::GrokCli,
    },
    domain::ActionRecord,
    prior_context::{prior_context, with_prior_context},
};
use tokio::sync::watch;

/// Share of a model's context, in characters, that earlier conversation may
/// take when it is handed to a session that has not seen it.
const PRIOR_CONTEXT_CHARS_PER_TOKEN: usize = 2;
/// Used when a CLI does not report its model's context size.
const ASSUMED_CONTEXT_TOKENS: usize = 200_000;
/// Decisions are ignored briefly after a request appears, so a click meant
/// for the previous request cannot answer this one.
const APPROVAL_SETTLE: Duration = Duration::from_millis(600);

pub(super) struct ActiveCli {
    task: JoinHandle<()>,
    conversation_id: Uuid,
    assistant_id: Uuid,
    provider: InferenceProvider,
    events: mpsc::UnboundedReceiver<Event>,
    controls: mpsc::UnboundedSender<Control>,
    cancel: watch::Sender<bool>,
    result: std_mpsc::Receiver<RunOutcome>,
    tools: Vec<ToolItem>,
    approvals: VecDeque<(ApprovalRequest, std::time::Instant)>,
    session_id: Option<String>,
}

pub(super) struct CliUiState {
    claude: Arc<dyn CliAgent>,
    grok: Arc<dyn CliAgent>,
    pub(super) active: Option<ActiveCli>,
    readiness: HashMap<InferenceProvider, Result<Readiness, String>>,
    readiness_result: Option<(InferenceProvider, std_mpsc::Receiver<Result<Readiness, String>>)>,
}

impl CliUiState {
    /// `grok_approval_plugin` is where the Grok driver keeps the small plugin
    /// that makes its CLI ask before each action.
    pub(super) fn new(grok_approval_plugin: PathBuf) -> Self {
        Self {
            claude: Arc::new(ClaudeCode),
            grok: Arc::new(GrokCli::new(grok_approval_plugin)),
            active: None,
            readiness: HashMap::new(),
            readiness_result: None,
        }
    }

    /// The CLI behind `provider`, when that provider is a coding CLI.
    pub(super) fn agent(&self, provider: InferenceProvider) -> Option<Arc<dyn CliAgent>> {
        match provider {
            InferenceProvider::Claude => Some(Arc::clone(&self.claude)),
            InferenceProvider::Grok => Some(Arc::clone(&self.grok)),
            InferenceProvider::Ollama | InferenceProvider::Gpt => None,
        }
    }
}

pub(super) fn model_descriptor(option: ModelOption) -> ModelDescriptor {
    let supported: Vec<ReasoningEffort> =
        option.efforts.iter().filter_map(|effort| ReasoningEffort::parse(effort)).collect();
    // The effort shown is the effort sent. Where the CLI names no default,
    // Taceta starts in the middle of the range it offers.
    let default = option
        .default_effort
        .as_deref()
        .and_then(ReasoningEffort::parse)
        .or_else(|| supported.get(supported.len() / 2).copied());
    ModelDescriptor {
        name: option.id,
        size: 0,
        thinking: if supported.is_empty() {
            ThinkingCapability::None
        } else {
            ThinkingCapability::Efforts { default, supported }
        },
        vision: false,
        tools: true,
        context_length: option.context_window.and_then(|window| u32::try_from(window).ok()),
    }
}

/// The prompt a session receives: the request itself, preceded by the turns
/// of this conversation the session has not seen.
fn prompt_for_session(
    earlier: &[ChatMessage],
    seen_messages: usize,
    prompt: &str,
    context_tokens: Option<u32>,
) -> (String, usize) {
    let unseen = earlier.get(seen_messages.min(earlier.len())..).unwrap_or_default();
    let budget = context_tokens.map_or(ASSUMED_CONTEXT_TOKENS, |tokens| tokens as usize)
        * PRIOR_CONTEXT_CHARS_PER_TOKEN;
    match prior_context(unseen, budget) {
        Some(context) => (with_prior_context(&context, prompt), context.omitted_messages),
        None => (prompt.to_owned(), 0),
    }
}

impl TacetaApp {
    pub(super) fn is_cli_provider(&self) -> bool {
        self.cli_ui.agent(self.state.inference_provider).is_some()
    }

    pub(super) fn refresh_cli_models(&mut self, agent: Arc<dyn CliAgent>, epoch: u64) {
        let result_tx = self.model_result_tx.clone();
        let provider = self.state.inference_provider;
        let (readiness_tx, readiness_rx) = std_mpsc::channel();
        self.cli_ui.readiness_result = Some((provider, readiness_rx));
        self.runtime.spawn(async move {
            let readiness = agent.readiness().await;
            let models = match &readiness {
                Ok(_) => agent.models().await.map(|models| models.into_iter().map(model_descriptor).collect()),
                Err(error) => Err(error.clone()),
            };
            let _ = readiness_tx.send(readiness);
            let _ = result_tx.send((epoch, models));
        });
    }

    pub(super) fn show_cli_controls(&mut self, root_ui: &mut Ui) {
        let language = self.language();
        let provider = self.state.inference_provider;
        Panel::top("taceta-agent-controls").show_inside(root_ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                self.show_provider_selector(ui);
                if self.state.inference_provider != provider {
                    return;
                }
                let workspace = self.state.active_conversation().workspace.clone();
                let label = workspace
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| text(language, "作業フォルダーを選ぶ", "Choose workspace").into());
                if ui
                    .add_enabled(!self.is_generating(), Button::new(label))
                    .on_hover_text(workspace.map(|path| path.display().to_string()).unwrap_or_default())
                    .clicked()
                {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        let conversation = self.state.active_conversation_mut();
                        if conversation.workspace.as_ref() != Some(&path) {
                            // A CLI session belongs to the folder it was started in.
                            conversation.cli_sessions.clear();
                            conversation.workspace = Some(path);
                        }
                    }
                }
                ui.separator();
                ui.label(text(language, "操作の承認", "Approval"));
                ui.add_enabled_ui(!self.is_generating(), |ui| {
                    ui.selectable_value(
                        &mut self.state.cli_ask_every_time,
                        true,
                        text(language, "毎回確認", "Ask every time"),
                    );
                    ui.selectable_value(
                        &mut self.state.cli_ask_every_time,
                        false,
                        text(language, "自動承認", "Automatic"),
                    );
                });
            });
            if self.state.inference_provider != provider {
                return;
            }
            ui.label(
                RichText::new(if self.state.cli_ask_every_time {
                    text(
                        language,
                        "ファイルの変更とコマンドの実行は、1件ずつこの画面で確認します。",
                        "Each file change and command is confirmed here before it runs.",
                    )
                } else {
                    text(
                        language,
                        "CLI自身の権限設定に従います。設定によっては確認なしでファイルの変更とコマンドの実行が行われます。",
                        "The CLI's own permission settings decide. Depending on them, files change and commands run without confirmation.",
                    )
                })
                .small()
                .color(if self.state.cli_ask_every_time {
                    ui.visuals().weak_text_color()
                } else {
                    theme::palette(ui).warning
                }),
            );
            match self.cli_ui.readiness.get(&provider) {
                Some(Ok(readiness)) => {
                    let version = readiness.version.clone().unwrap_or_default();
                    let line = match readiness.signed_in {
                        Some(false) => text(
                            language,
                            "CLIにサインインしていません。ターミナルでCLIを起動してサインインしてください。",
                            "The CLI is not signed in. Start it in a terminal and sign in there.",
                        )
                        .to_owned(),
                        _ => format!(
                            "{version} · {}",
                            text(
                                language,
                                "普段のCLIのログインと設定を使います。会話の文脈はCLIが管理します。",
                                "Uses your normal CLI login and settings. The CLI manages its own context.",
                            )
                        ),
                    };
                    ui.label(RichText::new(line).small().weak());
                }
                Some(Err(error)) => {
                    ui.colored_label(theme::palette(ui).error, error);
                }
                None => {}
            }
            if let Some(usage) = &self.state.active_conversation().cli_usage {
                ui.label(
                    RichText::new(format!(
                        "{}: {} / {} · {}: {}",
                        text(language, "入力 / 出力トークン", "Input / output tokens"),
                        usage.input_tokens,
                        usage.output_tokens,
                        text(language, "コンテキスト上限", "Context capacity"),
                        usage
                            .context_window
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| text(language, "未取得", "Unavailable").into())
                    ))
                    .small()
                    .weak(),
                );
            }
        });
    }

    pub(super) fn start_cli_send(&mut self) {
        let language = self.language();
        let provider = self.state.inference_provider;
        let Some(agent) = self.cli_ui.agent(provider) else {
            return;
        };
        if self.is_generating() {
            return;
        }
        if self.state.draft.trim().is_empty() {
            return;
        }
        let Some(workspace) = self.state.active_conversation().workspace.clone() else {
            self.notice = Some(Notice {
                kind: NoticeKind::Warning,
                text: text(language, "作業フォルダーを選んでください。", "Choose a workspace first.").to_owned(),
            });
            return;
        };
        let model = self.selected_model().cloned();
        let effort = match self.selected_thinking_mode() {
            ThinkingMode::Effort(effort) => Some(effort.as_str().to_owned()),
            _ => None,
        };
        let prompt = std::mem::take(&mut self.state.draft);
        let conversation_id = self.state.active_conversation_id;
        let conversation = self.state.active_conversation_mut();
        if conversation.should_generate_title() {
            conversation.title = conversation_title(&prompt, "Taceta");
        }
        let session = conversation.cli_sessions.get(&provider).cloned();
        let (session_prompt, omitted) = prompt_for_session(
            &conversation.messages,
            session.as_ref().map_or(0, |session| session.seen_messages),
            &prompt,
            model.as_ref().and_then(|model| model.context_length),
        );
        conversation.messages.push(ChatMessage::new_user(prompt));
        let assistant = ChatMessage::new_assistant("");
        let assistant_id = assistant.id;
        conversation.messages.push(assistant);
        if omitted > 0 {
            self.notice = Some(Notice {
                kind: NoticeKind::Warning,
                text: match language {
                    AppShellLanguage::Japanese => format!(
                        "これまでの会話のうち、古い {omitted} 件はこの接続先の文脈に収まらないため渡していません。"
                    ),
                    AppShellLanguage::English => format!(
                        "The {omitted} oldest messages did not fit this engine's context and were not handed over."
                    ),
                },
            });
        } else {
            self.notice = None;
        }
        let request = RunRequest {
            session_id: session.map(|session| session.session_id),
            workspace,
            model: model.map(|model| model.name),
            effort,
            prompt: session_prompt,
            approval: if self.state.cli_ask_every_time {
                ApprovalPolicy::AskEveryTime
            } else {
                ApprovalPolicy::FollowCli
            },
            max_duration_secs: self.state.agent_max_duration_secs,
        };
        let (event_tx, events) = mpsc::unbounded_channel();
        let (controls, control_rx) = mpsc::unbounded_channel();
        let (cancel, cancel_rx) = watch::channel(false);
        let (result_tx, result) = std_mpsc::channel();
        let session_id = request.session_id.clone();
        let task = self.runtime.spawn(async move {
            let outcome = agent.run(request, event_tx, control_rx, cancel_rx).await;
            let _ = result_tx.send(outcome);
        });
        self.cli_ui.active = Some(ActiveCli {
            task,
            conversation_id,
            assistant_id,
            provider,
            events,
            controls,
            cancel,
            result,
            tools: Vec::new(),
            approvals: VecDeque::new(),
            session_id,
        });
        self.scroll_to_bottom = true;
    }

    pub(super) fn drain_cli_work(&mut self) {
        if let Some((provider, receiver)) = &self.cli_ui.readiness_result {
            if let Ok(readiness) = receiver.try_recv() {
                let provider = *provider;
                self.cli_ui.readiness.insert(provider, readiness);
                self.cli_ui.readiness_result = None;
            }
        }
        let mut events = Vec::new();
        let mut outcome = None;
        if let Some(active) = self.cli_ui.active.as_mut() {
            while let Ok(event) = active.events.try_recv() {
                events.push(event);
            }
            match active.result.try_recv() {
                Ok(result) => outcome = Some(result),
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    outcome = Some(RunOutcome::failed(
                        active.session_id.clone(),
                        "The CLI run ended without reporting a result.",
                    ));
                }
                Err(std_mpsc::TryRecvError::Empty) => {}
            }
        }
        for event in events {
            self.apply_cli_event(event);
        }
        if let Some(outcome) = outcome {
            // Output sent just before the result still belongs to this turn.
            let mut late = Vec::new();
            if let Some(active) = self.cli_ui.active.as_mut() {
                while let Ok(event) = active.events.try_recv() {
                    late.push(event);
                }
            }
            for event in late {
                self.apply_cli_event(event);
            }
            self.finish_cli_run(outcome);
        }
    }

    fn apply_cli_event(&mut self, event: Event) {
        let Some(active) = self.cli_ui.active.as_mut() else {
            return;
        };
        let (conversation_id, assistant_id, provider) =
            (active.conversation_id, active.assistant_id, active.provider);
        match event {
            Event::SessionReady { session_id } => {
                active.session_id = Some(session_id.clone());
                if let Some(conversation) =
                    self.state.conversations.iter_mut().find(|conversation| conversation.id == conversation_id)
                {
                    // Recorded at once so a crash mid-turn still finds the session.
                    let seen = conversation.cli_sessions.get(&provider).map_or(0, |session| session.seen_messages);
                    conversation
                        .cli_sessions
                        .insert(provider, crate::persistence::CliSession { session_id, seen_messages: seen });
                }
            }
            Event::MessageDelta(delta) => {
                self.update_assistant(conversation_id, assistant_id, |message| message.content.push_str(&delta));
            }
            Event::ThinkingDelta(delta) => {
                self.update_assistant(conversation_id, assistant_id, |message| message.thinking.push_str(&delta));
            }
            Event::Tool(item) => match active.tools.iter_mut().find(|tool| tool.id == item.id) {
                Some(existing) => *existing = item,
                None => active.tools.push(item),
            },
            Event::Approval(request) => {
                active.approvals.push_back((request, std::time::Instant::now()));
            }
            Event::ApprovalResolved { id } => {
                active.approvals.retain(|(request, _)| request.id != id);
            }
            Event::Usage(usage) => {
                if let Some(conversation) =
                    self.state.conversations.iter_mut().find(|conversation| conversation.id == conversation_id)
                {
                    conversation.cli_usage = Some(crate::persistence::GptUsage {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        context_window: usage.context_window,
                    });
                }
            }
        }
    }

    fn finish_cli_run(&mut self, outcome: RunOutcome) {
        let Some(active) = self.cli_ui.active.take() else {
            return;
        };
        let language = self.language();
        let actions: Vec<ActionRecord> = active
            .tools
            .iter()
            .filter(|tool| tool.status != ToolStatus::Running)
            .map(|tool| ActionRecord {
                title: tool.title.clone(),
                detail: tool.detail.clone(),
                diff: tool.diff.clone(),
                succeeded: tool.status == ToolStatus::Completed,
            })
            .collect();
        let completed = outcome.status == RunStatus::Completed;
        let session_id = outcome.session_id.or(active.session_id);
        if let Some(conversation) =
            self.state.conversations.iter_mut().find(|conversation| conversation.id == active.conversation_id)
        {
            let answered = conversation
                .messages
                .iter_mut()
                .find(|message| message.id == active.assistant_id)
                .map(|message| {
                    message.actions = actions;
                    // A cut-off answer stays visible but is not handed to another engine.
                    message.interrupted = !completed;
                    !message.content.trim().is_empty() || !message.actions.is_empty()
                })
                .unwrap_or(false);
            if !answered {
                conversation.messages.retain(|message| message.id != active.assistant_id);
            }
            if let Some(session_id) = session_id {
                // The CLI's session holds this turn whether or not it finished.
                let seen_messages = conversation.messages.len();
                conversation
                    .cli_sessions
                    .insert(active.provider, crate::persistence::CliSession { session_id, seen_messages });
            }
        }
        self.notice = match outcome.status {
            RunStatus::Completed => self.notice.take(),
            RunStatus::Interrupted => Some(Notice {
                kind: NoticeKind::Info,
                text: text(language, "作業を停止しました。", "The task was stopped.").to_owned(),
            }),
            RunStatus::LimitReached => Some(Notice {
                kind: NoticeKind::Warning,
                text: text(
                    language,
                    "時間の上限に達したため作業を停止しました。続きは次の送信で再開できます。",
                    "The task stopped at its time limit. Send again to continue.",
                )
                .to_owned(),
            }),
            RunStatus::Failed => Some(Notice {
                kind: NoticeKind::Error,
                text: outcome.error.unwrap_or_else(|| {
                    text(language, "作業は失敗しました。", "The task failed.").to_owned()
                }),
            }),
        };
        self.scroll_to_bottom = true;
    }

    pub(super) fn stop_cli_run(&mut self) {
        if let Some(active) = &self.cli_ui.active {
            let _ = active.cancel.send(true);
        }
    }

    pub(super) fn abort_cli_on_exit(&mut self) {
        if let Some(active) = self.cli_ui.active.take() {
            let _ = active.cancel.send(true);
            // Dropping the run ends the CLI and the processes it started.
            active.task.abort();
        }
    }

    pub(super) fn show_cli_chat(&mut self, root_ui: &mut Ui) {
        let language = self.language();
        CentralPanel::default().show_inside(root_ui, |ui| {
            self.show_notice(ui);
            ScrollArea::vertical()
                .id_salt("cli-transcript")
                .stick_to_bottom(self.scroll_to_bottom || self.is_generating())
                .show(ui, |ui| {
                    let conversation = self.state.active_conversation();
                    if conversation.messages.is_empty() {
                        ui.label(text(
                            language,
                            "作業フォルダーとモデルを選び、実行したいことを入力してください。",
                            "Choose a workspace and model, then describe the work.",
                        ));
                    }
                    for message in &conversation.messages {
                        self.show_message(ui, message, self.is_generating());
                        for (index, action) in message.actions.iter().enumerate() {
                            show_action(
                                ui,
                                (message.id, index),
                                if action.succeeded { "✓" } else { "✗" },
                                &action.title,
                                &action.detail,
                                action.diff.as_deref(),
                            );
                        }
                    }
                    if let Some(active) = &self.cli_ui.active {
                        if active.conversation_id == conversation.id {
                            for tool in &active.tools {
                                show_action(
                                    ui,
                                    (active.assistant_id, &tool.id),
                                    match tool.status {
                                        ToolStatus::Running => "…",
                                        ToolStatus::Completed => "✓",
                                        ToolStatus::Failed => "✗",
                                    },
                                    &tool.title,
                                    &tool.detail,
                                    tool.diff.as_deref(),
                                );
                            }
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(text(language, "作業中…", "Working…"));
                            });
                        }
                    }
                });
        });
        self.scroll_to_bottom = false;
    }

    pub(super) fn show_cli_approval(&mut self, ctx: &Context) {
        let language = self.language();
        let Some(active) = self.cli_ui.active.as_ref() else {
            return;
        };
        let Some((request, shown_at)) = active.approvals.front() else {
            return;
        };
        let settled = shown_at.elapsed() >= APPROVAL_SETTLE;
        if !settled {
            ctx.request_repaint_after(APPROVAL_SETTLE);
        }
        let waiting = active.approvals.len() - 1;
        let mut decision = None;
        egui::Window::new(text(language, "実行内容の確認", "Approve this action"))
            .id(egui::Id::new("taceta-cli-approval"))
            .collapsible(false)
            .resizable(true)
            .default_width(680.0)
            .show(ctx, |ui| {
                ui.strong(&request.title);
                if waiting > 0 {
                    ui.label(
                        RichText::new(match language {
                            AppShellLanguage::Japanese => format!("このあと {waiting} 件の確認があります。"),
                            AppShellLanguage::English => format!("{waiting} more to confirm after this."),
                        })
                        .small()
                        .weak(),
                    );
                }
                ScrollArea::both().max_height(440.0).show(ui, |ui| show_diff_text(ui, &request.detail));
                ui.add_enabled_ui(settled, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button(text(language, "この1回を許可", "Approve once")).clicked() {
                            decision = Some(true);
                        }
                        if ui.button(text(language, "拒否", "Deny")).clicked() {
                            decision = Some(false);
                        }
                    });
                });
            });
        if let Some(approved) = decision {
            self.decide_cli_approval(approved);
        }
    }

    fn decide_cli_approval(&mut self, approved: bool) {
        if let Some(active) = self.cli_ui.active.as_mut() {
            if let Some((request, _)) = active.approvals.pop_front() {
                let _ = active.controls.send(Control::Approval { id: request.id, approved });
            }
        }
    }
}

fn show_action(
    ui: &mut Ui,
    id: impl std::hash::Hash,
    mark: &str,
    title: &str,
    detail: &str,
    diff: Option<&str>,
) {
    egui::CollapsingHeader::new(format!("{mark} {title}"))
        .id_salt(("cli-action", id))
        .show(ui, |ui| {
            ScrollArea::both().max_height(360.0).show(ui, |ui| {
                if let Some(diff) = diff {
                    show_diff_text(ui, diff);
                }
                if !detail.is_empty() {
                    ui.monospace(detail);
                }
            });
        });
}

/// Draws text in which lines starting with `+ ` and `- ` are a change.
fn show_diff_text(ui: &mut Ui, text: &str) {
    let palette = theme::palette(ui);
    for line in text.lines() {
        let rich = RichText::new(line).monospace();
        ui.label(if line.starts_with("+ ") {
            rich.color(palette.success)
        } else if line.starts_with("- ") {
            rich.color(palette.error)
        } else {
            rich
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::model_manager_tests::{RecordingServices, test_app};

    fn active_fixture(app: &mut TacetaApp) -> (mpsc::UnboundedSender<Event>, mpsc::UnboundedReceiver<Control>, std_mpsc::Sender<RunOutcome>) {
        let (event_tx, events) = mpsc::unbounded_channel();
        let (controls, control_rx) = mpsc::unbounded_channel();
        let (cancel, _cancel_rx) = watch::channel(false);
        let (result_tx, result) = std_mpsc::channel();
        let conversation = app.state.active_conversation_mut();
        conversation.messages.push(ChatMessage::new_user("do it"));
        let assistant = ChatMessage::new_assistant("");
        let assistant_id = assistant.id;
        conversation.messages.push(assistant);
        app.cli_ui.active = Some(ActiveCli {
            task: app.runtime.spawn(std::future::pending()),
            conversation_id: app.state.active_conversation_id,
            assistant_id,
            provider: InferenceProvider::Claude,
            events,
            controls,
            cancel,
            result,
            tools: Vec::new(),
            approvals: VecDeque::new(),
            session_id: None,
        });
        (event_tx, control_rx, result_tx)
    }

    fn tool(id: &str, status: ToolStatus) -> ToolItem {
        ToolItem { id: id.into(), title: format!("Edit {id}"), detail: String::new(), diff: Some("+ x".into()), status }
    }

    #[test]
    fn a_finished_turn_is_recorded_in_the_conversation_with_its_actions_and_session() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let (events, _controls, result) = active_fixture(&mut app);
        events.send(Event::SessionReady { session_id: "session-1".into() }).unwrap();
        events.send(Event::ThinkingDelta("reasoning".into())).unwrap();
        events.send(Event::Tool(tool("a", ToolStatus::Running))).unwrap();
        events.send(Event::Tool(tool("a", ToolStatus::Completed))).unwrap();
        events.send(Event::Tool(tool("b", ToolStatus::Running))).unwrap();
        events.send(Event::MessageDelta("All ".into())).unwrap();
        result
            .send(RunOutcome { session_id: Some("session-1".into()), status: RunStatus::Completed, error: None })
            .unwrap();
        // Sent after the result was queued, as a fast CLI can do.
        events.send(Event::MessageDelta("done".into())).unwrap();
        app.drain_cli_work();

        assert!(app.cli_ui.active.is_none());
        let conversation = app.state.active_conversation();
        let answer = conversation.messages.last().unwrap();
        assert_eq!(answer.content, "All done");
        assert_eq!(answer.thinking, "reasoning");
        assert!(!answer.interrupted);
        // Only finished actions are part of the record.
        assert_eq!(answer.actions.len(), 1);
        assert_eq!(answer.actions[0].title, "Edit a");
        assert!(answer.actions[0].succeeded);
        assert_eq!(
            conversation.cli_sessions[&InferenceProvider::Claude],
            crate::persistence::CliSession { session_id: "session-1".into(), seen_messages: 2 }
        );
    }

    #[test]
    fn a_stopped_turn_keeps_its_partial_answer_out_of_what_other_engines_receive() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let (events, _controls, result) = active_fixture(&mut app);
        events.send(Event::SessionReady { session_id: "session-1".into() }).unwrap();
        events.send(Event::MessageDelta("half".into())).unwrap();
        result
            .send(RunOutcome { session_id: Some("session-1".into()), status: RunStatus::Interrupted, error: None })
            .unwrap();
        app.drain_cli_work();
        let conversation = app.state.active_conversation();
        assert!(conversation.messages.last().unwrap().interrupted);
        assert_eq!(conversation.cli_sessions[&InferenceProvider::Claude].seen_messages, 2);
        let handed_over = prior_context(&conversation.messages, 10_000).unwrap().text;
        assert!(handed_over.contains("do it") && !handed_over.contains("half"));
    }

    #[test]
    fn a_turn_that_produced_nothing_leaves_no_empty_answer() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let (_events, _controls, result) = active_fixture(&mut app);
        result.send(RunOutcome::failed(None, "The CLI was not found.")).unwrap();
        app.drain_cli_work();
        let conversation = app.state.active_conversation();
        assert_eq!(conversation.messages.len(), 1);
        assert!(conversation.cli_sessions.is_empty());
        assert_eq!(app.notice.as_ref().unwrap().text, "The CLI was not found.");
    }

    #[test]
    fn approvals_are_answered_in_order_and_withdrawn_ones_disappear() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        let (events, mut controls, _result) = active_fixture(&mut app);
        let request = |title: &str| ApprovalRequest { id: Uuid::new_v4(), title: title.into(), detail: String::new() };
        let (first, second, third) = (request("one"), request("two"), request("three"));
        for request in [&first, &second, &third] {
            events.send(Event::Approval(request.clone())).unwrap();
        }
        events.send(Event::ApprovalResolved { id: second.id }).unwrap();
        app.drain_cli_work();
        app.decide_cli_approval(true);
        app.decide_cli_approval(false);
        app.decide_cli_approval(true);
        assert_eq!(controls.try_recv().unwrap(), Control::Approval { id: first.id, approved: true });
        assert_eq!(controls.try_recv().unwrap(), Control::Approval { id: third.id, approved: false });
        assert!(controls.try_recv().is_err());
    }

    #[test]
    fn a_session_is_given_only_the_turns_it_has_not_seen() {
        let earlier = vec![
            ChatMessage::new_user("first question"),
            ChatMessage::new_assistant("first answer"),
            ChatMessage::new_user("asked elsewhere"),
            ChatMessage::new_assistant("answered elsewhere"),
        ];
        // A session that is current gets the request alone.
        assert_eq!(prompt_for_session(&earlier, 4, "next", None), ("next".to_owned(), 0));
        // One that missed two turns gets exactly those.
        let (prompt, omitted) = prompt_for_session(&earlier, 2, "next", None);
        assert_eq!(omitted, 0);
        assert!(prompt.contains("asked elsewhere") && prompt.contains("answered elsewhere"));
        assert!(!prompt.contains("first question"));
        assert!(prompt.ends_with("Current request:\nnext"));
        // A new session gets everything.
        assert!(prompt_for_session(&earlier, 0, "next", None).0.contains("first question"));
        // A count beyond the record (messages were deleted) is not an error.
        assert_eq!(prompt_for_session(&earlier, 9, "next", None), ("next".to_owned(), 0));
    }

    #[test]
    fn model_options_become_selectable_models_with_their_efforts() {
        let descriptor = model_descriptor(ModelOption {
            id: "opus".into(),
            label: "Opus".into(),
            context_window: Some(256_000),
            efforts: vec!["low".into(), "high".into(), "not-an-effort".into()],
            default_effort: Some("high".into()),
        });
        assert_eq!(descriptor.name, "opus");
        assert_eq!(descriptor.context_length, Some(256_000));
        assert_eq!(
            descriptor.thinking,
            ThinkingCapability::Efforts {
                supported: vec![ReasoningEffort::Low, ReasoningEffort::High],
                default: Some(ReasoningEffort::High),
            }
        );
        let unnamed_default = model_descriptor(ModelOption {
            id: "sonnet".into(),
            label: "Sonnet".into(),
            context_window: None,
            efforts: ["low", "medium", "high", "xhigh", "max"].map(str::to_owned).to_vec(),
            default_effort: None,
        });
        assert!(matches!(
            unnamed_default.thinking,
            ThinkingCapability::Efforts { default: Some(ReasoningEffort::High), .. }
        ));
    }
}
