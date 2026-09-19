//! Offline SDK/FS benchmark. Build with `cargo build --release -p kurama-cli
//! --example control_plane_bench`, then run the example binary with `--help`.
//! Delayed streams use a loopback-only SSE fixture through the production adapter
//! because the CLI does not directly depend on a Stream implementation crate.
use std::{
    collections::BTreeMap,
    error::Error,
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use kurama_adapters::{FsSessionStore, HttpClient, OpenAiCompatBackend, WriteTool};
use kurama_core::{
    engine::{EngineHandle, RuntimeEvents},
    orchestrator::SmartOrchestrator,
    sink::NoopSink,
    testing::{AllowAllPolicy, ScriptedBackend, SequenceIds},
};
use kurama_sdk::{
    AgentBudget, AgentBuilder, AgentId, AgentRuntime, AgentSpec, BackendCapabilities, BoxFuture,
    CancelSignal, DelegationRequest, EventEnvelope, ExecutionMode, FinishReason, KuramaError,
    ModelBackend, ModelEvent, ModelItem, ModelProfile, ModelRequest, ModelStream,
    OrchestrationContext, RuntimeEvent, SessionEvent, SessionMetadata, SessionStore, WriteScope,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

type Result<T, E = Box<dyn Error + Send + Sync>> = std::result::Result<T, E>;
const TURN_TIMEOUT: Duration = Duration::from_secs(20);
const CHILD_CHUNK: usize = 4096;

#[derive(Clone)]
struct Options {
    mode: String,
    repetitions: usize,
    warmups: usize,
    histories: Vec<usize>,
    rounds: usize,
    children: usize,
    child_rounds: usize,
    timeout_seconds: u64,
}

impl Options {
    fn parse() -> Result<Option<Self>> {
        let mut options = Self {
            mode: "all".into(),
            repetitions: 5,
            warmups: 1,
            histories: vec![100, 1000, 5000],
            rounds: 16,
            children: 2,
            child_rounds: 3,
            timeout_seconds: 180,
        };
        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            if flag == "--help" {
                println!(
                    "control_plane_bench [--mode all|live|stream|children|tools] [--repetitions 5] [--warmups 1] [--histories 100,1000,5000] [--rounds 16] [--children 2] [--child-rounds 3] [--timeout-seconds 180]\nJSON on stdout; nonzero exit for correctness failures or timeout. Use a release binary. No network access except a credential-free 127.0.0.1 SSE fixture."
                );
                return Ok(None);
            }
            let value = args
                .next()
                .ok_or_else(|| io::Error::other(format!("missing value for {flag}")))?;
            match flag.as_str() {
                "--mode" => options.mode = value,
                "--repetitions" => options.repetitions = value.parse()?,
                "--warmups" => options.warmups = value.parse()?,
                "--histories" => {
                    options.histories = value
                        .split(',')
                        .map(str::parse)
                        .collect::<std::result::Result<_, _>>()?
                }
                "--rounds" => options.rounds = value.parse()?,
                "--children" => options.children = value.parse()?,
                "--child-rounds" => options.child_rounds = value.parse()?,
                "--timeout-seconds" => options.timeout_seconds = value.parse()?,
                _ => return Err(io::Error::other(format!("unknown argument {flag}")).into()),
            }
        }
        ensure(
            ["all", "live", "stream", "children", "tools"].contains(&options.mode.as_str()),
            "invalid mode",
        )?;
        ensure(
            options.repetitions > 0 && options.repetitions <= 1000,
            "repetitions must be 1..=1000",
        )?;
        ensure(options.warmups <= 100, "warmups must be <=100")?;
        ensure(
            (1..=8).contains(&options.children),
            "children must be 1..=8",
        )?;
        ensure(
            (1..=128).contains(&options.rounds) && (1..=16).contains(&options.child_rounds),
            "rounds must be 1..=128; child-rounds 1..=16",
        )?;
        ensure(options.timeout_seconds > 0, "timeout must be positive")?;
        ensure(
            !options.histories.is_empty() && options.histories.iter().all(|n| *n <= 100_000),
            "history sizes must be <=100000",
        )?;
        Ok(Some(options))
    }
}

fn ensure(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message.into()).into())
    }
}

fn text(value: impl Into<String>) -> ModelEvent {
    ModelEvent::TextDelta { text: value.into() }
}
fn completed(tool: bool) -> ModelEvent {
    ModelEvent::ResponseCompleted {
        cursor: None,
        finish_reason: if tool {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        },
    }
}
fn write_call(path: String, content: String, call: String) -> ModelEvent {
    ModelEvent::ToolCall {
        call_id: call.into(),
        name: "write".into(),
        arguments: json!({"path": path, "content": content}),
    }
}
fn child_prefix(round: usize) -> String {
    format!("{}round-{round}\n", "x".repeat(CHILD_CHUNK))
}
fn child_summary(child: usize, rounds: usize) -> String {
    let mut result = String::new();
    for round in 0..rounds {
        result.push_str(&child_prefix(round));
    }
    result.push_str(&format!("CHILD_OK:{child}"));
    result
}

struct Backend {
    mode: String,
    options: Options,
    calls: Mutex<BTreeMap<String, usize>>,
    first_delta: Arc<Mutex<Option<Instant>>>,
    servers: Mutex<Vec<JoinHandle<std::result::Result<(), String>>>>,
    http: HttpClient,
}

impl Backend {
    fn new(mode: &str, options: &Options) -> Result<Self> {
        Ok(Self {
            mode: mode.into(),
            options: options.clone(),
            calls: Mutex::new(BTreeMap::new()),
            first_delta: Arc::new(Mutex::new(None)),
            servers: Mutex::new(Vec::new()),
            http: HttpClient::with_timeout(TURN_TIMEOUT)?,
        })
    }

    fn plan(
        &self,
        request: &ModelRequest,
    ) -> std::result::Result<(Vec<ModelEvent>, u64), KuramaError> {
        let user = request
            .items
            .iter()
            .rev()
            .find_map(|item| match item {
                ModelItem::User { text } => Some(text.as_str()),
                _ => None,
            })
            .ok_or_else(|| {
                KuramaError::Protocol("benchmark request lost latest user message".into())
            })?;
        if self.mode == "live" {
            return Ok((vec![text(format!("ACK:{user}")), completed(false)], 0));
        }
        if self.mode == "stream" {
            return Ok((
                vec![
                    text("one "),
                    text("two "),
                    text("three "),
                    text("four"),
                    completed(false),
                ],
                60,
            ));
        }
        let key = request
            .agent_id
            .as_ref()
            .map_or_else(|| user.to_string(), ToString::to_string);
        let round = {
            let mut calls = self.calls.lock().expect("backend calls lock");
            let next = calls.entry(key.clone()).or_default();
            let round = *next;
            *next += 1;
            round
        };
        let tool_results = request
            .items
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    ModelItem::ToolResult {
                        is_error: false,
                        ..
                    }
                )
            })
            .count();
        if self.mode == "tools" {
            if round < self.options.rounds {
                return Ok((
                    vec![
                        write_call(
                            format!("round-{round}.txt"),
                            format!("durable round {round}\n"),
                            format!("write-{round}"),
                        ),
                        completed(true),
                    ],
                    0,
                ));
            }
            return Ok((
                vec![
                    text(if tool_results == self.options.rounds {
                        format!("TOOLS_OK:{}", self.options.rounds)
                    } else {
                        format!("TOOLS_INCOMPLETE:{tool_results}")
                    }),
                    completed(false),
                ],
                0,
            ));
        }
        if request.agent_id.is_none() {
            if round == 0 {
                if request.delegation.is_none() {
                    return Err(KuramaError::Protocol(
                        "explicit delegation gate is not enabled".into(),
                    ));
                }
                let agents = (0..self.options.children)
                    .map(|child| AgentSpec {
                        role: "implementer".into(),
                        objective: format!("bench child {child}"),
                        profile: None,
                        context_refs: Vec::new(),
                        depends_on: Vec::new(),
                        write_scope: WriteScope {
                            roots: vec![
                                PathBuf::from(&request.workspace_root)
                                    .join(format!("child-{child}")),
                            ],
                            files: Vec::new(),
                        },
                        budget: AgentBudget {
                            max_input_tokens: 80_000,
                            max_output_tokens: 8_000,
                            max_turns: 4,
                            max_seconds: 15,
                        },
                    })
                    .collect();
                return Ok((
                    vec![
                        ModelEvent::Delegation {
                            request: DelegationRequest { agents },
                        },
                        completed(true),
                    ],
                    0,
                ));
            }
            let results = request
                .items
                .iter()
                .filter_map(|item| match item {
                    ModelItem::AgentResult { summary, .. } => Some(summary),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let valid = results.len() == self.options.children
                && (0..self.options.children).all(|child| {
                    results
                        .iter()
                        .any(|summary| **summary == child_summary(child, self.options.child_rounds))
                });
            return Ok((
                vec![
                    text(if valid {
                        "PARENT_OK"
                    } else {
                        "PARENT_INCOMPLETE"
                    }),
                    completed(false),
                ],
                0,
            ));
        }
        if request.delegation.is_some() {
            return Err(KuramaError::Protocol("depth-one child can delegate".into()));
        }
        let child = user
            .lines()
            .find_map(|line| line.strip_prefix("Objective: bench child "))
            .and_then(|n| n.parse::<usize>().ok())
            .ok_or_else(|| KuramaError::Protocol("child objective missing".into()))?;
        if round < self.options.child_rounds {
            return Ok((
                vec![
                    text("x".repeat(CHILD_CHUNK)),
                    text(format!("round-{round}\n")),
                    write_call(
                        format!("child-{child}/round-{round}.txt"),
                        format!("child {child} round {round}\n"),
                        format!("{key}-write-{round}"),
                    ),
                    completed(true),
                ],
                5,
            ));
        }
        Ok((
            vec![
                text(if tool_results == self.options.child_rounds {
                    format!("CHILD_OK:{child}")
                } else {
                    format!("CHILD_INCOMPLETE:{child}:{tool_results}")
                }),
                completed(false),
            ],
            5,
        ))
    }

    async fn finish_servers(&self) -> Vec<String> {
        let handles = std::mem::take(&mut *self.servers.lock().expect("server handles lock"));
        let mut errors = Vec::new();
        for mut handle in handles {
            match tokio::time::timeout(Duration::from_secs(2), &mut handle).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => errors.push(error),
                Ok(Err(error)) => errors.push(format!("fixture task: {error}")),
                Err(_) => {
                    handle.abort();
                    errors.push("fixture task shutdown timeout".into());
                }
            }
        }
        errors
    }
}

impl ModelBackend for Backend {
    fn backend_name(&self) -> &'static str {
        "control-plane-bench"
    }
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::remote_default()
    }
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, std::result::Result<ModelStream, KuramaError>> {
        Box::pin(async move {
            let (events, delay_ms) = self.plan(&request)?;
            if delay_ms == 0 {
                if events
                    .iter()
                    .any(|event| matches!(event, ModelEvent::TextDelta { .. }))
                {
                    self.first_delta
                        .lock()
                        .expect("first delta lock")
                        .get_or_insert_with(Instant::now);
                }
                return ScriptedBackend::new(vec![events.into_iter().map(Ok).collect()])
                    .stream(request, cancel)
                    .await;
            }
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let endpoint = format!("http://{}/v1/", listener.local_addr()?);
            let first_delta = self.first_delta.clone();
            let server = tokio::spawn(async move {
                match tokio::time::timeout(
                    TURN_TIMEOUT,
                    serve_sse(listener, events, delay_ms, first_delta),
                )
                .await
                {
                    Ok(result) => result.map_err(|error| format!("loopback fixture: {error}")),
                    Err(_) => Err("loopback fixture timeout".into()),
                }
            });
            self.servers
                .lock()
                .expect("server handles lock")
                .push(server);
            OpenAiCompatBackend::from_endpoint(self.http.clone(), &endpoint, None)?
                .stream(request, cancel)
                .await
        })
    }
}

async fn serve_sse(
    listener: TcpListener,
    events: Vec<ModelEvent>,
    delay_ms: u64,
    first_delta: Arc<Mutex<Option<Instant>>>,
) -> Result<()> {
    let (mut socket, peer) = listener.accept().await?;
    ensure(peer.ip().is_loopback(), "non-loopback fixture peer")?;
    socket.set_nodelay(true)?;
    let mut request = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let bytes = socket.read(&mut buffer).await?;
        ensure(bytes != 0, "fixture request ended early")?;
        request.extend_from_slice(&buffer[..bytes]);
        ensure(
            request.len() <= 16 * 1024 * 1024,
            "fixture request too large",
        )?;
        if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&request[..header_end])?;
            let length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim())
                })
                .ok_or_else(|| io::Error::other("missing fixture Content-Length"))?
                .parse::<usize>()?;
            if request.len() >= header_end + 4 + length {
                break;
            }
        }
    }
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        )
        .await?;
    for event in events {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let payload = match event {
            ModelEvent::TextDelta { text } => {
                first_delta
                    .lock()
                    .expect("first delta lock")
                    .get_or_insert_with(Instant::now);
                json!({"id":"offline", "choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]})
            }
            ModelEvent::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":call_id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":null}]})
            }
            ModelEvent::ResponseCompleted { finish_reason, .. } => {
                json!({"choices":[{"index":0,"delta":{},"finish_reason": if finish_reason == FinishReason::ToolCalls {"tool_calls"} else {"stop"}}]})
            }
            _ => return Err(io::Error::other("unsupported fixture event").into()),
        };
        socket
            .write_all(format!("data: {payload}\n\n").as_bytes())
            .await?;
    }
    socket.write_all(b"data: [DONE]\n\n").await?;
    socket.shutdown().await?;
    Ok(())
}

struct Fixture {
    _temp: TempDir,
    workspace: PathBuf,
    store: Arc<FsSessionStore>,
    session: SessionMetadata,
    backend: Arc<Backend>,
    runtime: AgentRuntime,
}

impl Fixture {
    fn new(mode: &str, options: &Options) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let workspace = workspace.canonicalize()?;
        for child in 0..options.children {
            std::fs::create_dir(workspace.join(format!("child-{child}")))?;
        }
        let store = Arc::new(FsSessionStore::open(temp.path().join("store"))?);
        let backend = Arc::new(Backend::new(mode, options)?);
        let profile = ModelProfile::new("bench", "offline", 128_000, 8_000);
        let ids = Arc::new(SequenceIds::default());
        let scope = WriteScope {
            roots: vec![workspace.clone()],
            files: Vec::new(),
        };
        let mut builder = AgentBuilder::new()
            .profile(profile.clone(), backend.clone())
            .tool(Arc::new(WriteTool::default()))
            .policy(Arc::new(AllowAllPolicy))
            .store(store.clone())
            .sink(Arc::new(NoopSink))
            .orchestrator(Arc::new(SmartOrchestrator::new(ids.clone())))
            .ids(ids)
            .write_scope(scope.clone())
            .provider_retry_delays_ms(Vec::new());
        if mode == "children" {
            builder = builder.orchestration_context(OrchestrationContext {
                parent_profile: profile.clone(),
                profiles: BTreeMap::from([(profile.name.clone(), profile)]),
                role_routes: BTreeMap::new(),
                role_escalations: BTreeMap::new(),
                profile_escalations: BTreeMap::new(),
                parent_write_scope: scope,
                max_concurrency: options.children,
                depth: 0,
                yolo: false,
            });
        }
        let runtime = builder.build()?;
        let session = SessionMetadata {
            id: "bench-session".into(),
            created_at_ms: 0,
            project_root: workspace.display().to_string(),
            profile: "bench".into(),
            mode: ExecutionMode::Supervised,
            redaction_best_effort: false,
        };
        Ok(Self {
            _temp: temp,
            workspace,
            store,
            session,
            backend,
            runtime,
        })
    }

    fn seed(&self, turns: usize) -> Result<Vec<EventEnvelope>> {
        self.store.create(&self.session)?;
        let mut sequence = 0;
        let mut append = |event| -> Result<()> {
            self.store.append(&EventEnvelope::new(
                sequence,
                sequence,
                self.session.id.clone(),
                None,
                event,
            ))?;
            sequence += 1;
            Ok(())
        };
        append(SessionEvent::SessionStarted {
            metadata: self.session.clone(),
        })?;
        for turn in 0..turns {
            append(serde_json::from_value(json!({
                "type": "user_message",
                "text": format!("seed-{turn}: deterministic context for completed work"),
                "explicit_delegation": false,
            }))?)?;
            append(SessionEvent::AssistantMessage {
                text: format!("seed-answer-{turn}: completed deterministic work"),
            })?;
            append(SessionEvent::TurnCompleted)?;
        }
        Ok(self.store.replay(&self.session.id)?)
    }
}

async fn run_turn(
    fixture: &Fixture,
    handle: &EngineHandle,
    events: &mut RuntimeEvents,
    prompt: &str,
    explicit: bool,
) -> Value {
    *fixture
        .backend
        .first_delta
        .lock()
        .expect("first delta lock") = None;
    let start = Instant::now();
    let mut first_visible = None;
    let mut output = String::new();
    let mut errors = Vec::<String>::new();
    let mut deltas = 0;
    let mut updates = 0;
    let mut tool_completions = 0;
    let mut child_snapshots = BTreeMap::new();
    let turn = tokio::time::timeout(TURN_TIMEOUT, async {
        handle.submit(prompt, explicit).await?;
        loop {
            match events.recv().await.ok_or(KuramaError::Cancelled)? {
                RuntimeEvent::AssistantDelta { text } => {
                    first_visible.get_or_insert_with(Instant::now);
                    deltas += 1;
                    output.push_str(&text);
                }
                RuntimeEvent::AgentUpdated { snapshot } => {
                    updates += 1;
                    child_snapshots.insert(snapshot.id.clone(), snapshot);
                }
                RuntimeEvent::ToolCompleted { result, .. } => {
                    tool_completions += 1;
                    if result.is_error {
                        errors.push(format!("tool result: {}", result.output));
                    }
                }
                RuntimeEvent::ApprovalRequired { .. } => {
                    return Err(KuramaError::Protocol(
                        "unexpected approval in deterministic benchmark".into(),
                    ));
                }
                RuntimeEvent::Error { message } => {
                    errors.push(message);
                    break;
                }
                RuntimeEvent::TurnCompleted => return Ok(true),
                RuntimeEvent::Shutdown => return Err(KuramaError::Cancelled),
                _ => {}
            }
        }
        Ok(false)
    })
    .await;
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    let completed = match turn {
        Ok(Ok(completed)) => completed,
        Ok(Err(error)) => {
            errors.push(error.to_string());
            false
        }
        Err(_) => {
            errors.push("turn timeout".into());
            false
        }
    };
    let provider = *fixture
        .backend
        .first_delta
        .lock()
        .expect("first delta lock");
    json!({"elapsed_ms":elapsed, "turn_completed":completed, "output":output,
        "errors":errors, "assistant_deltas":deltas, "agent_updates":updates, "tool_completions":tool_completions,
        "last_observed_child_snapshots":child_snapshots.into_values().collect::<Vec<_>>(),
        "provider_first_delta_ms":provider.map(|time| time.duration_since(start).as_secs_f64()*1000.0),
        "first_visible_delta_ms":first_visible.map(|time| time.duration_since(start).as_secs_f64()*1000.0),
        "first_visible_lag_ms":provider.zip(first_visible).map(|(provider, visible)| visible.saturating_duration_since(provider).as_secs_f64()*1000.0)})
}

fn sample_error(sample: &mut Value, message: impl Into<String>) {
    sample["errors"]
        .as_array_mut()
        .expect("sample errors")
        .push(json!(message.into()));
}

fn contiguous(events: &[EventEnvelope], agent: Option<&AgentId>) -> Result<()> {
    ensure(!events.is_empty(), "missing durable log")?;
    for (sequence, event) in events.iter().enumerate() {
        ensure(
            event.sequence == sequence as u64 && event.agent_id.as_ref() == agent,
            format!("noncontiguous or wrong-agent log at sequence {sequence}"),
        )?;
    }
    Ok(())
}

fn validate(
    fixture: &Fixture,
    sample: &mut Value,
    expected: &str,
    options: &Options,
) -> Result<()> {
    if sample["output"] != expected {
        sample_error(
            sample,
            format!("final output mismatch: expected {expected}"),
        );
    }
    if sample["turn_completed"] != true {
        sample_error(sample, "missing TurnCompleted");
    }
    let parent = fixture.store.replay(&fixture.session.id)?;
    contiguous(&parent, None)?;
    sample["parent_log_events"] = json!(parent.len());
    if !matches!(
        parent.last().map(|event| &event.event),
        Some(SessionEvent::TurnCompleted)
    ) {
        sample_error(sample, "parent log missing terminal TurnCompleted");
    }
    let last_text = parent.iter().rev().find_map(|event| match &event.event {
        SessionEvent::AssistantMessage { text } => Some(text.as_str()),
        _ => None,
    });
    if last_text != Some(expected) {
        sample_error(sample, "durable final assistant message mismatch");
    }
    if fixture.backend.mode == "tools" {
        let durable_events = parent.len().saturating_sub(1);
        sample["timed_durable_events"] = json!(durable_events);
        sample["loop_ms_per_durable_event"] = sample["elapsed_ms"]
            .as_f64()
            .filter(|_| durable_events > 0)
            .map_or(Value::Null, |elapsed| {
                json!(elapsed / durable_events as f64)
            });
        let completed = parent.iter().filter(|event| matches!(&event.event, SessionEvent::ToolCompleted { result, .. } if !result.is_error)).count();
        sample["durable_tool_completions"] = json!(completed);
        if completed != options.rounds {
            sample_error(
                sample,
                format!(
                    "expected {} durable tool completions, got {completed}",
                    options.rounds
                ),
            );
        }
        for round in 0..options.rounds {
            match std::fs::read_to_string(fixture.workspace.join(format!("round-{round}.txt"))) {
                Ok(content) if content == format!("durable round {round}\n") => {}
                result => sample_error(
                    sample,
                    format!("round {round} artifact mismatch: {result:?}"),
                ),
            }
        }
    }
    if fixture.backend.mode == "children" {
        let mut children = BTreeMap::new();
        let mut completed = 0;
        let mut failed = 0;
        let mut cancelled = 0;
        for event in &parent {
            match &event.event {
                SessionEvent::AgentQueued { snapshot } => {
                    children.insert(snapshot.id.clone(), snapshot.objective.clone());
                }
                SessionEvent::AgentCompleted { snapshot, summary } => {
                    completed += 1;
                    let child = snapshot
                        .objective
                        .strip_prefix("bench child ")
                        .and_then(|n| n.parse::<usize>().ok());
                    if child
                        .is_none_or(|child| *summary != child_summary(child, options.child_rounds))
                    {
                        sample_error(sample, format!("child {} summary mismatch", snapshot.id));
                    }
                    if snapshot.changed_files.len() != options.child_rounds {
                        sample_error(
                            sample,
                            format!("child {} changed-file evidence mismatch", snapshot.id),
                        );
                    }
                }
                SessionEvent::AgentFailed { snapshot, error } => {
                    failed += 1;
                    sample_error(sample, format!("child {} failed: {error}", snapshot.id));
                }
                SessionEvent::AgentCancelled { snapshot } => {
                    cancelled += 1;
                    sample_error(
                        sample,
                        format!("child {} cancelled: {:?}", snapshot.id, snapshot.last_error),
                    );
                }
                _ => {}
            }
        }
        let mut logs = Vec::new();
        for (agent, objective) in &children {
            match fixture.store.replay_agent(&fixture.session.id, agent) {
                Ok(log) => {
                    let valid = contiguous(&log, Some(agent));
                    if let Err(error) = &valid {
                        sample_error(sample, format!("child {agent} log: {error}"));
                    }
                    let completed_turns = log
                        .iter()
                        .filter(|event| matches!(event.event, SessionEvent::TurnCompleted))
                        .count();
                    let tool_completions = log.iter().filter(|event| matches!(&event.event, SessionEvent::ToolCompleted { result, .. } if !result.is_error)).count();
                    let child_errors = log
                        .iter()
                        .filter_map(|event| match &event.event {
                            SessionEvent::TurnFailed { error }
                            | SessionEvent::AgentFailed { error, .. } => Some(error.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    for error in &child_errors {
                        sample_error(sample, format!("child {agent} durable failure: {error}"));
                    }
                    if completed_turns != 1 || tool_completions != options.child_rounds {
                        sample_error(
                            sample,
                            format!(
                                "child {agent} incomplete durable work: {completed_turns} turns, {tool_completions} tools"
                            ),
                        );
                    }
                    logs.push(json!({"agent":agent,"objective":objective,"events":log.len(),"contiguous":valid.is_ok(),"progress_events":log.iter().filter(|event| matches!(event.event, SessionEvent::AgentProgress { .. })).count(),"completed_turns":completed_turns,"tool_completions":tool_completions,"errors":child_errors}));
                }
                Err(error) => {
                    sample_error(sample, format!("child {agent} replay: {error}"));
                    logs.push(json!({"agent":agent,"contiguous":false,"error":error.to_string()}));
                }
            }
        }
        sample["children"] = json!({"queued":children.len(),"completed":completed,"failed":failed,"cancelled":cancelled,"logs":logs});
        if completed != options.children || children.len() != options.children {
            sample_error(
                sample,
                format!(
                    "expected {} successful children, got {completed}",
                    options.children
                ),
            );
        }
        for child in 0..options.children {
            for round in 0..options.child_rounds {
                match std::fs::read_to_string(
                    fixture
                        .workspace
                        .join(format!("child-{child}/round-{round}.txt")),
                ) {
                    Ok(content) if content == format!("child {child} round {round}\n") => {}
                    result => sample_error(
                        sample,
                        format!("child {child} round {round} artifact mismatch: {result:?}"),
                    ),
                }
            }
        }
    }
    Ok(())
}

async fn stop(handle: &EngineHandle, events: &mut RuntimeEvents) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        handle.shutdown().await?;
        while let Some(event) = events.recv().await {
            match event {
                RuntimeEvent::Shutdown => return Ok(()),
                RuntimeEvent::Error { message } => return Err(io::Error::other(message).into()),
                _ => {}
            }
        }
        Err(io::Error::other("runtime closed before Shutdown").into())
    })
    .await?
}

fn distribution(values: impl Iterator<Item = f64>) -> Value {
    let mut values = values.collect::<Vec<_>>();
    values.sort_by(f64::total_cmp);
    if values.is_empty() {
        return Value::Null;
    }
    let middle = values.len() / 2;
    let median = if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    };
    let p95 = values[(values.len() * 95).div_ceil(100) - 1];
    json!({"median":median,"p95":p95,"min":values[0],"max":values[values.len()-1],"count":values.len()})
}

fn report(name: &str, params: Value, samples: Vec<Value>) -> Value {
    let measured = samples
        .iter()
        .filter(|sample| sample["warmup"] == false)
        .collect::<Vec<_>>();
    let successful = measured
        .iter()
        .filter(|sample| {
            sample["errors"].as_array().is_some_and(Vec::is_empty)
                && sample["turn_completed"] == true
        })
        .collect::<Vec<_>>();
    let mut stats = serde_json::Map::new();
    for metric in [
        "elapsed_ms",
        "first_visible_delta_ms",
        "provider_first_delta_ms",
        "first_visible_lag_ms",
        "loop_ms_per_durable_event",
    ] {
        stats.insert(
            metric.into(),
            distribution(
                successful
                    .iter()
                    .filter_map(|sample| sample[metric].as_f64()),
            ),
        );
    }
    let failures = samples
        .iter()
        .filter(|sample| {
            sample["errors"]
                .as_array()
                .is_none_or(|errors| !errors.is_empty())
        })
        .count();
    json!({"scenario":name,"parameters":params,"measured_samples":measured.len(),"successful_samples":successful.len(),"failed_samples_including_warmups":failures,
        "success_only_statistics":stats,"all_elapsed_ms":distribution(measured.iter().filter_map(|sample| sample["elapsed_ms"].as_f64())),"samples":samples})
}

async fn live(options: &Options, history: usize) -> Result<Value> {
    let fixture = Fixture::new("live", options)?;
    let replay = fixture.seed(history)?;
    let (handle, mut events) = fixture.runtime.start(fixture.session.clone(), replay)?;
    let mut samples = Vec::new();
    // At least one untimed live turn is mandatory: runtime startup/recovery is never measured.
    let warmups = options.warmups.max(1);
    for index in 0..warmups + options.repetitions {
        let prompt = format!("live-{index}");
        let mut sample = run_turn(&fixture, &handle, &mut events, &prompt, false).await;
        sample["warmup"] = json!(index < warmups);
        sample["history_turns_before_submission"] = json!(history + index);
        if let Err(error) = validate(&fixture, &mut sample, &format!("ACK:{prompt}"), options) {
            sample_error(&mut sample, error.to_string());
        }
        let completed = sample["turn_completed"] == true;
        samples.push(sample);
        if !completed {
            break;
        }
    }
    if let Err(error) = stop(&handle, &mut events).await {
        sample_error(
            samples.last_mut().expect("at least one sample"),
            format!("shutdown: {error}"),
        );
    }
    Ok(report(
        "warmed_live_turn",
        json!({"seeded_completed_turns":history,"seeded_events":1+3*history,"warmups":warmups,"repetitions":options.repetitions,"input_budget_tokens":128_000,"seed_start_replay_excluded":true}),
        samples,
    ))
}

async fn independent(options: &Options, mode: &str) -> Result<Value> {
    let mut samples = Vec::new();
    for index in 0..options.warmups + options.repetitions {
        let fixture = Fixture::new(mode, options)?;
        let (handle, mut events) = fixture.runtime.start(fixture.session.clone(), Vec::new())?;
        let prompt = if mode == "children" {
            "use sub-agents for the deterministic benchmark"
        } else {
            "run the deterministic benchmark"
        };
        let mut sample = run_turn(&fixture, &handle, &mut events, prompt, mode == "children").await;
        sample["warmup"] = json!(index < options.warmups);
        let expected = match mode {
            "stream" => "one two three four".into(),
            "children" => "PARENT_OK".into(),
            _ => format!("TOOLS_OK:{}", options.rounds),
        };
        if let Err(error) = validate(&fixture, &mut sample, &expected, options) {
            sample_error(&mut sample, error.to_string());
        }
        if let Err(error) = stop(&handle, &mut events).await {
            sample_error(&mut sample, format!("shutdown: {error}"));
        }
        for error in fixture.backend.finish_servers().await {
            sample_error(&mut sample, error);
        }
        samples.push(sample);
    }
    let (name, parameters) = match mode {
        "stream" => (
            "short_slow_stream",
            json!({"chunks":4,"response_bytes":"one two three four".len(),"frame_delay_ms":60,"provider_clock":"loopback SSE write, includes local transport/normalizer lag"}),
        ),
        "children" => (
            "real_child_orchestration",
            json!({"children":options.children,"max_concurrency":options.children,"rounds_per_child":options.child_rounds,"text_chunk_bytes":CHILD_CHUNK,"frame_delay_ms":5,"tool":"production WriteTool","runner":"production RuntimeChildRunner","orchestrator":"production SmartOrchestrator"}),
        ),
        _ => (
            "durable_tool_loop",
            json!({"rounds":options.rounds,"tool":"production WriteTool","unique_artifacts":options.rounds,"timing_includes":"SDK model/tool loop, durable appends, checkpoints and actual atomic writes"}),
        ),
    };
    Ok(report(name, parameters, samples))
}

async fn run(options: &Options) -> Value {
    let mut scenarios = Vec::new();
    if options.mode == "all" || options.mode == "live" {
        for history in &options.histories {
            scenarios.push(match live(options, *history).await {
                Ok(report) => report,
                Err(error) => json!({"scenario":"warmed_live_turn","seeded_completed_turns":history,"fatal_error":error.to_string()}),
            });
        }
    }
    for mode in ["stream", "children", "tools"] {
        if options.mode == "all" || options.mode == mode {
            scenarios.push(match independent(options, mode).await {
                Ok(report) => report,
                Err(error) => json!({"scenario":mode,"fatal_error":error.to_string()}),
            });
        }
    }
    let ok = scenarios.iter().all(|scenario| {
        scenario.get("fatal_error").is_none()
            && scenario["failed_samples_including_warmups"] == 0
            && scenario["successful_samples"] == options.repetitions
    });
    json!({"schema_version":1,"benchmark":"kurama-control-plane","ok":ok,"release_build":!cfg!(debug_assertions),
        "version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"arch":std::env::consts::ARCH,
        "parameters":{"mode":options.mode,"repetitions":options.repetitions,"warmups":options.warmups,"histories":options.histories,"rounds":options.rounds,"children":options.children,"child_rounds":options.child_rounds,"process_timeout_seconds":options.timeout_seconds,"turn_timeout_seconds":TURN_TIMEOUT.as_secs()},
        "measurement":"monotonic wall clock from submit through observed TurnCompleted; setup, seed, replay and validation excluded; failures retained, successful latency statistics separate",
        "scenarios":scenarios})
}

fn main() {
    let options = match Options::parse() {
        Ok(Some(options)) => options,
        Ok(None) => return,
        Err(error) => {
            println!("{}", json!({"ok":false,"fatal_error":error.to_string()}));
            std::process::exit(2);
        }
    };
    let deadline = options.timeout_seconds;
    // Separate OS thread also bounds synchronous fsync/replay stalls that cannot
    // be interrupted by a Tokio timeout on the current-thread runtime.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(deadline));
        println!(
            "{}",
            json!({"ok":false,"fatal_error":"process watchdog timeout","timeout_seconds":deadline})
        );
        std::process::exit(124);
    });
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            println!("{}", json!({"ok":false,"fatal_error":error.to_string()}));
            std::process::exit(2);
        }
    };
    let result = runtime.block_on(run(&options));
    println!("{result}");
    std::process::exit(if result["ok"] == true { 0 } else { 1 });
}
