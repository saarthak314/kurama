#![cfg(all(feature = "codex-bridge", feature = "claude-bridge"))]
#![allow(dead_code)]

#[path = "../src/bridges/mod.rs"]
mod bridges;

use std::path::Path;
use std::{fs, time::Instant};

use bridges::{
    claude::ClaudeBridge,
    codex::CodexBridge,
    control::{control_schema, parse_control},
};
use futures_util::StreamExt;
use kurama_protocol::{
    KuramaError,
    id::SessionId,
    model::{BackendCursor, ModelEvent, ModelItem, ModelProfile, ModelRequest},
    traits::{BoxFuture, CancelSignal, ModelBackend},
};

fn request() -> ModelRequest {
    ModelRequest {
        session_id: SessionId::from("session-1"),
        agent_id: None,
        profile: ModelProfile::new("subscription", "frontier", 32_000, 4_000),
        system: "Be exact.".into(),
        items: vec![ModelItem::User {
            text: "Inspect it".into(),
        }],
        tools: Vec::new(),
        delegation: None,
        continuation: None,
    }
}

#[test]
fn codex_initial_command_is_read_only_isolated_and_jsonl() {
    let command = CodexBridge::command_for(
        &request(),
        None,
        Path::new("/tmp/bridge"),
        Path::new("/tmp/control.json"),
    );

    assert_eq!(command.program, "codex");
    assert!(
        command
            .args
            .windows(2)
            .any(|pair| pair == ["--sandbox", "read-only"])
    );
    assert!(command.args.iter().any(|argument| argument == "--json"));
    assert!(
        command
            .args
            .iter()
            .any(|argument| argument == "--ignore-user-config")
    );
    assert!(
        command
            .args
            .iter()
            .any(|argument| argument == "--ignore-rules")
    );
    assert!(
        !command
            .args
            .iter()
            .any(|argument| argument.contains("token"))
    );
}

#[test]
fn codex_resume_command_preserves_thread_cursor() {
    let cursor = BackendCursor {
        backend: "codex_cli".into(),
        value: "thread-1".into(),
    };
    let command = CodexBridge::command_for(
        &request(),
        Some(&cursor),
        Path::new("/tmp/bridge"),
        Path::new("/tmp/control.json"),
    );

    assert_eq!(&command.args[..2], ["exec", "resume"]);
    assert!(command.args.iter().any(|argument| argument == "thread-1"));
}

#[test]
fn claude_command_disables_builtin_tools_and_customization() {
    let command = ClaudeBridge::command_for(&request(), None, Path::new("/tmp/control.json"));

    assert_eq!(command.program, "claude");
    assert!(command.args.windows(2).any(|pair| pair == ["--tools", ""]));
    assert!(
        command
            .args
            .iter()
            .any(|argument| argument == "--safe-mode")
    );
    assert!(
        command
            .args
            .iter()
            .any(|argument| argument == "stream-json")
    );
    assert!(
        command
            .args
            .iter()
            .any(|argument| argument == "--strict-mcp-config")
    );
    assert_eq!(command.stdin, request_prompt_marker());
}

fn request_prompt_marker() -> String {
    bridges::control::bridge_prompt(&request())
}

#[test]
fn control_schema_omits_delegation_when_disabled() {
    let disabled = control_schema(false);
    let enabled = control_schema(true);

    assert_eq!(disabled["oneOf"].as_array().expect("branches").len(), 2);
    assert_eq!(enabled["oneOf"].as_array().expect("branches").len(), 3);
}

#[test]
fn strict_control_parser_normalizes_tools_and_delegation() {
    let tools = parse_control(r#"{"kind":"tool_calls","calls":[{"call_id":"c1","name":"read","arguments":{"files":[]}}]}"#, true)
        .expect("tools");
    assert!(matches!(tools.as_slice(), [ModelEvent::ToolCall { name, .. }] if name == "read"));

    let delegation = parse_control(
        r#"{"kind":"delegate","agents":[{"objective":"Review","write_roots":[],"write_files":[],"depends_on":[]}]}"#,
        true,
    )
    .expect("delegation");
    assert!(matches!(
        delegation.as_slice(),
        [ModelEvent::Delegation { .. }]
    ));
    assert!(parse_control(r#"{"kind":"delegate","agents":[]}"#, false).is_err());
}

#[test]
fn codex_and_claude_jsonl_fixtures_normalize() {
    let codex = CodexBridge::parse_fixture(include_str!(
        "../../../tests/fixtures/codex/tool_turn.jsonl"
    ))
    .expect("codex fixture");
    let claude = ClaudeBridge::parse_fixture(include_str!(
        "../../../tests/fixtures/claude/tool_turn.jsonl"
    ))
    .expect("claude fixture");

    for events in [&codex, &claude] {
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::ToolCall { name, .. } if name == "read"))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::Usage { .. }))
        );
        assert!(matches!(
            events.last(),
            Some(ModelEvent::ResponseCompleted { .. })
        ));
    }
}

#[tokio::test]
async fn fake_codex_process_streams_normalized_events() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let executable = temporary.path().join("codex");
    write_executable(
        &executable,
        r#"#!/bin/sh
printf '%s\n' '{"type":"thread.started","thread_id":"thread-live"}'
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"{\"kind\":\"final\",\"text\":\"done\"}"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":4,"output_tokens":2,"cached_input_tokens":1}}'
"#,
    );
    let bridge = CodexBridge::new(
        temporary.path().join("work"),
        temporary.path().join("control.json"),
    )
    .with_program(executable.display().to_string());

    let mut stream = bridge
        .stream(request(), &NeverCancel)
        .await
        .expect("bridge stream");
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.expect("event"));
    }

    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::TextDelta { text } if text == "done"))
    );
    assert!(matches!(
        events.last(),
        Some(ModelEvent::ResponseCompleted { .. })
    ));
}

#[tokio::test]
async fn bridge_cancellation_terminates_without_waiting_for_child() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let executable = temporary.path().join("claude");
    write_executable(&executable, "#!/bin/sh\nsleep 10\n");
    let bridge = ClaudeBridge::new(temporary.path().join("control.json"))
        .with_program(executable.display().to_string());
    let started = Instant::now();

    let result = bridge.stream(request(), &DelayedCancel).await;

    assert!(matches!(result, Err(KuramaError::Cancelled)));
    assert!(started.elapsed().as_secs() < 2);
}

#[tokio::test]
async fn bridge_nonzero_errors_are_bounded_and_redacted() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let executable = temporary.path().join("claude");
    write_executable(
        &executable,
        "#!/bin/sh\nprintf 'secret-value' >&2\nexit 7\n",
    );
    let bridge = ClaudeBridge::new(temporary.path().join("control.json"))
        .with_program(executable.display().to_string())
        .with_redactions(vec!["secret-value".into()]);

    let error = match bridge.stream(request(), &NeverCancel).await {
        Ok(_) => panic!("nonzero process unexpectedly succeeded"),
        Err(error) => error,
    };
    let message = error.to_string();

    assert!(!message.contains("secret-value"));
    assert!(message.contains("[REDACTED]"));
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("chmod executable");
    }
}

struct NeverCancel;

impl CancelSignal for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

struct DelayedCancel;

impl CancelSignal for DelayedCancel {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep(std::time::Duration::from_millis(50)))
    }
}
