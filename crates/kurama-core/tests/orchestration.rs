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
        WriteScope,
    },
    model::ModelProfile,
    policy::{ApprovalRequest, ApprovalResponse},
    runtime::RuntimeEvent,
    tool::Operation,
    traits::{BoxFuture, Orchestrator},
};
use tokio::{
    sync::{Notify, mpsc, oneshot},
    time::Duration,
};

fn agent(role: &str, profile: Option<&str>, depends_on: &[&str]) -> AgentSpec {
    AgentSpec {
        role: role.into(),
        objective: format!("do {role}"),
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

#[test]
fn explicit_delegation_gate_is_off_for_ordinary_turns() {
    let orchestrator = SmartOrchestrator::new(Arc::new(SequenceIds::new(1)));
    assert!(!orchestrator.explicit_delegation("fix the failing test"));
    assert!(orchestrator.explicit_delegation("use sub-agents to fix it"));
    assert!(orchestrator.explicit_delegation("parallelize this with a reviewer"));
}

#[test]
fn routes_profiles_and_queues_above_concurrency() {
    let orchestrator = SmartOrchestrator::new(Arc::new(SequenceIds::new(1)));
    let request = DelegationRequest {
        agents: vec![
            agent("custom", Some("user"), &[]),
            agent("reviewer", None, &[]),
            agent("third", None, &[]),
            agent("fourth", None, &[]),
            agent("fifth", None, &[]),
        ],
    };
    let plan = orchestrator.resolve(request, &context()).expect("resolve");
    assert_eq!(plan.ready[0].profile.name, "user");
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
    let manager = AgentManager::new(
        "session".into(),
        None,
        4,
        Arc::new(MemoryStore::default()),
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
}

struct ControlledRunner {
    started: mpsc::UnboundedSender<kurama_protocol::id::AgentId>,
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
    let manager = Arc::new(AgentManager::new(
        "controlled".into(),
        None,
        4,
        Arc::new(MemoryStore::default()),
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
        Box::pin(async move {
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
