use super::{
    journal::{EventData, Record},
    *,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub(super) struct MessageGroup {
    pub first_seq: u64,
    pub last_seq: u64,
    pub messages: Vec<AgentMessage>,
    pub complete: bool,
    pub first_message: usize,
    pub last_message: usize,
}

impl MessageGroup {
    pub fn range(&self) -> RetainedHistoryRange {
        RetainedHistoryRange {
            first_seq: self.first_seq,
            last_seq: self.last_seq,
            first_message: self.first_message,
            last_message: self.last_message,
        }
    }
}

pub(super) struct UsageObservation {
    pub model: String,
    pub projected_input_tokens: u64,
}

pub(super) struct State {
    pub snapshot: SessionSnapshot,
    pub groups: Vec<MessageGroup>,
    pub instructions: Vec<(u64, String)>,
    pending: BTreeMap<String, (usize, AgentToolCall)>,
    started: BTreeSet<String>,
    pub usage: Option<UsageObservation>,
    pub previous_model: Option<(String, u32)>,
    current_context_length: u32,
}

impl State {
    pub fn restore(records: &[Record]) -> Result<Self, String> {
        let Some(Record {
            event: EventData::Created { id, workspace },
            ..
        }) = records.first()
        else {
            return Err("Missing session creation event".into());
        };
        let mut state = Self {
            snapshot: SessionSnapshot {
                id: *id,
                workspace: workspace.clone(),
                status: SessionStatus::Interrupted,
                goal: String::new(),
                user_instructions: Vec::new(),
                work_state: WorkState::default(),
                messages: Vec::new(),
                error: None,
                event_count: 0,
                summary: None,
                latest_model: None,
            },
            instructions: Vec::new(),
            groups: Vec::new(),
            pending: BTreeMap::new(),
            started: BTreeSet::new(),
            usage: None,
            previous_model: None,
            current_context_length: 0,
        };
        for record in records {
            state.apply(record)?;
        }
        Ok(state)
    }

    fn user_instruction(&mut self, seq: u64, text: &str) {
        if self.instructions.is_empty() {
            self.snapshot.goal = text.to_owned();
        }
        self.instructions.push((seq, text.to_owned()));
        self.snapshot.user_instructions.push(text.to_owned());
    }

    pub fn apply(&mut self, record: &Record) -> Result<(), String> {
        match &record.event {
            EventData::Created { .. }
            | EventData::InferenceStarted { .. }
            | EventData::Recovery { .. } => {}
            EventData::InterruptedOutput { content } => {
                // Visible to the user, excluded from the model's message groups.
                self.snapshot.messages.push(AgentMessage::text(
                    AgentRole::Assistant,
                    format!("[Interrupted response; excluded from later model input]\n{content}"),
                ));
            }
            EventData::ImportedMessages { messages } => {
                for (index, message) in messages.iter().enumerate() {
                    if message.role == AgentRole::User {
                        self.user_instruction(record.seq, &message.content);
                    } else if message.role == AgentRole::Assistant {
                        self.groups.push(MessageGroup {
                            first_seq: record.seq,
                            last_seq: record.seq,
                            messages: vec![message.clone()],
                            complete: true,
                            first_message: index,
                            last_message: index,
                        });
                    }
                    self.snapshot.messages.push(message.clone());
                }
            }
            EventData::UserInput { content } => {
                self.user_instruction(record.seq, content);
                // New durable input is unfinished even if the process exits
                // before the following RunStarted event is written.
                self.snapshot.status = SessionStatus::Interrupted;
                self.snapshot.error = None;
                if let Some(usage) = &mut self.usage {
                    usage.projected_input_tokens = usage
                        .projected_input_tokens
                        .saturating_add(content.len() as u64 + 384);
                }
                self.snapshot
                    .messages
                    .push(AgentMessage::text(AgentRole::User, content));
            }
            EventData::RunStarted {
                model,
                context_length,
                ..
            } => {
                if let Some(previous) = &self.snapshot.latest_model
                    && self.current_context_length > 0
                    && (previous != model || self.current_context_length != *context_length)
                {
                    self.previous_model = Some((previous.clone(), self.current_context_length));
                }
                self.current_context_length = *context_length;
                self.snapshot.latest_model = Some(model.clone());
                self.snapshot.status = SessionStatus::Running;
                self.snapshot.error = None;
            }
            EventData::AssistantTurn { turn } => {
                let mut message = AgentMessage::text(AgentRole::Assistant, &turn.content);
                message.tool_calls = turn.tool_calls.clone();
                let index = self.groups.len();
                self.usage = turn.prompt_tokens.map(|tokens| UsageObservation {
                    model: self.snapshot.latest_model.clone().unwrap_or_default(),
                    projected_input_tokens: tokens.saturating_add(
                        turn.completion_tokens.unwrap_or_else(|| {
                            context::cost(std::slice::from_ref(&message), &[]) as u64
                        }),
                    ),
                });
                for call in &turn.tool_calls {
                    if self
                        .pending
                        .insert(call.id.clone(), (index, call.clone()))
                        .is_some()
                    {
                        return Err(
                            "Journal contains ambiguous duplicate pending tool call IDs".into()
                        );
                    }
                }
                self.groups.push(MessageGroup {
                    first_seq: record.seq,
                    last_seq: record.seq,
                    messages: vec![message.clone()],
                    complete: turn.tool_calls.is_empty(),
                    first_message: 0,
                    last_message: 0,
                });
                self.snapshot.messages.push(message);
            }
            EventData::ApprovalRequested { .. } => {
                self.snapshot.status = SessionStatus::AwaitingApproval
            }
            EventData::ApprovalResolved { .. } => self.snapshot.status = SessionStatus::Running,
            EventData::ToolStarted { call } => {
                self.started.insert(call.id.clone());
            }
            EventData::ToolResult {
                call_id,
                name,
                output,
                is_error,
            } => {
                let mut message = AgentMessage::text(AgentRole::Tool,
                    serde_json::json!({"untrusted_tool_result": true, "is_error": is_error, "output": output}).to_string());
                message.tool_call_id = Some(call_id.clone());
                message.tool_name = Some(name.clone());
                let Some((index, _)) = self.pending.remove(call_id) else {
                    return Err(format!(
                        "Journal tool result has no matching pending call: {call_id}"
                    ));
                };
                self.started.remove(call_id);
                let group = &mut self.groups[index];
                group.messages.push(message.clone());
                group.last_seq = record.seq;
                group.last_message = 0;
                group.complete = !self.pending.values().any(|(other, _)| *other == index);
                self.snapshot.messages.push(message);
                if let Some(usage) = &mut self.usage {
                    usage.projected_input_tokens = usage.projected_input_tokens.saturating_add(
                        context::cost(
                            self.snapshot
                                .messages
                                .last()
                                .map(std::slice::from_ref)
                                .unwrap_or(&[]),
                            &[],
                        ) as u64,
                    );
                }
            }
            EventData::WorkStateUpdated { state } => {
                if let Some(usage) = &mut self.usage {
                    let old_size = serde_json::to_vec(&self.snapshot.work_state)
                        .map_err(|e| e.to_string())?
                        .len();
                    let new_size = serde_json::to_vec(state).map_err(|e| e.to_string())?.len();
                    usage.projected_input_tokens = usage
                        .projected_input_tokens
                        .saturating_add(new_size.saturating_sub(old_size) as u64);
                }
                self.snapshot.work_state = state.clone();
            }
            EventData::Compacted { summary } => {
                self.snapshot.summary = Some(summary.clone());
                self.usage = None;
            }
            EventData::StatusChanged { status, message } => {
                self.snapshot.status = status.clone();
                self.snapshot.error = message.clone();
            }
        }
        self.snapshot.event_count = record.seq;
        if self.snapshot.messages.len() > 200 {
            self.snapshot
                .messages
                .drain(..self.snapshot.messages.len() - 200);
        }
        Ok(())
    }

    pub fn unresolved(&self) -> Vec<(AgentToolCall, bool)> {
        self.pending
            .values()
            .map(|(_, call)| (call.clone(), self.started.contains(&call.id)))
            .collect()
    }
}
