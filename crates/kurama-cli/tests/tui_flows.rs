use kurama_cli::tui::{OnboardingState, TuiState};
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
    state.replace_approval_arguments(serde_json::json!({"path":"safe.txt","content":"safe"}));
    state.submit_approval_edit().expect("submit edit");
    assert!(matches!(
        state.sent_commands().last(),
        Some(EngineCommand::ResolveApproval(ApprovalResponse::Edit { arguments }))
            if arguments["path"] == "safe.txt"
    ));
}
