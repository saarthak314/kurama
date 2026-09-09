use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use kurama_core::{orchestrator::SmartOrchestrator, testing::SequenceIds};
use kurama_protocol::{
    agent::{
        AgentBudget, AgentSpec, DelegationRequest, OrchestrationContext, ResolvedAgentSpec,
        WriteScope,
    },
    model::ModelProfile,
    traits::Orchestrator,
};

fn context() -> OrchestrationContext {
    let parent_profile = ModelProfile::new("parent", "model", 100_000, 10_000);
    OrchestrationContext {
        parent_profile: parent_profile.clone(),
        profiles: BTreeMap::from([("parent".into(), parent_profile)]),
        role_routes: BTreeMap::new(),
        role_escalations: BTreeMap::new(),
        profile_escalations: BTreeMap::new(),
        parent_write_scope: WriteScope {
            roots: vec![PathBuf::from("/parent/workspace")],
            files: Vec::new(),
        },
        max_concurrency: 4,
        depth: 0,
        yolo: false,
    }
}

fn resolve_one(
    objective: &str,
    write_scope: WriteScope,
    context: &OrchestrationContext,
) -> Result<ResolvedAgentSpec, kurama_protocol::KuramaError> {
    let request = DelegationRequest {
        agents: vec![AgentSpec {
            role: "model-selected".into(),
            objective: objective.into(),
            profile: None,
            context_refs: Vec::new(),
            write_scope,
            budget: AgentBudget::default(),
            depends_on: Vec::new(),
        }],
    };
    let plan = SmartOrchestrator::new(Arc::new(SequenceIds::new(1))).resolve(request, context)?;
    Ok(plan.ready.into_iter().next().expect("one ready agent"))
}

#[test]
fn researcher_is_read_only_even_when_request_has_roots() {
    let resolved = resolve_one(
        "investigate auth",
        WriteScope {
            roots: vec![PathBuf::from("/outside")],
            files: Vec::new(),
        },
        &context(),
    )
    .expect("resolve researcher");

    assert_eq!(resolved.role, "researcher");
    assert!(resolved.write_scope.is_read_only());
}

#[test]
fn reviewer_is_read_only() {
    let resolved = resolve_one(
        "review the diff",
        WriteScope {
            roots: vec![PathBuf::from("/parent/workspace")],
            files: Vec::new(),
        },
        &context(),
    )
    .expect("resolve reviewer");

    assert_eq!(resolved.role, "reviewer");
    assert!(resolved.write_scope.is_read_only());
}

#[test]
fn planner_is_read_only() {
    let resolved = resolve_one(
        "plan the migration",
        WriteScope {
            roots: vec![PathBuf::from("/parent/workspace")],
            files: Vec::new(),
        },
        &context(),
    )
    .expect("resolve planner");

    assert_eq!(resolved.role, "planner");
    assert!(resolved.write_scope.is_read_only());
}

#[test]
fn implementer_with_empty_scope_inherits_parent_scope() {
    let context = context();
    let resolved = resolve_one("implement the fix", WriteScope::default(), &context)
        .expect("resolve implementer");

    assert_eq!(resolved.role, "implementer");
    assert_eq!(resolved.write_scope, context.parent_write_scope);
}

#[test]
fn implementer_keeps_files_inside_parent_scope() {
    let write_scope = WriteScope {
        roots: Vec::new(),
        files: vec![PathBuf::from("/parent/workspace/src/lib.rs")],
    };
    let resolved = resolve_one("implement the fix", write_scope.clone(), &context())
        .expect("resolve implementer");

    assert_eq!(resolved.role, "implementer");
    assert_eq!(resolved.write_scope, write_scope);
}

#[test]
fn implementer_outside_parent_scope_is_rejected() {
    let error = resolve_one(
        "implement the fix",
        WriteScope {
            roots: vec![PathBuf::from("/outside")],
            files: Vec::new(),
        },
        &context(),
    )
    .expect_err("outside scope must be rejected");

    assert!(error.to_string().contains("write scope exceeds"));
}
