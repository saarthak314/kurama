use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    sync::{Arc, Mutex},
};

use kurama_core::{
    agent_manager::{AgentManager, ChildApproval, ChildProgress, ChildRunContext, ChildRunner},
    orchestrator::SmartOrchestrator,
    testing::{CollectingSink, MemoryStore, SequenceIds},
};
use kurama_protocol::{
    KuramaError,
    agent::{
        AgentBudget, AgentResult, AgentSpec, AgentState, DelegationRequest, OrchestrationContext,
        ResolvedAgentSpec, SchedulePlan, WriteScope,
    },
    id::{AgentId, SessionId},
    model::ModelProfile,
    policy::{ApprovalRequest, ApprovalResponse},
    runtime::RuntimeEvent,
    session::SessionEvent,
    tool::Operation,
    traits::{BoxFuture, EventSink, Orchestrator, SessionStore},
};
use tokio::{
    sync::{Barrier, Notify, mpsc, oneshot},
    time::Duration,
};

fn agent(role: &str, profile: Option<&str>, depends_on: &[&str]) -> AgentSpec {
    AgentSpec {
        role: role.into(),
        objective: role.into(),
        profile: profile.map(str::to_owned),
        context_refs: Vec::new(),
        write_scope: WriteScope::default(),
        budget: AgentBudget::default(),
        depends_on: depends_on.iter().map(|value| (*value).into()).collect(),
    }
}

fn context() -> OrchestrationContext {
    let profiles = [
        ("parent", ModelProfile::new("parent", "p", 100_000, 10_000)),
        ("review", ModelProfile::new("review", "r", 100_000, 10_000)),
        ("user", ModelProfile::new("user", "u", 100_000, 10_000)),
    ]
    .into_iter()
    .map(|(name, profile)| (name.into(), profile))
    .collect();
    OrchestrationContext {
        parent_profile: ModelProfile::new("parent", "p", 100_000, 10_000),
        profiles,
        role_routes: BTreeMap::from([("reviewer".into(), "review".into())]),
        role_escalations: BTreeMap::new(),
        profile_escalations: BTreeMap::new(),
        parent_write_scope: WriteScope {
            roots: vec![PathBuf::from(".")],
            files: Vec::new(),
        },
        max_concurrency: 4,
        depth: 0,
        yolo: false,
    }
}

fn resolved_agent(id: &str, role: &str, objective: &str, depends_on: &[&str]) -> ResolvedAgentSpec {
    ResolvedAgentSpec {
        id: id.into(),
        parent_id: None,
        depth: 1,
        role: role.into(),
        objective: objective.into(),
        profile: ModelProfile::new("parent", "p", 100_000, 10_000),
        context_refs: Vec::new(),
        write_scope: WriteScope::default(),
        budget: AgentBudget::default(),
        depends_on: depends_on.iter().map(|value| (*value).into()).collect(),
        escalation_profiles: Vec::new(),
    }
}

fn cancelled_event_count(store: &MemoryStore, session_id: &str, agent_id: &AgentId) -> usize {
    let session_id: SessionId = session_id.into();
    store
        .replay_agent(&session_id, agent_id)
        .expect("replay agent")
        .into_iter()
        .filter(|event| matches!(event.event, SessionEvent::AgentCancelled { .. }))
        .count()
}

#[test]
fn explicit_delegation_gate_is_off_for_ordinary_turns() {
    let orchestrator = SmartOrchestrator::new(Arc::new(SequenceIds::new(1)));
    assert!(!orchestrator.explicit_delegation("fix the failing test"));
    assert!(orchestrator.explicit_delegation("use sub-agents to fix it"));
    assert!(orchestrator.explicit_delegation("parallelize this with a reviewer"));
}

#[test]
fn derives_roles_routes_profiles_and_queues_above_concurrency() {
    let orchestrator = SmartOrchestrator::new(Arc::new(SequenceIds::new(1)));
    let request = DelegationRequest {
        agents: vec![
            agent("custom", Some("missing"), &[]),
            agent("reviewer", None, &[]),
            agent("third", None, &[]),
            agent("fourth", None, &[]),
            agent("fifth", None, &[]),
        ],
    };
    let plan = orchestrator.resolve(request, &context()).expect("resolve");
    assert_eq!(plan.ready[0].role, "implementer");
    assert_eq!(plan.ready[0].profile.name, "parent");
    assert_eq!(plan.ready[1].role, "reviewer");
    assert_eq!(plan.ready[1].profile.name, "review");
    assert_eq!(plan.ready[2].profile.name, "parent");
    assert_eq!(plan.ready.len(), 4);
    assert_eq!(plan.queued.len(), 1);
}

#[test]
fn depth_one_children_cannot_delegate() {
    let orchestrator = SmartOrchestrator::new(Arc::new(SequenceIds::new(1)));
    let mut child_context = context();
    child_context.depth = 1;
    let error = orchestrator
        .resolve(
            DelegationRequest {
                agents: vec![agent("nested", None, &[])],
            },
            &child_context,
        )
        .expect_err("nested delegation denied");
    assert!(error.to_string().contains("depth"));
}

struct ReportingRunner;

impl ChildRunner for ReportingRunner {
    fn run(
        &self,
        context: ChildRunContext,
    ) -> BoxFuture<'static, Result<AgentResult, KuramaError>> {
        Box::pin(async move {
            context
                .progress
                .send(ChildProgress {
                    phase: Some("reviewing".into()),
                    transcript_line: Some("checked the target".into()),
                    ..ChildProgress::default()
                })
                .await
                .expect("progress");
            Ok(AgentResult {
                agent_id: context.agent_id,
                summary: "done".into(),
                changed_files: Vec::new(),
                evidence_refs: Vec::new(),
            })
        })
    }
}

#[tokio::test]
async fn manager_canonicalizes_unique_objective_dependencies() {
    let mut implementation = agent("ignored", None, &[]);
    implementation.objective = "implement api".into();
    let mut review = agent("ignored", None, &["implement api"]);
    review.objective = "review api".into();
    let plan = SmartOrchestrator::new(Arc::new(SequenceIds::new(2)))
        .resolve(
            DelegationRequest {
                agents: vec![implementation, review],
            },
            &context(),
        )
        .expect("resolve");
    let manager = AgentManager::new(
        "objective-dependency".into(),
        None,
        2,
        Arc::new(MemoryStore::default()),
        Arc::new(CollectingSink::default()),
    );

    let results = tokio::time::timeout(
        Duration::from_millis(250),
        manager.execute(plan, "project".into(), Arc::new(ReportingRunner)),
    )
    .await
    .expect("objective dependency must not deadlock")
    .expect("execute");

    assert_eq!(results.len(), 2);
}

#[tokio::test]
async fn manager_rejects_cyclic_resolved_dependencies() {
    let plan = SchedulePlan {
        ready: Vec::new(),
        queued: Vec::new(),
        blocked: vec![
            resolved_agent("agent-a", "first", "first task", &["second"]),
            resolved_agent("agent-b", "second", "second task", &["first"]),
        ],
    };
    let manager = AgentManager::new(
        "cyclic-dependency".into(),
        None,
        2,
        Arc::new(MemoryStore::default()),
        Arc::new(CollectingSink::default()),
    );

    let error = tokio::time::timeout(
        Duration::from_millis(250),
        manager.execute(plan, "project".into(), Arc::new(ReportingRunner)),
    )
    .await
    .expect("cyclic plan must not deadlock")
    .expect_err("cyclic plan must be rejected");

    assert!(error.to_string().contains("cycle"));
}

#[tokio::test]
async fn manager_runs_and_exposes_compact_child_inspection() {
    let orchestrator = SmartOrchestrator::new(Arc::new(SequenceIds::new(1)));
    let plan = orchestrator
        .resolve(
            DelegationRequest {
                agents: vec![agent("reviewer", None, &[])],
            },
            &context(),
        )
        .expect("resolve");
    let agent_id = plan.ready[0].id.clone();
    let store = Arc::new(MemoryStore::default());
    let manager = AgentManager::new(
        "session".into(),
        None,
        4,
        store.clone(),
        Arc::new(CollectingSink::default()),
    );
    let results = manager
        .execute(plan, "project".into(), Arc::new(ReportingRunner))
        .await
        .expect("execute");
    assert_eq!(results.len(), 1);
    let inspection = manager.inspect(&agent_id).await.expect("inspect");
    assert_eq!(inspection.snapshot.state, AgentState::Completed);
    assert_eq!(inspection.transcript, vec!["checked the target"]);
    let replay = store
        .replay_agent(&"session".into(), &agent_id)
        .expect("replay agent");
    let progress = replay
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                SessionEvent::AgentProgress { snapshot }
                    if snapshot.phase.as_deref() == Some("reviewing")
            )
        })
        .expect("durable child progress");
    let completed = replay
        .iter()
        .position(|event| matches!(event.event, SessionEvent::AgentCompleted { .. }))
        .expect("durable child completion");
    assert!(progress < completed);
}

struct ControlledRunner {
    started: mpsc::UnboundedSender<kurama_protocol::id::AgentId>,
}

struct CleanupRunner {
    started: Option<mpsc::UnboundedSender<kurama_protocol::id::AgentId>>,
    finished: Arc<AtomicUsize>,
    cleanup_delay: Duration,
}

impl ChildRunner for CleanupRunner {
    fn run(
        &self,
        context: ChildRunContext,
    ) -> BoxFuture<'static, Result<AgentResult, KuramaError>> {
        let started = self.started.clone();
        let finished = self.finished.clone();
        let cleanup_delay = self.cleanup_delay;
        Box::pin(async move {
            if let Some(started) = started {
                started.send(context.agent_id).expect("started");
            }
            context.cancel.cancelled().await;
            tokio::time::sleep(cleanup_delay).await;
            finished.fetch_add(1, Ordering::SeqCst);
            Err(KuramaError::Cancelled)
        })
    }
}

struct NonCooperativeRunner {
    started: mpsc::UnboundedSender<kurama_protocol::id::AgentId>,
    dropped: Arc<AtomicUsize>,
}

struct DropCounter(Arc<AtomicUsize>);

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl ChildRunner for NonCooperativeRunner {
    fn run(
        &self,
        context: ChildRunContext,
    ) -> BoxFuture<'static, Result<AgentResult, KuramaError>> {
        let started = self.started.clone();
        let dropped = self.dropped.clone();
        Box::pin(async move {
            started.send(context.agent_id).expect("started");
            let _drop_counter = DropCounter(dropped);
            std::future::pending::<Result<AgentResult, KuramaError>>().await
        })
    }
}

struct ErrorPathRunner {
    barrier: Arc<Barrier>,
    dropped: Arc<AtomicUsize>,
}

impl ChildRunner for ErrorPathRunner {
    fn run(
        &self,
        context: ChildRunContext,
    ) -> BoxFuture<'static, Result<AgentResult, KuramaError>> {
        let barrier = self.barrier.clone();
        let dropped = self.dropped.clone();
        Box::pin(async move {
            let _drop_counter = DropCounter(dropped);
            barrier.wait().await;
            if context.launch.brief.role == "first" {
                context
                    .progress
                    .send(ChildProgress {
                        phase: Some("sink-failure".into()),
                        ..ChildProgress::default()
                    })
                    .await
                    .expect("progress");
            }
            std::future::pending::<Result<AgentResult, KuramaError>>().await
        })
    }
}

struct FailingProgressSink;

impl EventSink for FailingProgressSink {
    fn emit(&self, event: RuntimeEvent) -> Result<(), KuramaError> {
        if matches!(
            event,
            RuntimeEvent::AgentUpdated { snapshot }
                if snapshot.phase.as_deref() == Some("sink-failure")
        ) {
            return Err(KuramaError::Protocol("sink rejected child progress".into()));
        }
        Ok(())
    }
}

#[tokio::test]
async fn manager_timeout_cancels_and_awaits_child_cleanup() {
    let mut spec = resolved_agent("timed-child", "worker", "slow task", &[]);
    spec.budget.max_seconds = 0;
    let plan = SchedulePlan {
        ready: vec![spec],
        queued: Vec::new(),
        blocked: Vec::new(),
    };
    let finished = Arc::new(AtomicUsize::new(0));
    let manager = AgentManager::new(
        "timeout-cleanup".into(),
        None,
        1,
        Arc::new(MemoryStore::default()),
        Arc::new(CollectingSink::default()),
    );

    manager
        .execute(
            plan,
            "project".into(),
            Arc::new(CleanupRunner {
                started: None,
                finished: finished.clone(),
                cleanup_delay: Duration::from_millis(20),
            }),
        )
        .await
        .expect("execute");

    assert_eq!(finished.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancel_all_aborts_non_cooperative_child_after_grace() {
    let plan = SchedulePlan {
        ready: vec![resolved_agent("stuck-child", "worker", "never stops", &[])],
        queued: Vec::new(),
        blocked: Vec::new(),
    };
    let manager = AgentManager::new(
        "non-cooperative-cancel".into(),
        None,
        1,
        Arc::new(MemoryStore::default()),
        Arc::new(CollectingSink::default()),
    );
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let dropped = Arc::new(AtomicUsize::new(0));
    let executing = manager.execute(
        plan,
        "project".into(),
        Arc::new(NonCooperativeRunner {
            started: started_tx,
            dropped: dropped.clone(),
        }),
    );
    tokio::pin!(executing);
    tokio::select! {
        result = &mut executing => panic!("execution ended before cancellation: {result:?}"),
        started = started_rx.recv() => started.expect("child started"),
    };

    tokio::time::timeout(Duration::from_millis(500), manager.cancel_all())
        .await
        .expect("cancel_all must abort a non-cooperative child");
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_millis(250), &mut executing)
        .await
        .expect("execution must stop")
        .expect("execute");
}

#[tokio::test]
async fn execute_error_cancels_and_awaits_live_children() {
    let plan = SchedulePlan {
        ready: vec![
            resolved_agent("error-child-a", "first", "first task", &[]),
            resolved_agent("error-child-b", "second", "second task", &[]),
        ],
        queued: Vec::new(),
        blocked: Vec::new(),
    };
    let manager = AgentManager::new(
        "error-cleanup".into(),
        None,
        2,
        Arc::new(MemoryStore::default()),
        Arc::new(FailingProgressSink),
    );
    let dropped = Arc::new(AtomicUsize::new(0));

    let error = tokio::time::timeout(
        Duration::from_millis(500),
        manager.execute(
            plan,
            "project".into(),
            Arc::new(ErrorPathRunner {
                barrier: Arc::new(Barrier::new(2)),
                dropped: dropped.clone(),
            }),
        ),
    )
    .await
    .expect("execute must return after bounded child cleanup")
    .expect_err("sink failure must escape");
    let dropped_when_execute_returned = dropped.load(Ordering::SeqCst);
    manager.cancel_all().await;

    assert!(error.to_string().contains("sink rejected child progress"));
    assert_eq!(dropped_when_execute_returned, 2);
}

#[tokio::test]
async fn cancel_all_terminalizes_without_repolling_execute() {
    let session_id = "cancel-drop";
    let agent_ids: Vec<AgentId> = vec!["child-a".into(), "child-b".into()];
    let plan = SchedulePlan {
        ready: vec![
            resolved_agent("child-a", "first", "first task", &[]),
            resolved_agent("child-b", "second", "second task", &[]),
        ],
        queued: Vec::new(),
        blocked: Vec::new(),
    };
    let store = Arc::new(MemoryStore::default());
    let manager = AgentManager::new(
        session_id.into(),
        None,
        1,
        store.clone(),
        Arc::new(CollectingSink::default()),
    );
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let finished = Arc::new(AtomicUsize::new(0));
    let mut executing = Box::pin(manager.execute(
        plan,
        "project".into(),
        Arc::new(CleanupRunner {
            started: Some(started_tx),
            finished: finished.clone(),
            cleanup_delay: Duration::from_millis(20),
        }),
    ));
    tokio::select! {
        result = &mut executing => panic!("execution ended before cancellation: {result:?}"),
        started = started_rx.recv() => assert_eq!(started.expect("child started"), agent_ids[0]),
    };

    manager.cancel_all().await;
    drop(executing);

    assert_eq!(finished.load(Ordering::SeqCst), 1);
    assert!(started_rx.try_recv().is_err());
    for agent_id in agent_ids {
        let inspection = manager.inspect(&agent_id).await.expect("inspect");
        assert_eq!(inspection.snapshot.state, AgentState::Cancelled);
        assert_eq!(cancelled_event_count(&store, session_id, &agent_id), 1);
    }
}

#[tokio::test]
async fn cancel_all_awaits_running_children_and_repoll_does_not_duplicate_events() {
    let session_id = "cancel-all";
    let plan = SchedulePlan {
        ready: vec![
            resolved_agent("child-a", "first", "first task", &[]),
            resolved_agent("child-b", "second", "second task", &[]),
            resolved_agent("child-c", "third", "third task", &[]),
        ],
        queued: Vec::new(),
        blocked: Vec::new(),
    };
    let agent_ids: Vec<_> = plan.ready.iter().map(|agent| agent.id.clone()).collect();
    let store = Arc::new(MemoryStore::default());
    let manager = AgentManager::new(
        session_id.into(),
        None,
        2,
        store.clone(),
        Arc::new(CollectingSink::default()),
    );
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let finished = Arc::new(AtomicUsize::new(0));
    let executing = manager.execute(
        plan,
        "project".into(),
        Arc::new(CleanupRunner {
            started: Some(started_tx),
            finished: finished.clone(),
            cleanup_delay: Duration::from_millis(20),
        }),
    );
    tokio::pin!(executing);
    for _ in 0..2 {
        tokio::select! {
            result = &mut executing => panic!("execution ended before cancellation: {result:?}"),
            started = started_rx.recv() => started.expect("child started"),
        };
    }

    manager.cancel_all().await;
    let finished_when_cancel_returned = finished.load(Ordering::SeqCst);
    tokio::time::timeout(Duration::from_millis(250), &mut executing)
        .await
        .expect("execution must stop")
        .expect("execute");

    assert_eq!(finished_when_cancel_returned, 2);
    assert_eq!(finished.load(Ordering::SeqCst), 2);
    assert!(started_rx.try_recv().is_err());
    for agent_id in agent_ids {
        let inspection = manager.inspect(&agent_id).await.expect("inspect");
        assert_eq!(inspection.snapshot.state, AgentState::Cancelled);
        assert_eq!(cancelled_event_count(&store, session_id, &agent_id), 1);
    }
}

impl ChildRunner for ControlledRunner {
    fn run(
        &self,
        mut context: ChildRunContext,
    ) -> BoxFuture<'static, Result<AgentResult, KuramaError>> {
        let started = self.started.clone();
        Box::pin(async move {
            started.send(context.agent_id.clone()).expect("started");
            tokio::select! {
                message = context.messages.recv() => Ok(AgentResult {
                    agent_id: context.agent_id,
                    summary: message.expect("message"),
                    changed_files: Vec::new(),
                    evidence_refs: Vec::new(),
                }),
                () = context.cancel.cancelled() => Err(KuramaError::Cancelled),
            }
        })
    }
}

#[tokio::test]
async fn manager_messages_one_child_and_cancels_another() {
    let orchestrator = SmartOrchestrator::new(Arc::new(SequenceIds::new(10)));
    let plan = orchestrator
        .resolve(
            DelegationRequest {
                agents: vec![agent("first", None, &[]), agent("second", None, &[])],
            },
            &context(),
        )
        .expect("resolve");
    let first = plan.ready[0].id.clone();
    let second = plan.ready[1].id.clone();
    let store = Arc::new(MemoryStore::default());
    let manager = Arc::new(AgentManager::new(
        "controlled".into(),
        None,
        4,
        store.clone(),
        Arc::new(CollectingSink::default()),
    ));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let executing = {
        let manager = manager.clone();
        tokio::spawn(async move {
            manager
                .execute(
                    plan,
                    "project".into(),
                    Arc::new(ControlledRunner {
                        started: started_tx,
                    }),
                )
                .await
        })
    };
    started_rx.recv().await.expect("first started");
    started_rx.recv().await.expect("second started");
    manager
        .message(&first, "continue with this".into())
        .await
        .expect("message");
    manager.cancel(&second).await.expect("cancel");
    let results = executing.await.expect("join").expect("execute");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].summary, "continue with this");
    let replay = store
        .replay_agent(&"controlled".into(), &first)
        .expect("replay messaged child");
    let message = replay
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                SessionEvent::AgentMessage { agent_id, text }
                    if agent_id == &first && text == "continue with this"
            )
        })
        .expect("durable child message");
    let completed = replay
        .iter()
        .position(|event| matches!(event.event, SessionEvent::AgentCompleted { .. }))
        .expect("durable child completion");
    assert!(message < completed);
    assert_eq!(
        manager
            .inspect(&second)
            .await
            .expect("inspect")
            .snapshot
            .state,
        AgentState::Cancelled
    );
}

#[tokio::test]
async fn queued_child_message_overflow_is_rejected_without_aborting_orchestration() {
    let orchestrator = SmartOrchestrator::new(Arc::new(SequenceIds::new(20)));
    let plan = orchestrator
        .resolve(
            DelegationRequest {
                agents: vec![agent("first", None, &[]), agent("second", None, &[])],
            },
            &context(),
        )
        .expect("resolve");
    let first = plan.ready[0].id.clone();
    let second = plan.ready[1].id.clone();
    let store = Arc::new(MemoryStore::default());
    let manager = Arc::new(AgentManager::new(
        "queued-message-cap".into(),
        None,
        1,
        store,
        Arc::new(CollectingSink::default()),
    ));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let executing = {
        let manager = manager.clone();
        tokio::spawn(async move {
            manager
                .execute(
                    plan,
                    "project".into(),
                    Arc::new(ControlledRunner {
                        started: started_tx,
                    }),
                )
                .await
        })
    };

    assert_eq!(started_rx.recv().await.expect("first started"), first);
    for index in 0..16 {
        manager
            .message(&second, format!("queued {index}"))
            .await
            .expect("message within capacity");
    }
    let error = manager
        .message(&second, "overflow".into())
        .await
        .expect_err("overflow must be rejected before launch");
    assert!(
        matches!(error, KuramaError::Protocol(message) if message.contains("too many queued child messages"))
    );

    manager.cancel(&first).await.expect("cancel first");
    assert_eq!(started_rx.recv().await.expect("second started"), second);
    let results = executing.await.expect("join").expect("execute");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].agent_id, second);
    assert_eq!(results[0].summary, "queued 0");
}

struct ApprovalRunner {
    requested: mpsc::UnboundedSender<kurama_protocol::id::AgentId>,
    release_cancelled: Arc<Notify>,
}

impl ChildRunner for ApprovalRunner {
    fn run(
        &self,
        context: ChildRunContext,
    ) -> BoxFuture<'static, Result<AgentResult, KuramaError>> {
        let requested = self.requested.clone();
        let release_cancelled = self.release_cancelled.clone();
        Box::pin(async move {
            let (response, decision) = oneshot::channel();
            context
                .approvals
                .send(ChildApproval {
                    agent_id: context.agent_id.clone(),
                    request: ApprovalRequest {
                        operation_id: format!("approval-{}", context.agent_id).into(),
                        operation: Operation::Write {
                            paths: vec![PathBuf::from("child.txt")],
                            destructive: false,
                            external: false,
                        },
                        summary: context.agent_id.to_string(),
                        arguments: serde_json::json!({"path":"child.txt"}),
                    },
                    response,
                })
                .await
                .expect("approval request");
            requested
                .send(context.agent_id.clone())
                .expect("approval requested");
            tokio::select! {
                biased;
                () = context.cancel.cancelled() => {
                    release_cancelled.notified().await;
                    Err(KuramaError::Cancelled)
                }
                response = decision => {
                    response.map_err(|_| KuramaError::Cancelled)?;
                    Ok(AgentResult {
                        agent_id: context.agent_id,
                        summary: "approved".into(),
                        changed_files: Vec::new(),
                        evidence_refs: Vec::new(),
                    })
                }
            }
        })
    }
}

#[tokio::test]
async fn terminal_child_approval_is_removed_and_next_live_request_is_promoted() {
    let plan = SmartOrchestrator::new(Arc::new(SequenceIds::new(20)))
        .resolve(
            DelegationRequest {
                agents: vec![agent("first", None, &[]), agent("second", None, &[])],
            },
            &context(),
        )
        .expect("resolve");
    let agent_ids: Vec<_> = plan.ready.iter().map(|spec| spec.id.clone()).collect();
    let (runtime_tx, mut runtime_rx) = mpsc::channel(16);
    let manager = Arc::new(
        AgentManager::new(
            "approvals".into(),
            None,
            2,
            Arc::new(MemoryStore::default()),
            Arc::new(CollectingSink::default()),
        )
        .with_runtime_sender(runtime_tx),
    );
    let (requested_tx, mut requested_rx) = mpsc::unbounded_channel();
    let release_cancelled = Arc::new(Notify::new());
    let executing = {
        let manager = manager.clone();
        let release_cancelled = release_cancelled.clone();
        tokio::spawn(async move {
            manager
                .execute(
                    plan,
                    "project".into(),
                    Arc::new(ApprovalRunner {
                        requested: requested_tx,
                        release_cancelled,
                    }),
                )
                .await
        })
    };

    requested_rx.recv().await.expect("first approval request");
    requested_rx.recv().await.expect("second approval request");
    let active = next_approval(&mut runtime_rx).await;
    assert_eq!(active.arguments, serde_json::json!({"path":"child.txt"}));
    let active_agent = agent_ids
        .iter()
        .find(|agent_id| agent_id.to_string() == active.summary)
        .expect("active approval agent")
        .clone();
    manager
        .cancel(&active_agent)
        .await
        .expect("cancel active child");

    let promoted = next_approval(&mut runtime_rx).await;
    assert_eq!(promoted.arguments, serde_json::json!({"path":"child.txt"}));
    assert_ne!(promoted.summary, active.summary);
    let error = manager
        .resolve_approval(&active.operation_id, ApprovalResponse::ApproveOnce)
        .await
        .expect_err("stale approval must not authorize the promoted request");
    assert!(error.to_string().contains("no longer pending"));
    manager
        .resolve_approval(&promoted.operation_id, ApprovalResponse::ApproveOnce)
        .await
        .expect("approve promoted request");
    release_cancelled.notify_one();

    let results = executing.await.expect("join").expect("execute");
    assert_eq!(results.len(), 1);
}

async fn next_approval(events: &mut mpsc::Receiver<RuntimeEvent>) -> ApprovalRequest {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let RuntimeEvent::ApprovalRequired { request } =
                events.recv().await.expect("runtime event")
            {
                break request;
            }
        }
    })
    .await
    .expect("approval timeout")
}

struct EscalatingRunner {
    attempts: AtomicUsize,
    profiles: Mutex<Vec<String>>,
    delays: [Duration; 3],
}

impl ChildRunner for EscalatingRunner {
    fn run(
        &self,
        context: ChildRunContext,
    ) -> BoxFuture<'static, Result<AgentResult, KuramaError>> {
        let attempt = self.attempts.fetch_add(1, Ordering::Relaxed);
        self.profiles
            .lock()
            .expect("profiles")
            .push(context.launch.profile.name.clone());
        let delay = self.delays.get(attempt).copied().unwrap_or_default();
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            if attempt < 2 {
                Err(KuramaError::Model("capability mismatch".into()))
            } else {
                Ok(AgentResult {
                    agent_id: context.agent_id,
                    summary: "escalated".into(),
                    changed_files: Vec::new(),
                    evidence_refs: Vec::new(),
                })
            }
        })
    }
}

#[tokio::test]
async fn manager_retries_twice_then_uses_configured_escalation() {
    let mut orchestration = context();
    orchestration
        .role_escalations
        .insert("reviewer".into(), vec!["user".into()]);
    let profiles = orchestration.profiles.clone();
    let plan = SmartOrchestrator::new(Arc::new(SequenceIds::new(30)))
        .resolve(
            DelegationRequest {
                agents: vec![agent("reviewer", None, &[])],
            },
            &orchestration,
        )
        .expect("resolve");
    let runner = Arc::new(EscalatingRunner {
        attempts: AtomicUsize::new(0),
        profiles: Mutex::new(Vec::new()),
        delays: [Duration::ZERO; 3],
    });
    let manager = AgentManager::new(
        "escalation".into(),
        None,
        1,
        Arc::new(MemoryStore::default()),
        Arc::new(CollectingSink::default()),
    )
    .with_profiles(profiles);
    let results = manager
        .execute(plan, "project".into(), runner.clone())
        .await
        .expect("execute");
    assert_eq!(results[0].summary, "escalated");
    assert_eq!(
        runner.profiles.lock().expect("profiles").as_slice(),
        &["review", "review", "user"]
    );
}

#[tokio::test]
async fn manager_enforces_seconds_across_retries_and_escalation() {
    let mut orchestration = context();
    orchestration
        .role_escalations
        .insert("reviewer".into(), vec!["user".into()]);
    let profiles = orchestration.profiles.clone();
    let mut spec = agent("reviewer", None, &[]);
    spec.budget.max_seconds = 1;
    let plan = SmartOrchestrator::new(Arc::new(SequenceIds::new(40)))
        .resolve(DelegationRequest { agents: vec![spec] }, &orchestration)
        .expect("resolve");
    let agent_id = plan.ready[0].id.clone();
    let runner = Arc::new(EscalatingRunner {
        attempts: AtomicUsize::new(0),
        profiles: Mutex::new(Vec::new()),
        delays: [
            Duration::from_millis(150),
            Duration::from_millis(150),
            Duration::from_millis(850),
        ],
    });
    let manager = AgentManager::new(
        "time-budget".into(),
        None,
        1,
        Arc::new(MemoryStore::default()),
        Arc::new(CollectingSink::default()),
    )
    .with_profiles(profiles);

    let results = manager
        .execute(plan, "project".into(), runner.clone())
        .await
        .expect("execute");

    assert!(results.is_empty());
    assert_eq!(runner.attempts.load(Ordering::Relaxed), 3);
    assert_eq!(
        runner.profiles.lock().expect("profiles").as_slice(),
        &["review", "review", "user"]
    );
    let inspection = manager.inspect(&agent_id).await.expect("inspect");
    assert_eq!(inspection.snapshot.state, AgentState::Failed);
    assert_eq!(
        inspection.snapshot.last_error.as_deref(),
        Some("child execution timed out")
    );
}
