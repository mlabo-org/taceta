use super::*;
use serde_json::json;
use std::{
    collections::VecDeque,
    fs::{DirBuilder, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Fixture {
    base: PathBuf,
    store: PathBuf,
    workspace: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let base = PathBuf::from("/private/tmp")
            .join(format!("taceta-agent-session-test-{}", Uuid::new_v4()));
        DirBuilder::new().mode(0o700).create(&base).unwrap();
        let store = base.join("sessions");
        let workspace = base.join("workspace");
        DirBuilder::new().mode(0o700).create(&workspace).unwrap();
        Self {
            base,
            store,
            workspace,
        }
    }
    fn write(&self, path: &str, text: &str) {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.workspace.join(path))
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fn remove_owned(path: &Path) {
            if let Ok(metadata) = fs::symlink_metadata(path) {
                if metadata.is_dir() && !metadata.file_type().is_symlink() {
                    if let Ok(entries) = fs::read_dir(path) {
                        for entry in entries.flatten() {
                            remove_owned(&entry.path());
                        }
                    }
                    let _ = fs::remove_dir(path);
                } else {
                    let _ = fs::remove_file(path);
                }
            }
        }
        remove_owned(&self.base);
    }
}

struct ScriptModel {
    turns: Mutex<VecDeque<AgentTurn>>,
    requests: Mutex<Vec<AgentRequest>>,
    summaries: AtomicUsize,
    summary_error: bool,
}
impl ScriptModel {
    fn new(turns: Vec<AgentTurn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            requests: Mutex::new(Vec::new()),
            summaries: AtomicUsize::new(0),
            summary_error: false,
        }
    }
}
impl AgentModel for ScriptModel {
    fn turn(
        &self,
        request: AgentRequest,
        events: UnboundedSender<ModelDelta>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<AgentTurn, String>> + Send>>
    {
        let compact = request.tools.is_empty();
        self.requests.lock().unwrap().push(request);
        let response = if compact {
            self.summaries.fetch_add(1, Ordering::SeqCst);
            if self.summary_error {
                Err("fixture compaction failure".into())
            } else {
                Ok(answer(
                    "Background only. Original facts are available by their event sequence.",
                ))
            }
        } else {
            self.turns
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "Unexpected inference after fixture script ended".into())
        };
        Box::pin(async move {
            let _ = events.send(ModelDelta::Thinking(
                "PRIVATE_THINKING_MUST_NOT_REENTER".into(),
            ));
            if let Ok(turn) = &response {
                let _ = events.send(ModelDelta::Content(turn.content.clone()));
            }
            tokio::task::yield_now().await;
            response
        })
    }
}
fn answer(text: &str) -> AgentTurn {
    AgentTurn {
        content: text.into(),
        ..AgentTurn::default()
    }
}
fn tool(name: &str, arguments: serde_json::Value) -> AgentTurn {
    AgentTurn {
        tool_calls: vec![AgentToolCall {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            arguments,
        }],
        ..AgentTurn::default()
    }
}
fn config(prompt: Option<&str>) -> RunConfig {
    RunConfig {
        model: "fixture-model".into(),
        thinking: crate::domain::ThinkingMode::Default,
        context_length: 48_000,
        prompt: prompt.map(str::to_owned),
        max_steps: 30,
        max_duration_secs: 30,
        compact_only: false,
    }
}
fn saved_work_state() -> WorkState {
    WorkState {
        tasks: vec![WorkItem {
            id: "task-7".into(),
            description: "Validate src/result.txt, value 731".into(),
            status: WorkItemStatus::InProgress,
        }],
        decisions: vec!["Keep exact value 731".into()],
        constraints: vec!["Never edit secrets.txt".into()],
        artifacts: vec!["src/result.txt".into()],
        resume_point: "task-7 remains unfinished; inspect output before continuing".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loop_waits_for_exact_approvals_and_returns_real_tool_results() {
    let fixture = Fixture::new();
    fixture.write("main.txt", "alpha\n");
    fixture.write("AGENTS.md", "Preserve the fixture's exact value 731.\n");
    let id = Uuid::new_v4();
    let session = AgentSession::create(&fixture.store, id, &fixture.workspace).unwrap();
    let model = Arc::new(ScriptModel::new(vec![
        tool("read_file", json!({"path":"main.txt"})),
        tool(
            "edit_file",
            json!({"path":"main.txt","old_text":"alpha","new_text":"declined"}),
        ),
        tool(
            "edit_file",
            json!({"path":"main.txt","old_text":"alpha","new_text":"beta"}),
        ),
        tool(
            "run_command",
            json!({"command":"/bin/cat main.txt","timeout_ms":3000}),
        ),
        tool(
            "update_work_state",
            serde_json::to_value(saved_work_state()).unwrap(),
        ),
        answer("The approved edit and command finished."),
    ]));
    let (events, mut receiver) = mpsc::unbounded_channel();
    let (approval_sender, approvals) = mpsc::unbounded_channel();
    let (_cancel_sender, cancel) = watch::channel(false);
    let task = tokio::spawn(session.run(
        config(Some(
            "Read the file, propose an edit, and run the approved command.",
        )),
        model.clone(),
        events,
        approvals,
        cancel,
    ));
    let mut approvals_seen = 0;
    let mut old_approval = None;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(15), receiver.recv())
        .await
        .unwrap()
    {
        if let AgentEvent::ApprovalRequired(request) = event {
            approvals_seen += 1;
            match approvals_seen {
                1 => {
                    assert_eq!(
                        fs::read_to_string(fixture.workspace.join("main.txt")).unwrap(),
                        "alpha\n"
                    );
                    assert!(request.details.contains("declined"));
                    old_approval = Some(request.id);
                    approval_sender
                        .send(ApprovalDecision {
                            id: request.id,
                            approved: false,
                        })
                        .unwrap();
                }
                2 => {
                    assert_eq!(
                        fs::read_to_string(fixture.workspace.join("main.txt")).unwrap(),
                        "alpha\n"
                    );
                    assert!(request.details.contains("beta"));
                    // A previous decision cannot authorize the new exact proposal.
                    approval_sender
                        .send(ApprovalDecision {
                            id: old_approval.unwrap(),
                            approved: true,
                        })
                        .unwrap();
                    tokio::task::yield_now().await;
                    assert_eq!(
                        fs::read_to_string(fixture.workspace.join("main.txt")).unwrap(),
                        "alpha\n"
                    );
                    approval_sender
                        .send(ApprovalDecision {
                            id: request.id,
                            approved: true,
                        })
                        .unwrap();
                }
                3 => {
                    assert_eq!(
                        fs::read_to_string(fixture.workspace.join("main.txt")).unwrap(),
                        "beta\n"
                    );
                    assert!(request.details.contains("/bin/cat main.txt"));
                    approval_sender
                        .send(ApprovalDecision {
                            id: request.id,
                            approved: true,
                        })
                        .unwrap();
                }
                _ => panic!("Unexpected extra effect proposal"),
            }
        }
    }
    let snapshot = task.await.unwrap().unwrap();
    assert_eq!(approvals_seen, 3);
    assert_eq!(
        snapshot.status,
        SessionStatus::Completed,
        "{:?}",
        snapshot.error
    );
    assert_eq!(snapshot.work_state, saved_work_state());
    let session = AgentSession::open(&fixture.store, id).unwrap();
    assert!(session.journal.records.iter().any(|record| matches!(&record.event,
        EventData::ToolResult { name, output, is_error: false, .. } if name == "run_command" && output.contains("beta"))));
    let serialized = serde_json::to_string(&session.journal.records).unwrap();
    assert!(!serialized.contains("PRIVATE_THINKING_MUST_NOT_REENTER"));
    for request in model.requests.lock().unwrap().iter() {
        let text = serde_json::to_string(&request.messages).unwrap();
        assert!(!text.contains("PRIVATE_THINKING_MUST_NOT_REENTER"));
        assert!(text.contains("Preserve the fixture's exact value 731"));
        assert!(
            context::cost(&request.messages, &request.tools)
                <= context::input_budget(request.context_length).unwrap()
        );
        let mut pending = BTreeSet::new();
        for message in &request.messages {
            for call in &message.tool_calls {
                assert!(pending.insert(call.id.clone()));
            }
            if message.role == AgentRole::Tool {
                assert!(pending.remove(message.tool_call_id.as_ref().unwrap()));
            }
        }
        assert!(pending.is_empty(), "A call/result group was split");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_preserves_unknown_effects_import_and_recoverable_archive() {
    let fixture = Fixture::new();
    fixture.write("main.txt", "alpha\n");
    let id = Uuid::new_v4();
    let (mut session, created) =
        AgentSession::open_or_create(&fixture.store, id, &fixture.workspace).unwrap();
    assert!(created);
    session
        .import_messages(vec![
            AgentMessage::text(AgentRole::User, "Keep value 731 and continue this task."),
            AgentMessage::text(AgentRole::Assistant, "Earlier plain assistant context."),
        ])
        .unwrap();
    assert!(session.import_messages(Vec::new()).is_err());
    session
        .record(EventData::RunStarted {
            model: "old-model".into(),
            context_length: 48_000,
            max_steps: 10,
            max_duration_secs: 30,
            compact_only: false,
        })
        .unwrap();
    let turn = tool(
        "edit_file",
        json!({"path":"main.txt","old_text":"alpha","new_text":"beta"}),
    );
    let call = turn.tool_calls[0].clone();
    session.record(EventData::AssistantTurn { turn }).unwrap();
    let request = ApprovalRequest {
        id: Uuid::new_v4(),
        tool_call_id: call.id.clone(),
        description: "fixture edit".into(),
        details: "alpha -> beta".into(),
    };
    session
        .record(EventData::ApprovalRequested {
            request: request.clone(),
        })
        .unwrap();
    session
        .record(EventData::ApprovalResolved {
            id: request.id,
            approved: true,
        })
        .unwrap();
    session.record(EventData::ToolStarted { call }).unwrap();
    fixture.write("main.txt", "beta\n"); // crash after effect, before its durable result
    let partial_path = session
        .journal
        .directory
        .join("99999999999999999999.fixture.pending");
    let mut partial = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial_path)
        .unwrap();
    partial.write_all(b"{\"seq\":").unwrap();
    drop(partial);
    let lock = session.journal.lock().unwrap();
    assert!(session.journal.lock().is_err());
    assert!(AgentSession::archive(&fixture.store, id).is_err());
    drop(lock);
    drop(session);
    let session = AgentSession::open(&fixture.store, id).unwrap();
    assert_eq!(session.snapshot().status, SessionStatus::Interrupted);
    let model = Arc::new(ScriptModel::new(vec![
        tool("read_file", json!({"path":"main.txt"})),
        answer("Inspected the previous effect."),
    ]));
    let (events, _receiver) = mpsc::unbounded_channel();
    let (approval_sender, approvals) = mpsc::unbounded_channel();
    approval_sender
        .send(ApprovalDecision {
            id: request.id,
            approved: true,
        })
        .unwrap();
    let (_cancel_sender, cancel) = watch::channel(false);
    let snapshot = session
        .run(config(None), model.clone(), events, approvals, cancel)
        .await
        .unwrap();
    assert_eq!(
        snapshot.status,
        SessionStatus::Completed,
        "{:?}",
        snapshot.error
    );
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("main.txt")).unwrap(),
        "beta\n"
    );
    let requests = model.requests.lock().unwrap();
    assert!(
        requests[0]
            .messages
            .iter()
            .any(|message| message.content.contains("Outcome unknown"))
    );
    assert!(
        requests[0]
            .messages
            .iter()
            .any(|message| message.content.contains("Keep value 731"))
    );
    drop(requests);
    let reopened = AgentSession::open(&fixture.store, id).unwrap();
    assert_eq!(
        fs::read(partial_path.with_file_name("99999999999999999999.fixture.pending.interrupted"))
            .unwrap(),
        b"{\"seq\":"
    );
    assert_eq!(
        fs::metadata(&reopened.journal.directory)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for entry in fs::read_dir(&reopened.journal.directory).unwrap() {
        assert_eq!(
            entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(AgentSession::list_ids(&fixture.store).unwrap(), vec![id]);
    AgentSession::archive(&fixture.store, id).unwrap();
    assert!(AgentSession::list_ids(&fixture.store).unwrap().is_empty());
    assert_eq!(
        fs::read_dir(fixture.store.join("archive")).unwrap().count(),
        1
    );
    AgentSession::archive(&fixture.store, id).unwrap();
}

fn long_session(fixture: &Fixture) -> (AgentSession, u64, Vec<u8>) {
    let mut session =
        AgentSession::create(&fixture.store, Uuid::new_v4(), &fixture.workspace).unwrap();
    session
        .record(EventData::UserInput {
            content: "Never edit secrets.txt. Preserve exact path src/result.txt and number 731."
                .into(),
        })
        .unwrap();
    session
        .record(EventData::UserInput {
            content: "Correction: task-7 is unfinished; never infer that it completed.".into(),
        })
        .unwrap();
    session
        .record(EventData::WorkStateUpdated {
            state: saved_work_state(),
        })
        .unwrap();
    let mut marker_seq = 0;
    for index in 0..80 {
        let turn = tool("read_file", json!({"path":"fixture.txt"}));
        let call = turn.tool_calls[0].clone();
        session.record(EventData::AssistantTurn { turn }).unwrap();
        session
            .record(EventData::ToolResult {
                call_id: call.id,
                name: call.name,
                output: format!(
                    "record {index}; retention-marker-731; {}",
                    "historical fixture output 12345 日本語\n".repeat(120)
                ),
                is_error: false,
            })
            .unwrap();
        if index == 0 {
            marker_seq = session.snapshot().event_count;
        }
    }
    let bytes = fs::read(
        session
            .journal
            .directory
            .join(format!("{marker_seq:020}.json")),
    )
    .unwrap();
    (session, marker_seq, bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_compaction_model_shrink_restart_and_original_retrieval() {
    let fixture = Fixture::new();
    let (session, marker_seq, original_bytes) = long_session(&fixture);
    let id = session.snapshot().id;
    let first_model = Arc::new(ScriptModel::new(Vec::new()));
    let (events, _receiver) = mpsc::unbounded_channel();
    let (_approval_sender, approvals) = mpsc::unbounded_channel();
    let (_cancel_sender, cancel) = watch::channel(false);
    let mut first_config = config(None);
    first_config.max_steps = 100;
    first_config.compact_only = true;
    let snapshot = session
        .run(first_config, first_model.clone(), events, approvals, cancel)
        .await
        .unwrap();
    assert_eq!(
        snapshot.status,
        SessionStatus::Interrupted,
        "{:?}",
        snapshot.error
    );
    assert!(first_model.summaries.load(Ordering::SeqCst) > 1);
    assert!(
        first_model
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.tools.is_empty())
    );
    let first_checkpoint = serde_json::to_value(&snapshot.summary).unwrap();
    let session = AgentSession::open(&fixture.store, id).unwrap();
    assert_eq!(
        serde_json::to_value(&session.snapshot().summary).unwrap(),
        first_checkpoint
    );
    assert_eq!(session.snapshot().work_state, saved_work_state());
    let mut observed_usage_turn = tool(
        "search_history",
        json!({"query":"retention-marker-731","limit":2}),
    );
    observed_usage_turn.prompt_tokens = Some(23_500);
    observed_usage_turn.completion_tokens = Some(50);
    let second_model = Arc::new(ScriptModel::new(vec![
        observed_usage_turn,
        tool("read_history", json!({"seq":marker_seq,"max_bytes":16384})),
        answer("Retrieved the preserved original; task-7 remains unfinished."),
    ]));
    let mut second_config = config(None);
    second_config.model = "smaller-different-model".into();
    second_config.context_length = 24_000;
    second_config.max_steps = 150;
    let (events, _receiver2) = mpsc::unbounded_channel();
    let (_approval_sender2, approvals) = mpsc::unbounded_channel();
    let (_cancel_sender2, cancel) = watch::channel(false);
    let snapshot = session
        .run(
            second_config,
            second_model.clone(),
            events,
            approvals,
            cancel,
        )
        .await
        .unwrap();
    assert_eq!(
        snapshot.status,
        SessionStatus::Completed,
        "{:?}",
        snapshot.error
    );
    assert!(second_model.summaries.load(Ordering::SeqCst) > 1);
    assert_eq!(snapshot.work_state, saved_work_state());
    assert_eq!(snapshot.user_instructions.len(), 2);
    assert!(snapshot.goal.contains("Never edit secrets.txt"));
    let session = AgentSession::open(&fixture.store, id).unwrap();
    assert_eq!(
        fs::read(
            session
                .journal
                .directory
                .join(format!("{marker_seq:020}.json"))
        )
        .unwrap(),
        original_bytes
    );
    assert!(
        session
            .journal
            .records
            .iter()
            .filter(|record| matches!(record.event, EventData::Compacted { .. }))
            .count()
            > 4
    );
    let mut summaries_cover_to = 0;
    let mut previous_window = None;
    for record in &session.journal.records {
        if let EventData::Compacted { summary } = &record.event {
            assert!(summary.to_seq >= summaries_cover_to);
            summaries_cover_to = summary.to_seq;
            assert_eq!(summary.previous_window_id, previous_window);
            assert_eq!(summary.through_event_id, format!("{id}:{}", summary.to_seq));
            assert!(!summary.retained_ranges.is_empty());
            assert!(summary.context_length == 48_000 || summary.context_length == 24_000);
            previous_window = Some(summary.window_id);
        }
    }
    let requests = second_model.requests.lock().unwrap();
    let observed_request = requests
        .iter()
        .position(|request| !request.tools.is_empty())
        .unwrap();
    assert!(
        requests[observed_request + 1].tools.is_empty(),
        "Server prompt tokens plus the new tool result must trigger compaction before the next work inference"
    );
    for request in requests.iter() {
        assert_eq!(request.model, "smaller-different-model");
        assert!(
            context::cost(&request.messages, &request.tools)
                <= context::input_budget(request.context_length).unwrap()
        );
        let raw = serde_json::to_string(&request.messages).unwrap();
        assert!(!raw.contains("PRIVATE_THINKING_MUST_NOT_REENTER"));
        if !request.tools.is_empty() {
            assert!(raw.contains("Never edit secrets.txt"));
            assert!(raw.contains("src/result.txt"));
            assert!(raw.contains("731"));
            assert!(raw.contains("InProgress"));
            assert!(raw.contains("task-7 is unfinished"));
        }
    }
    assert!(
        requests
            .last()
            .unwrap()
            .messages
            .iter()
            .any(|message| message.role == AgentRole::Tool
                && message.content.contains("retention-marker-731"))
    );
    let tools = tool_definitions();
    let too_small = context::assemble(&session.state, &[], &tools, 4096, "tiny-model", false);
    assert!(
        too_small.is_err(),
        "Mandatory instructions must stop instead of being silently truncated"
    );
    let page = session
        .read_history(HistoryRead {
            seq: marker_seq,
            offset: 0,
            max_bytes: 25,
        })
        .unwrap();
    let page: serde_json::Value = serde_json::from_str(&page).unwrap();
    let next = page["next_offset"].as_u64().unwrap() as usize;
    assert_eq!(next, 25);
    let full = session
        .read_history(HistoryRead {
            seq: marker_seq,
            offset: next,
            max_bytes: MAX_HISTORY_PAGE_BYTES,
        })
        .unwrap();
    let full: serde_json::Value = serde_json::from_str(&full).unwrap();
    assert_eq!(
        format!(
            "{}{}",
            page["raw"].as_str().unwrap(),
            full["raw"].as_str().unwrap()
        )
        .as_bytes(),
        original_bytes
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_compaction_keeps_originals_and_reports_failed() {
    let fixture = Fixture::new();
    let (session, marker_seq, original_bytes) = long_session(&fixture);
    let id = session.snapshot().id;
    let mut model = ScriptModel::new(Vec::new());
    model.summary_error = true;
    let (events, _receiver) = mpsc::unbounded_channel();
    let (_approval_sender, approvals) = mpsc::unbounded_channel();
    let (_cancel_sender, cancel) = watch::channel(false);
    let snapshot = session
        .run(config(None), Arc::new(model), events, approvals, cancel)
        .await
        .unwrap();
    assert_eq!(snapshot.status, SessionStatus::Failed);
    assert!(snapshot.error.unwrap().contains("Compaction failed"));
    let session = AgentSession::open(&fixture.store, id).unwrap();
    assert_eq!(
        fs::read(
            session
                .journal
                .directory
                .join(format!("{marker_seq:020}.json"))
        )
        .unwrap(),
        original_bytes
    );
    assert!(session.snapshot().summary.is_none());
}

struct PendingModel;
impl AgentModel for PendingModel {
    fn turn(
        &self,
        _request: AgentRequest,
        _events: UnboundedSender<ModelDelta>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<AgentTurn, String>> + Send>>
    {
        Box::pin(std::future::pending())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_and_total_deadline_are_terminal_without_claiming_completion() {
    let fixture = Fixture::new();
    let first = AgentSession::create(&fixture.store, Uuid::new_v4(), &fixture.workspace).unwrap();
    let (events, _receiver) = mpsc::unbounded_channel();
    let (_approval_sender, approvals) = mpsc::unbounded_channel();
    let (cancel_sender, cancel) = watch::channel(false);
    let task = tokio::spawn(first.run(
        config(Some("Wait for inference.")),
        Arc::new(PendingModel),
        events,
        approvals,
        cancel,
    ));
    tokio::task::yield_now().await;
    cancel_sender.send(true).unwrap();
    assert_eq!(
        task.await.unwrap().unwrap().status,
        SessionStatus::Interrupted
    );
    let second = AgentSession::create(&fixture.store, Uuid::new_v4(), &fixture.workspace).unwrap();
    let (events, _receiver2) = mpsc::unbounded_channel();
    let (_approval_sender2, approvals) = mpsc::unbounded_channel();
    let (_cancel_sender2, cancel) = watch::channel(false);
    let mut bounded = config(Some("Wait for inference within the deadline."));
    bounded.max_duration_secs = 1;
    let start = Instant::now();
    let snapshot = second
        .run(bounded, Arc::new(PendingModel), events, approvals, cancel)
        .await
        .unwrap();
    assert_eq!(snapshot.status, SessionStatus::LimitReached);
    assert!(start.elapsed() < Duration::from_secs(3));
    fixture.write(
        "limit.txt",
        "One permitted read before the inference limit.\n",
    );
    let third = AgentSession::create(&fixture.store, Uuid::new_v4(), &fixture.workspace).unwrap();
    let (events, _receiver3) = mpsc::unbounded_channel();
    let (_approval_sender3, approvals) = mpsc::unbounded_channel();
    let (_cancel_sender3, cancel) = watch::channel(false);
    let mut bounded = config(Some("Read the fixture within one inference call."));
    bounded.max_steps = 1;
    let model = Arc::new(ScriptModel::new(vec![tool(
        "read_file",
        json!({"path":"limit.txt"}),
    )]));
    let snapshot = third
        .run(bounded, model.clone(), events, approvals, cancel)
        .await
        .unwrap();
    assert_eq!(snapshot.status, SessionStatus::LimitReached);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    assert!(
        snapshot
            .messages
            .iter()
            .any(|message| message.role == AgentRole::Tool
                && message.content.contains("One permitted read"))
    );
}
