use kurama_cli::tui::{OnboardingState, Overlay, TuiState};
use kurama_protocol::{
    id::OperationId,
    policy::{ApprovalRequest, ApprovalResponse, ExecutionMode},
    runtime::EngineCommand,
    tool::Operation,
};

#[test]
fn onboarding_offers_only_supported_connection_types() {
    let state = OnboardingState::new();
    assert_eq!(
        state.options(),
        [
            "Codex subscription",
            "Claude subscription",
            "OpenAI API key",
            "Anthropic API key",
            "OpenAI-compatible or local endpoint",
        ]
    );
    assert!(
        !state
            .options()
            .iter()
            .any(|option| option.contains("Gemini"))
    );
}

#[test]
fn approval_overlay_supports_approve_deny_and_edited_arguments() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.begin_approval(
        ApprovalRequest {
            operation_id: OperationId::from("o_1"),
            operation: Operation::Write {
                paths: vec!["safe.txt".into()],
                destructive: false,
                external: false,
            },
            summary: "Write safe.txt".into(),
        },
        serde_json::json!({"path":"unsafe.txt","content":"unsafe"}),
    );
    state.begin_approval_edit();
    state.set_approval_editor(r#"{"path":"safe.txt","content":"safe"}"#);
    state.submit_approval_edit().expect("submit edit");
    assert!(matches!(
        state.sent_commands().last(),
        Some(EngineCommand::ResolveApproval(ApprovalResponse::Edit { arguments }))
            if arguments["path"] == "safe.txt"
    ));
}

#[test]
fn invalid_approval_json_stays_open_and_reports_the_error() {
    let mut state = approval_state();
    state.begin_approval_edit();
    state.set_approval_editor(r#"{"path":"safe.txt""#);

    let error = state
        .submit_approval_edit()
        .expect_err("invalid JSON must not submit");

    assert!(error.contains("invalid approval arguments"));
    assert_eq!(state.overlay(), Overlay::ApprovalEdit);
    assert!(state.approval.is_some());
    assert!(state.status.contains("invalid approval arguments"));
    assert!(state.sent_commands().is_empty());
}

#[test]
fn approval_edit_escape_returns_to_the_pending_approval() {
    let mut state = approval_state();
    state.begin_approval_edit();

    state.close_overlay();

    assert_eq!(state.overlay(), Overlay::Approval);
    assert!(state.approval.is_some());
    assert!(!state.approval.as_ref().expect("approval").editing);
}

#[test]
fn unresolved_approval_cannot_be_dismissed() {
    let mut state = approval_state();

    state.close_overlay();

    assert_eq!(state.overlay(), Overlay::Approval);
    assert!(state.approval.is_some());
    assert!(state.sent_commands().is_empty());
}

fn approval_state() -> TuiState {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.begin_approval(
        ApprovalRequest {
            operation_id: OperationId::from("o_1"),
            operation: Operation::Write {
                paths: vec!["safe.txt".into()],
                destructive: false,
                external: false,
            },
            summary: "Write safe.txt".into(),
        },
        serde_json::json!({"path":"unsafe.txt","content":"unsafe"}),
    );
    state
}
