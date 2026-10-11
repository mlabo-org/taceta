//! Summarizing the older part of a conversation when it no longer fits the
//! engine about to answer. Nothing is dropped: the summary is stored beside
//! the messages, and a turn whose context cannot be made to fit is not sent.

use super::*;
use taceta::{
    cli_agent::{ApprovalPolicy, CliAgent, Control, Event, RunRequest, RunStatus},
    gpt::{GptAction, GptControl, GptEvent, GptMode, GptRunRequest, GptRunStatus},
    prior_context::{CompactionJob, Plan, Summary, plan, summarize_request},
};
use tokio::sync::watch;

/// Used when an engine does not report its model's context size.
const ASSUMED_CONTEXT_TOKENS: u32 = 200_000;
const SUMMARY_TIME_LIMIT_SECS: u64 = 600;

/// Characters of earlier conversation an engine may be handed. Japanese text
/// runs at roughly one token per character, so half the context in
/// characters leaves the other half for the request and the answer.
pub(super) fn handover_budget(context_tokens: Option<u32>) -> usize {
    context_tokens.unwrap_or(ASSUMED_CONTEXT_TOKENS) as usize / 2
}

/// The engine that is about to answer, used to write the summary it needs.
pub(super) enum Summarizer {
    Local { backend: Arc<dyn InferenceBackend>, model: String, context_length: u32 },
    Cli { agent: Arc<dyn CliAgent>, model: Option<String>, workspace: PathBuf },
    Gpt { client: taceta::gpt::GptClient, model: String },
}

impl Summarizer {
    async fn summarize(&self, request: String) -> Result<String, String> {
        let summary = match self {
            Self::Local { backend, model, context_length } => {
                let (events, mut received) = mpsc::unbounded_channel();
                backend
                    .stream_chat(
                        ChatRequest {
                            model: model.clone(),
                            messages: vec![ChatMessage::new_user(request)],
                            thinking: ThinkingMode::Default,
                            context_length: *context_length,
                            web_search_provider: None,
                            max_search_results: 1,
                            chatgpt_web_request_limit: 1,
                            fetch_search_pages: false,
                            web_authorization: None,
                        },
                        events,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                let mut text = String::new();
                while let Ok(event) = received.try_recv() {
                    match event {
                        GenerationEvent::ContentDelta(delta) => text.push_str(&delta),
                        GenerationEvent::ReplaceContent(content) => text = content,
                        _ => {}
                    }
                }
                text
            }
            Self::Cli { agent, model, workspace } => {
                let (events, mut received) = mpsc::unbounded_channel();
                let (controls, control_rx) = mpsc::unbounded_channel();
                let (_cancel, cancel_rx) = watch::channel(false);
                let run = agent.run(
                    RunRequest {
                        session_id: None,
                        workspace: workspace.clone(),
                        model: model.clone(),
                        effort: None,
                        prompt: request,
                        // Writing a summary needs no tools at all.
                        approval: ApprovalPolicy::NoTools,
                        max_duration_secs: SUMMARY_TIME_LIMIT_SECS,
                    },
                    events,
                    control_rx,
                    cancel_rx,
                );
                tokio::pin!(run);
                let mut text = String::new();
                let outcome = loop {
                    tokio::select! {
                        outcome = &mut run => break outcome,
                        Some(event) = received.recv() => match event {
                            Event::MessageDelta(delta) => text.push_str(&delta),
                            // Writing a summary needs no action on the workspace.
                            Event::Approval(request) => {
                                let _ = controls.send(Control::Approval { id: request.id, approved: false });
                            }
                            _ => {}
                        },
                    }
                };
                while let Ok(event) = received.try_recv() {
                    if let Event::MessageDelta(delta) = event {
                        text.push_str(&delta);
                    }
                }
                if outcome.status != RunStatus::Completed {
                    return Err(outcome.error.unwrap_or_else(|| "The engine did not finish the summary.".into()));
                }
                text
            }
            Self::Gpt { client, model } => {
                let (events, mut received) = mpsc::unbounded_channel();
                let (controls, control_rx) = mpsc::unbounded_channel();
                let (_cancel, cancel_rx) = watch::channel(false);
                let run = client.run(
                    GptRunRequest {
                        thread_id: None,
                        workspace: None,
                        mode: GptMode::Chat,
                        model: model.clone(),
                        thinking: ThinkingMode::Default,
                        action: GptAction::Send { text: request, attachments: Vec::new() },
                        handoff: None,
                        max_duration_secs: SUMMARY_TIME_LIMIT_SECS,
                        max_tool_actions: 1,
                    },
                    events,
                    control_rx,
                    cancel_rx,
                );
                tokio::pin!(run);
                let mut messages: Vec<(String, String)> = Vec::new();
                let note = |messages: &mut Vec<(String, String)>, event: GptEvent| match event {
                    GptEvent::ThreadReady { thread_id } => {
                        let _ = controls.send(GptControl::ThreadSaved { thread_id });
                    }
                    GptEvent::ContentDelta { id, text } => match messages.iter_mut().find(|(known, _)| *known == id) {
                        Some((_, existing)) => existing.push_str(&text),
                        None => messages.push((id, text)),
                    },
                    GptEvent::MessageCompleted { id, text } => match messages.iter_mut().find(|(known, _)| *known == id) {
                        Some((_, existing)) => *existing = text,
                        None => messages.push((id, text)),
                    },
                    GptEvent::Approval(approval) => {
                        let _ = controls.send(GptControl::Approval { id: approval.id, approved: false });
                    }
                    _ => {}
                };
                let outcome = loop {
                    tokio::select! {
                        outcome = &mut run => break outcome?,
                        Some(event) = received.recv() => note(&mut messages, event),
                    }
                };
                while let Ok(event) = received.try_recv() {
                    note(&mut messages, event);
                }
                if outcome.status != GptRunStatus::Completed {
                    return Err(outcome.error.unwrap_or_else(|| "GPT did not finish the summary.".into()));
                }
                messages.into_iter().map(|(_, text)| text).collect::<Vec<_>>().join("\n\n")
            }
        };
        let summary = summary.trim().to_owned();
        if summary.is_empty() {
            return Err("The engine returned an empty summary.".into());
        }
        Ok(summary)
    }
}

/// Folds every chunk of `job` into one summary, oldest first.
async fn run_job(summarizer: Summarizer, job: CompactionJob) -> Result<Summary, String> {
    let mut text = job.existing;
    for chunk in &job.chunks {
        text = Some(summarizer.summarize(summarize_request(text.as_deref(), chunk, job.summary_chars)).await?);
    }
    Ok(Summary {
        text: text.ok_or("There was nothing to summarize.")?,
        covers_messages: job.covers_messages,
    })
}

pub(super) struct ActiveCompaction {
    task: JoinHandle<()>,
    conversation_id: Uuid,
    result: std_mpsc::Receiver<Result<Summary, String>>,
}

impl TacetaApp {
    /// The earlier conversation an engine that has seen `seen` messages must
    /// be handed with the next request. Returns `None` when that conversation
    /// first has to be summarized: the summary is started, and the request is
    /// sent again by itself once it is stored.
    pub(super) fn handover_or_compact(
        &mut self,
        seen: usize,
        budget_chars: usize,
        summarizer: impl FnOnce() -> Summarizer,
    ) -> Option<Option<String>> {
        let conversation = self.state.active_conversation();
        let job = match plan(conversation.summary.as_ref(), &conversation.messages, seen, budget_chars) {
            Plan::Ready(context) => return Some(context),
            Plan::Compact(job) => job,
        };
        // A summary that was just written and still leaves too much cannot be
        // improved by writing it again.
        if job.chunks.is_empty() {
            self.notice = Some(Notice {
                kind: NoticeKind::Error,
                text: text(
                    self.language(),
                    "会話を圧縮しても、この接続先の文脈に収まりません。何も送っていません。より大きな文脈を持つモデルを選んでください。",
                    "The conversation does not fit this engine even after compression. Nothing was sent. Choose a model with a larger context.",
                )
                .to_owned(),
            });
            return None;
        }
        let conversation_id = conversation.id;
        let summarizer = summarizer();
        let (result_tx, result) = std_mpsc::channel();
        let task = self.runtime.spawn(async move {
            let _ = result_tx.send(run_job(summarizer, job).await);
        });
        self.compaction = Some(ActiveCompaction { task, conversation_id, result });
        self.notice = Some(Notice {
            kind: NoticeKind::Info,
            text: text(
                self.language(),
                "会話が長くなったため、古い部分を要約しています。終わりしだい送信します。元の発言は残ります。",
                "The conversation has grown long, so its older part is being summarized. Your message is sent when that finishes. The original messages are kept.",
            )
            .to_owned(),
        });
        None
    }

    pub(super) fn drain_compaction(&mut self) {
        let Some(active) = self.compaction.as_ref() else {
            return;
        };
        let result = match active.result.try_recv() {
            Ok(result) => result,
            Err(std_mpsc::TryRecvError::Empty) => return,
            Err(std_mpsc::TryRecvError::Disconnected) => Err("Summarizing the conversation stopped unexpectedly.".into()),
        };
        let conversation_id = active.conversation_id;
        self.compaction = None;
        match result {
            Ok(summary) => {
                if let Some(conversation) =
                    self.state.conversations.iter_mut().find(|conversation| conversation.id == conversation_id)
                {
                    conversation.summary = Some(summary);
                }
                self.notice = None;
                // The request that was waiting for the summary goes out now.
                if conversation_id == self.state.active_conversation_id {
                    self.start_generation();
                }
            }
            Err(error) => {
                self.notice = Some(Notice {
                    kind: NoticeKind::Error,
                    text: format!(
                        "{}: {error}",
                        text(
                            self.language(),
                            "会話を要約できなかったため、何も送っていません",
                            "The conversation could not be summarized, so nothing was sent",
                        )
                    ),
                });
            }
        }
    }

    pub(super) fn stop_compaction(&mut self) {
        if let Some(active) = self.compaction.take() {
            active.task.abort();
            self.notice = Some(Notice {
                kind: NoticeKind::Info,
                text: text(self.language(), "要約を中止しました。何も送っていません。", "Summarizing was stopped. Nothing was sent.").to_owned(),
            });
        }
    }
}
