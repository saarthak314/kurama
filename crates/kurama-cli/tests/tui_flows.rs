use kurama_cli::tui::{OnboardingState, Overlay, TranscriptKind, TuiState};
use kurama_protocol::{
    id::{CallId, OperationId, SessionId},
    policy::{ApprovalRequest, ApprovalResponse, ExecutionMode},
    runtime::{EngineCommand, RuntimeEvent},
    session::{EventEnvelope, SessionEvent},
    tool::{Operation, ToolResult},
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

#[test]
fn tool_output_deltas_for_the_same_call_update_one_entry() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);

    state.apply_runtime_event(tool_delta("call_1", "stdout", "first "));
    state.apply_runtime_event(tool_delta("call_1", "stdout", "second"));

    assert_eq!(state.transcript.len(), 1);
    assert_eq!(state.transcript[0].kind, TranscriptKind::Tool);
    assert_eq!(state.transcript[0].body, "first second");
}

#[test]
fn tool_completion_finalizes_the_streamed_entry_by_result_call_id() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);

    state.apply_runtime_event(tool_delta("call_1", "stdout", "partial"));
    state.apply_runtime_event(RuntimeEvent::ToolCompleted {
        operation_id: OperationId::from("operation_unrelated_to_call_id"),
        result: tool_result("call_1", "final output", "bash"),
    });

    assert_eq!(state.transcript.len(), 1);
    assert_eq!(state.transcript[0].label, "TOOL / bash");
    assert_eq!(state.transcript[0].body, "final output");
}

#[test]
fn tool_streams_with_different_call_ids_remain_separate() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);

    state.apply_runtime_event(tool_delta("call_1", "stdout", "one"));
    state.apply_runtime_event(tool_delta("call_2", "stderr", "two"));
    state.apply_runtime_event(tool_delta("call_1", "stdout", " more"));

    assert_eq!(state.transcript.len(), 2);
    assert_eq!(state.transcript[0].body, "one more");
    assert_eq!(state.transcript[1].body, "two");
}

#[test]
fn assistant_deltas_coalesce_only_while_adjacent() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);

    state.apply_runtime_event(RuntimeEvent::AssistantDelta {
        text: "first ".into(),
    });
    state.apply_runtime_event(RuntimeEvent::AssistantDelta {
        text: "second".into(),
    });

    assert_eq!(state.transcript.len(), 1);
    assert_eq!(state.transcript[0].kind, TranscriptKind::Assistant);
    assert_eq!(state.transcript[0].body, "first second");
    state.push_user("interrupt");
    state.apply_runtime_event(RuntimeEvent::AssistantDelta {
        text: "third".into(),
    });

    assert_eq!(state.transcript.len(), 3);
    assert_eq!(state.transcript[1].kind, TranscriptKind::User);
    assert_eq!(state.transcript[2].kind, TranscriptKind::Assistant);
    assert_eq!(state.transcript[2].body, "third");
}

#[test]
fn replay_hydration_clears_active_tool_stream_tracking() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.apply_runtime_event(tool_delta("call_1", "stdout", "stale"));

    state.hydrate_replay(&[replay_event(SessionEvent::ToolCompleted {
        operation_id: OperationId::from("replayed_operation"),
        result: tool_result("replayed_call", "replayed", "read"),
    })]);
    state.apply_runtime_event(RuntimeEvent::ToolCompleted {
        operation_id: OperationId::from("operation_1"),
        result: tool_result("call_1", "fresh", "bash"),
    });

    assert_eq!(state.transcript.len(), 2);
    assert_eq!(state.transcript[0].kind, TranscriptKind::Tool);
    assert_eq!(state.transcript[0].body, "replayed");
    assert_eq!(state.transcript[1].kind, TranscriptKind::Tool);
    assert_eq!(state.transcript[1].body, "fresh");
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

fn tool_delta(call_id: &str, stream: &str, chunk: &str) -> RuntimeEvent {
    RuntimeEvent::ToolOutputDelta {
        call_id: CallId::from(call_id),
        stream: stream.into(),
        chunk: chunk.into(),
    }
}

fn tool_result(call_id: &str, output: &str, tool_name: &str) -> ToolResult {
    let mut result = ToolResult::success(CallId::from(call_id), output);
    result.metadata = serde_json::json!({"tool_name": tool_name});
    result
}

fn replay_event(event: SessionEvent) -> EventEnvelope {
    EventEnvelope::new(1, 1, SessionId::from("session_1"), None, event)
}
