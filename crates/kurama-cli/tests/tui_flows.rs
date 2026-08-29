use kurama_cli::tui::{OnboardingState, Overlay, TuiState};
use kurama_protocol::{
    id::OperationId,
    policy::{ApprovalRequest, ApprovalResponse, ExecutionMode},
    runtime::{EngineCommand, RuntimeEvent},
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
fn onboarding_masks_session_credentials_in_the_rendered_form() {
    let mut state = TuiState::credential(".", "remote");
    for character in "top-secret".chars() {
        state.onboarding.push(character);
    }
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");

    terminal
        .draw(|frame| kurama_cli::tui::render(frame, &state))
        .expect("render");
    let buffer = format!("{:?}", terminal.backend().buffer());

    assert!(!buffer.contains("top-secret"));
    assert!(buffer.contains("••••••••••"));
}

#[test]
fn approval_overlay_supports_approve_deny_and_edited_arguments() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.begin_approval(ApprovalRequest {
        operation_id: OperationId::from("o_1"),
        operation: Operation::Write {
            paths: vec!["safe.txt".into()],
            destructive: false,
            external: false,
        },
        summary: "Write safe.txt".into(),
        arguments: serde_json::json!({"path":"unsafe.txt","content":"unsafe"}),
    });
    state.begin_approval_edit();
    state.set_approval_editor(r#"{"path":"safe.txt","content":"safe"}"#);
    state.submit_approval_edit().expect("submit edit");
    assert!(matches!(
        state.sent_commands().last(),
        Some(EngineCommand::ResolveApproval {
            operation_id,
            response: ApprovalResponse::Edit { arguments },
        }) if operation_id.as_ref() == "o_1" && arguments["path"] == "safe.txt"
    ));
}

#[test]
fn runtime_approval_hydrates_editor_from_request_arguments() {
    let arguments = serde_json::json!({"path":"safe.txt","content":"safe"});
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);

    state.apply_runtime_event(RuntimeEvent::ApprovalRequired {
        request: ApprovalRequest {
            operation_id: OperationId::from("o_1"),
            operation: Operation::Write {
                paths: vec!["safe.txt".into()],
                destructive: false,
                external: false,
            },
            summary: "Write safe.txt".into(),
            arguments: arguments.clone(),
        },
    });

    let approval = state.approval.as_ref().expect("approval");
    assert_eq!(approval.arguments, arguments);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&approval.editor).expect("editor JSON"),
        arguments
    );
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
    state.begin_approval(ApprovalRequest {
        operation_id: OperationId::from("o_1"),
        operation: Operation::Write {
            paths: vec!["safe.txt".into()],
            destructive: false,
            external: false,
        },
        summary: "Write safe.txt".into(),
        arguments: serde_json::json!({"path":"unsafe.txt","content":"unsafe"}),
    });
    state
}
