use kurama_cli::tui::{
    ActivityState, OnboardingState, Overlay, ToolLifecycle, TranscriptEntry, TuiState,
};
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
fn runtime_events_drive_typed_activity_without_duplicate_errors() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.set_thinking();
    assert!(matches!(state.activity(), ActivityState::Thinking { .. }));

    state.apply_runtime_event(RuntimeEvent::ToolStarted {
        operation_id: OperationId::from("operation_1"),
        name: "bash".into(),
    });
    assert!(matches!(state.activity(), ActivityState::RunningTool { name, .. } if name == "bash"));

    state.apply_runtime_event(RuntimeEvent::Error {
        message: "broken".into(),
    });
    assert_eq!(state.activity(), &ActivityState::Idle);
    assert_eq!(
        state
            .transcript
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::Error { body } if body == "broken"))
            .count(),
        1
    );
}

#[test]
fn runtime_status_and_shutdown_return_to_idle() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);

    state.apply_runtime_event(RuntimeEvent::Status {
        message: "compacting context".into(),
    });
    assert!(matches!(
        state.activity(),
        ActivityState::Working { label, .. } if label == "compacting context"
    ));

    state.apply_runtime_event(RuntimeEvent::Shutdown);
    assert_eq!(state.activity(), &ActivityState::Idle);
}

#[test]
fn tool_events_preserve_complete_output_and_lifecycle() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.apply_runtime_event(RuntimeEvent::ToolStarted {
        operation_id: OperationId::from("operation_1"),
        name: "bash".into(),
    });
    state.apply_runtime_event(tool_delta(
        "call_1",
        "stdout",
        "head\nfull middle output\ntail\n",
    ));

    assert!(matches!(
        &state.transcript[0],
        TranscriptEntry::ToolCall(tool)
            if tool.call_id.as_ref().is_some_and(|call_id| call_id.as_ref() == "call_1")
                && tool.name == "bash"
                && tool.lifecycle == ToolLifecycle::Running
    ));

    let mut result = tool_result("call_1", "head\n[omitted]\ntail\n", "bash");
    result.truncated = true;
    state.apply_runtime_event(RuntimeEvent::ToolCompleted {
        operation_id: OperationId::from("operation_1"),
        result,
    });

    assert_eq!(state.activity(), &ActivityState::Idle);
    assert!(matches!(
        &state.transcript[0],
        TranscriptEntry::ToolCall(tool)
            if tool.output == "head\nfull middle output\ntail\n"
                && tool.lifecycle == ToolLifecycle::Completed
    ));
}

#[test]
fn replay_hydrates_explicit_transcript_variants() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.hydrate_replay(&[
        replay_event(SessionEvent::UserMessage {
            text: "inspect".into(),
        }),
        replay_event(SessionEvent::AssistantMessage {
            text: "checking".into(),
        }),
        replay_event(SessionEvent::ToolCompleted {
            operation_id: OperationId::from("operation_1"),
            result: tool_result("call_1", "done", "read"),
        }),
        replay_event(SessionEvent::ModeSelected {
            mode: ExecutionMode::Auto,
        }),
        replay_event(SessionEvent::TurnFailed {
            error: "broken".into(),
        }),
    ]);

    assert!(matches!(
        &state.transcript[..],
        [
            TranscriptEntry::UserTurn { body: user },
            TranscriptEntry::AssistantMessage { body: assistant },
            TranscriptEntry::ToolCall(tool),
            TranscriptEntry::Notice {
                label: Some(label),
                body: mode,
            },
            TranscriptEntry::Error { body: error },
        ] if user == "inspect"
            && assistant == "checking"
            && tool.name == "read"
            && tool.output == "done"
            && tool.lifecycle == ToolLifecycle::Completed
            && label == "MODE"
            && mode == "auto"
            && error == "broken"
    ));
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
    assert_eq!(state.activity(), &ActivityState::AwaitingApproval);
    state.begin_approval_edit();
    state.set_approval_editor(r#"{"path":"safe.txt","content":"safe"}"#);
    state.submit_approval_edit().expect("submit edit");
    assert!(matches!(state.activity(), ActivityState::Thinking { .. }));
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

    assert_eq!(state.activity(), &ActivityState::AwaitingApproval);
    let approval = state.approval.as_ref().expect("approval");
    assert_eq!(approval.arguments, arguments);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&approval.editor).expect("editor JSON"),
        arguments
    );
}

#[test]
fn transcript_view_toggle_is_reversible() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);

    assert!(!state.transcript_view_expanded());
    state.toggle_transcript_view();
    assert!(state.transcript_view_expanded());
    state.toggle_transcript_view();
    assert!(!state.transcript_view_expanded());
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
    assert_eq!(state.activity(), &ActivityState::AwaitingApproval);
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
    assert!(matches!(
        &state.transcript[0],
        TranscriptEntry::ToolCall(tool)
            if tool.output == "first second" && tool.lifecycle == ToolLifecycle::Running
    ));
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
    assert!(matches!(
        &state.transcript[0],
        TranscriptEntry::ToolCall(tool)
            if tool.name == "bash"
                && tool.output == "final output"
                && tool.lifecycle == ToolLifecycle::Completed
    ));
}

#[test]
fn truncated_tool_completion_preserves_full_streamed_output() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.apply_runtime_event(tool_delta(
        "call_1",
        "stdout",
        "head\nfull middle output\ntail\n",
    ));
    let mut result = tool_result("call_1", "head\n[omitted]\ntail\n", "bash");
    result.truncated = true;

    state.apply_runtime_event(RuntimeEvent::ToolCompleted {
        operation_id: OperationId::from("operation_1"),
        result,
    });

    assert_eq!(state.transcript.len(), 1);
    assert!(matches!(
        &state.transcript[0],
        TranscriptEntry::ToolCall(tool) if tool.output == "head\nfull middle output\ntail\n"
    ));
}

#[test]
fn truncated_mixed_streams_keep_explicit_stream_boundaries() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.apply_runtime_event(tool_delta("call_1", "stdout", "output"));
    state.apply_runtime_event(tool_delta("call_1", "stderr", "warning"));
    state.apply_runtime_event(tool_delta("call_1", "stdout", "done"));
    let mut result = tool_result("call_1", "output\n[stderr]\nwarningdone", "bash");
    result.truncated = true;

    state.apply_runtime_event(RuntimeEvent::ToolCompleted {
        operation_id: OperationId::from("operation_1"),
        result,
    });

    assert!(matches!(
        &state.transcript[0],
        TranscriptEntry::ToolCall(tool)
            if tool.output == "output\n[stderr]\nwarning\n[stdout]\ndone"
    ));
}

#[test]
fn execution_errors_append_after_streamed_diagnostics() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.apply_runtime_event(tool_delta("call_1", "stderr", "diagnostic\n"));
    let mut result = tool_result("call_1", "command timed out after 1000 ms", "bash");
    result.is_error = true;
    result.metadata["execution_error"] = serde_json::Value::Bool(true);

    state.apply_runtime_event(RuntimeEvent::ToolCompleted {
        operation_id: OperationId::from("operation_1"),
        result,
    });

    assert!(matches!(
        &state.transcript[0],
        TranscriptEntry::ToolCall(tool)
            if tool.output == "diagnostic\n\n[error]\ncommand timed out after 1000 ms"
                && tool.lifecycle == ToolLifecycle::Failed
    ));
}

#[test]
fn replay_uses_display_hydrated_tool_output() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    let mut result = tool_result("call_1", "head\n[omitted]\ntail\n", "bash");
    result.truncated = true;
    result.metadata["display_output"] =
        serde_json::Value::String("head\nfull middle output\ntail\n".into());

    state.hydrate_replay(&[replay_event(SessionEvent::ToolCompleted {
        operation_id: OperationId::from("operation_1"),
        result,
    })]);

    assert!(matches!(
        &state.transcript[0],
        TranscriptEntry::ToolCall(tool) if tool.output == "head\nfull middle output\ntail\n"
    ));
}

#[test]
fn tool_streams_with_different_call_ids_remain_separate() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);

    state.apply_runtime_event(tool_delta("call_1", "stdout", "one"));
    state.apply_runtime_event(tool_delta("call_2", "stderr", "two"));
    state.apply_runtime_event(tool_delta("call_1", "stdout", " more"));

    assert_eq!(state.transcript.len(), 2);
    assert!(matches!(
        &state.transcript[..],
        [TranscriptEntry::ToolCall(first), TranscriptEntry::ToolCall(second)]
            if first.output == "one more" && second.output == "two"
    ));
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
    assert!(matches!(
        &state.transcript[0],
        TranscriptEntry::AssistantMessage { body } if body == "first second"
    ));
    state.push_user("interrupt");
    state.apply_runtime_event(RuntimeEvent::AssistantDelta {
        text: "third".into(),
    });

    assert_eq!(state.transcript.len(), 3);
    assert!(matches!(
        &state.transcript[1..],
        [
            TranscriptEntry::UserTurn { body: user },
            TranscriptEntry::AssistantMessage { body: assistant },
        ] if user == "interrupt" && assistant == "third"
    ));
}

#[test]
fn runtime_errors_are_appended_to_the_transcript() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.set_thinking();

    state.apply_runtime_event(RuntimeEvent::Error {
        message: "protocol error: malformed bridge output".into(),
    });

    assert_eq!(state.activity(), &ActivityState::Idle);
    assert_eq!(state.transcript.len(), 1);
    assert!(matches!(
        &state.transcript[0],
        TranscriptEntry::Error { body }
            if body == "protocol error: malformed bridge output"
    ));
}

#[test]
fn stable_transcript_prefix_excludes_mutable_assistant_and_tool_entries() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_user("inspect it");
    assert_eq!(state.stable_transcript_end(), 1);

    state.apply_runtime_event(RuntimeEvent::AssistantDelta {
        text: "working".into(),
    });
    assert_eq!(state.stable_transcript_end(), 1);

    state.mark_transcript_committed(1);
    assert_eq!(state.live_transcript().len(), 1);
    assert!(matches!(
        &state.live_transcript()[0],
        TranscriptEntry::AssistantMessage { body } if body == "working"
    ));

    state.apply_runtime_event(RuntimeEvent::TurnCompleted);
    assert_eq!(state.stable_transcript_end(), 2);

    state.apply_runtime_event(tool_delta("call_1", "stdout", "partial"));
    assert_eq!(state.stable_transcript_end(), 2);
    state.apply_runtime_event(RuntimeEvent::ToolCompleted {
        operation_id: OperationId::from("operation_1"),
        result: tool_result("call_1", "complete", "bash"),
    });
    assert_eq!(state.stable_transcript_end(), 3);
}

#[test]
fn committed_transcript_marker_never_moves_backwards() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_user("first");
    state.push_user("second");
    state.mark_transcript_committed(2);
    state.mark_transcript_committed(1);

    assert!(state.live_transcript().is_empty());
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
    assert!(matches!(
        &state.transcript[..],
        [TranscriptEntry::ToolCall(replayed), TranscriptEntry::ToolCall(fresh)]
            if replayed.output == "replayed" && fresh.output == "fresh"
    ));
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
