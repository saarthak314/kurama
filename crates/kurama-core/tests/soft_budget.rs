use std::sync::{Arc, Mutex};

use kurama_core::{
    agent_manager::{AgentManager, ChildProgress, ChildRunContext, ChildRunner},
    testing::{CollectingSink, MemoryStore},
};
use kurama_protocol::{
    KuramaError,
    agent::{AgentBudget, AgentResult, AgentState, ResolvedAgentSpec, SchedulePlan, WriteScope},
    model::ModelProfile,
    traits::BoxFuture,
};
use tokio::sync::{mpsc, oneshot};

fn plan(max_turns: u32, max_seconds: u64) -> SchedulePlan {
    SchedulePlan {
        ready: vec![ResolvedAgentSpec {
            id: "child".into(),
            parent_id: None,
            depth: 1,
            role: "implementer".into(),
            objective: "implement the change".into(),
            profile: ModelProfile::new("default", "model", 100_000, 10_000),
            context_refs: Vec::new(),
            write_scope: WriteScope::default(),
            budget: AgentBudget {
                max_input_tokens: 80_000,
                max_output_tokens: 8_000,
                max_turns,
                max_seconds,
            },
            depends_on: Vec::new(),
            escalation_profiles: Vec::new(),
        }],
        queued: Vec::new(),
        blocked: Vec::new(),
    }
}

struct TurnBudgetRunner {
    warning: mpsc::UnboundedSender<String>,
    exceed: Mutex<Option<oneshot::Receiver<()>>>,
}

impl ChildRunner for TurnBudgetRunner {
    fn run(
        &self,
        mut context: ChildRunContext,
    ) -> BoxFuture<'static, Result<AgentResult, KuramaError>> {
        let warning = self.warning.clone();
        let exceed = self.exceed.lock().expect("exceed receiver").take().unwrap();
        Box::pin(async move {
            context
                .progress
                .send(ChildProgress {
                    completed_turns: 10,
                    ..ChildProgress::default()
                })
                .await
                .expect("progress");
            let message = context.messages.recv().await.expect("wrap-up message");
            warning.send(message).expect("warning observation");
            exceed.await.expect("exceed signal");
            context
                .progress
                .send(ChildProgress {
                    completed_turns: 13,
                    ..ChildProgress::default()
                })
                .await
                .expect("over-budget progress");
            context.cancel.cancelled().await;
            Err(KuramaError::Cancelled)
        })
    }
}

#[tokio::test]
async fn turn_budget_warns_once_at_eighty_percent_then_hard_stops() {
    let manager = Arc::new(AgentManager::new(
        "soft-budget".into(),
        None,
        1,
        Arc::new(MemoryStore::default()),
        Arc::new(CollectingSink::default()),
    ));
    let (warning_tx, mut warning_rx) = mpsc::unbounded_channel();
    let (exceed_tx, exceed_rx) = oneshot::channel();
    let executing = {
        let manager = manager.clone();
        tokio::spawn(async move {
            manager
                .execute(
                    plan(12, 60),
                    "project".into(),
                    Arc::new(TurnBudgetRunner {
                        warning: warning_tx,
                        exceed: Mutex::new(Some(exceed_rx)),
                    }),
                )
                .await
        })
    };

    let warning = warning_rx.recv().await.expect("warning");
    assert!(warning.contains("wrap up"));
    let child_id = "child".into();
    let inspection = manager.inspect(&child_id).await.expect("inspection");
    assert_eq!(inspection.snapshot.state, AgentState::Running);
    assert_eq!(inspection.snapshot.phase.as_deref(), Some("wrapping up"));

    exceed_tx.send(()).expect("exceed");
    let results = executing.await.expect("execution task").expect("execute");
    assert!(results.is_empty());
    let inspection = manager.inspect(&child_id).await.expect("inspection");
    assert_eq!(inspection.snapshot.state, AgentState::Cancelled);
    assert_eq!(
        inspection.snapshot.last_error.as_deref(),
        Some("child budget exhausted")
    );
    assert!(warning_rx.try_recv().is_err());
}

struct TimeBudgetRunner {
    warning: mpsc::UnboundedSender<String>,
}

impl ChildRunner for TimeBudgetRunner {
    fn run(
        &self,
        mut context: ChildRunContext,
    ) -> BoxFuture<'static, Result<AgentResult, KuramaError>> {
        let warning = self.warning.clone();
        Box::pin(async move {
            let message = context.messages.recv().await.expect("wrap-up message");
            warning.send(message).expect("warning observation");
            context.cancel.cancelled().await;
            Err(KuramaError::Cancelled)
        })
    }
}

struct TokenBudgetRunner {
    warning: mpsc::UnboundedSender<String>,
}

impl ChildRunner for TokenBudgetRunner {
    fn run(
        &self,
        mut context: ChildRunContext,
    ) -> BoxFuture<'static, Result<AgentResult, KuramaError>> {
        let warning = self.warning.clone();
        Box::pin(async move {
            context
                .progress
                .send(ChildProgress {
                    usage: kurama_protocol::model::Usage {
                        input_tokens: 64_000,
                        output_tokens: 0,
                        cached_input_tokens: 0,
                    },
                    completed_turns: 1,
                    ..ChildProgress::default()
                })
                .await
                .expect("progress");
            let message = context.messages.recv().await.expect("wrap-up message");
            warning.send(message).expect("warning observation");
            Ok(AgentResult {
                agent_id: context.agent_id,
                summary: "wrapped".into(),
                changed_files: Vec::new(),
                evidence_refs: Vec::new(),
            })
        })
    }
}

#[tokio::test]
async fn token_budget_warns_at_eighty_percent() {
    let manager = Arc::new(AgentManager::new(
        "token-soft-budget".into(),
        None,
        1,
        Arc::new(MemoryStore::default()),
        Arc::new(CollectingSink::default()),
    ));
    let (warning_tx, mut warning_rx) = mpsc::unbounded_channel();
    let executing = {
        let manager = manager.clone();
        tokio::spawn(async move {
            manager
                .execute(
                    plan(12, 60),
                    "project".into(),
                    Arc::new(TokenBudgetRunner {
                        warning: warning_tx,
                    }),
                )
                .await
        })
    };

    let warning = warning_rx.recv().await.expect("warning");
    assert!(warning.contains("wrap up"));
    let results = executing.await.expect("execution task").expect("execute");
    assert_eq!(results.len(), 1);
    let inspection = manager.inspect(&"child".into()).await.expect("inspection");
    assert_eq!(inspection.snapshot.state, AgentState::Completed);
    assert_eq!(inspection.snapshot.phase.as_deref(), Some("wrapping up"));
}

#[tokio::test]
async fn time_budget_warns_before_the_existing_hard_timeout() {
    let manager = Arc::new(AgentManager::new(
        "time-soft-budget".into(),
        None,
        1,
        Arc::new(MemoryStore::default()),
        Arc::new(CollectingSink::default()),
    ));
    let (warning_tx, mut warning_rx) = mpsc::unbounded_channel();
    let executing = {
        let manager = manager.clone();
        tokio::spawn(async move {
            manager
                .execute(
                    plan(12, 1),
                    "project".into(),
                    Arc::new(TimeBudgetRunner {
                        warning: warning_tx,
                    }),
                )
                .await
        })
    };

    let warning = warning_rx.recv().await.expect("warning");
    assert!(warning.contains("wrap up"));
    let results = executing.await.expect("execution task").expect("execute");
    assert!(results.is_empty());
    let inspection = manager.inspect(&"child".into()).await.expect("inspection");
    assert_eq!(inspection.snapshot.state, AgentState::Failed);
    assert_eq!(
        inspection.snapshot.last_error.as_deref(),
        Some("child execution timed out")
    );
}
