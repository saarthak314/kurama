#![cfg(all(feature = "codex-bridge", feature = "claude-bridge"))]
#![allow(dead_code)]

#[path = "../src/bridges/mod.rs"]
mod bridges;

use std::path::Path;
use std::{
    fs,
    time::{Duration, Instant},
};

use bridges::{
    BridgeDecoder, InactivityWatchdog, bounded_stderr_diagnostic,
    claude::ClaudeBridge,
    codex::CodexBridge,
    control::{bridge_prompt, control_schema, parse_control},
    event_stream_with_inactivity, inactivity_error,
};
use futures_util::StreamExt;
use kurama_protocol::{
    KuramaError,
    id::SessionId,
    model::{BackendCursor, ModelEvent, ModelItem, ModelProfile, ModelRequest},
    tool::ToolDescriptor,
    traits::{BoxFuture, CancelSignal, ModelBackend},
};

fn request() -> ModelRequest {
    ModelRequest {
        session_id: SessionId::from("session-1"),
        agent_id: None,
        workspace_root: "/workspace/project".into(),
        profile: ModelProfile::new("subscription", "frontier", 32_000, 4_000),
        system: "Be exact.".into(),
        items: vec![ModelItem::User {
            text: "Inspect it".into(),
        }],
        tools: vec![ToolDescriptor {
            name: "bash".into(),
            description: "Run one bounded Bash command in the workspace.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "required": ["command", "cwd", "timeout_ms"]
            }),
        }],
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
    for feature in [
        "apps",
        "browser_use",
        "computer_use",
        "image_generation",
        "multi_agent",
        "shell_tool",
        "unified_exec",
        "view_image",
    ] {
        assert!(
            command
                .args
                .windows(2)
                .any(|pair| pair == ["--disable", feature]),
            "native Codex feature remained enabled: {feature}"
        );
    }
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
fn bridge_prompt_anchors_relative_tools_to_the_kurama_workspace() {
    let prompt = bridge_prompt(&request());

    assert!(prompt.contains("Kurama workspace root: \"/workspace/project\""));
    assert!(prompt.contains("never use the bridge process working directory"));
}

#[test]
fn bridge_prompt_does_not_invent_tool_unavailability() {
    let prompt = bridge_prompt(&request());

    assert!(prompt.contains("listed Kurama tool is available through this control protocol"));
    assert!(prompt.contains("CLI tools are deliberately disabled and irrelevant"));
    assert!(prompt.contains("Kurama protocol operations, not CLI tools"));
    assert!(prompt.contains("Kurama executes it after this response"));
    assert!(prompt.contains("Do not claim any operation ran, failed, or was unavailable"));
    assert!(prompt.contains("return kind=tool_calls instead of kind=final"));
    assert!(prompt.contains("nonzero exit status"));
    assert!(prompt.contains("is_error=true"));
    assert!(prompt.contains("not evidence that the tool is missing or unavailable"));
    assert!(prompt.contains("only from explicit tool-result content or an engine error"));
    assert!(prompt.contains("Never invent an unavailable-tool failure"));
}

#[test]
fn bridge_prompt_preserves_request_tail_beyond_legacy_limit() {
    let mut request = request();
    let tail_marker = "complete-request-tail-marker";
    request.items = vec![ModelItem::User {
        text: format!("{}{}", "x".repeat(256 * 1024), tail_marker),
    }];

    let prompt = bridge_prompt(&request);

    assert!(prompt.len() > 256 * 1024);
    assert!(prompt.contains(tail_marker));
}

#[test]
fn bridge_prompt_wrapper_fits_the_core_envelope_reserve() {
    let request = request();
    let serialized_request = serde_json::to_vec(&request).expect("serialize request");
    let prompt = bridge_prompt(&request);

    assert!(prompt.len() <= serialized_request.len() + 512 * 3);
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
    assert!(
        command
            .args
            .windows(2)
            .any(|pair| pair == ["--disable", "shell_tool"])
    );
}

#[test]
fn codex_rejects_native_tool_execution_events() {
    let error = CodexBridge::parse_fixture(concat!(
        r#"{"type":"thread.started","thread_id":"thread-1"}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item-1","type":"command_execution","command":"pwd","aggregated_output":"/workspace\n","exit_code":0,"status":"completed"}}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":5,"cached_input_tokens":0}}"#,
    ))
    .expect_err("native tool execution must fail closed");

    assert!(error.to_string().contains("native Codex tool"), "{error}");
}

#[test]
fn claude_command_uses_stream_json_and_disables_builtin_tools() {
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
            .windows(2)
            .any(|pair| pair == ["--output-format", "stream-json"])
    );
    assert!(command.args.iter().any(|argument| argument == "--verbose"));
    assert!(
        !command
            .args
            .iter()
            .any(|argument| argument == "--include-partial-messages")
    );
    assert!(
        command
            .args
            .iter()
            .any(|argument| argument == "--strict-mcp-config")
    );
    let system_prompt = command
        .args
        .windows(2)
        .find(|pair| pair[0] == "--system-prompt")
        .map(|pair| pair[1].as_str())
        .expect("Claude bridge system prompt");
    assert!(system_prompt.contains("You are a model bridge"));
    assert!(!system_prompt.contains("Operate only through the supplied tools"));
    assert!(system_prompt.contains("only Claude tool you may invoke is StructuredOutput"));
    assert!(system_prompt.contains("Every listed Kurama tool is available"));
    assert!(system_prompt.contains("Kurama executes it after this response"));
    assert!(command.stdin.starts_with("Active context:\n"));
    assert!(!command.stdin.contains("You are a model bridge"));
}

#[test]
fn claude_single_result_json_normalizes_tool_calls() {
    let events = ClaudeBridge::parse_fixture(
        r#"{"type":"result","subtype":"success","session_id":"session-1","structured_output":{"kind":"tool_calls","text":"","calls":[{"call_id":"c1","name":"bash","arguments":"{\"command\":\"pwd\",\"cwd\":\"/workspace/project\",\"timeout_ms\":10000}"}],"agents":[]},"usage":{"input_tokens":90,"output_tokens":16,"cache_read_input_tokens":70}}"#,
    )
    .expect("Claude result JSON");

    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::ToolCall { name, .. } if name == "bash"))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        ModelEvent::ResponseCompleted {
            cursor: Some(BackendCursor { backend, value }),
            finish_reason: kurama_protocol::model::FinishReason::ToolCalls,
        } if backend == "claude_cli" && value == "session-1"
    )));
}

#[test]
fn claude_recovers_protocol_calls_misrouted_as_native_tool_use() {
    let events = ClaudeBridge::parse_fixture(concat!(
        r#"{"type":"system","subtype":"init","session_id":"session-1"}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"bash","input":{"call_id":"c1","arguments":"{\"command\":\"pwd\",\"cwd\":\"/workspace/project\",\"timeout_ms\":10000}"}}]}}"#,
        "\n",
        r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"<tool_use_error>Error: No such tool available: bash</tool_use_error>","is_error":true,"tool_use_id":"toolu_1"}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","session_id":"session-1","structured_output":{"kind":"final","text":"bash is unavailable","calls":[],"agents":[]}}"#,
    ))
    .expect("Claude stream JSON");

    assert!(matches!(
        events.as_slice(),
        [
            ModelEvent::ResponseStarted { .. },
            ModelEvent::ToolCall { name, .. },
            ModelEvent::ResponseCompleted {
                finish_reason: kurama_protocol::model::FinishReason::ToolCalls,
                ..
            }
        ] if name == "bash"
    ));
}

#[test]
fn claude_recovers_direct_native_tool_arguments_without_resuming_poisoned_history() {
    let events = ClaudeBridge::parse_fixture(concat!(
        r#"{"type":"system","subtype":"init","session_id":"session-1"}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"bash","input":{"command":"pwd","cwd":"/workspace/project","timeout_ms":10000}}]}}"#,
        "\n",
        r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"<tool_use_error>Error: No such tool available: bash</tool_use_error>","is_error":true,"tool_use_id":"toolu_1"}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","session_id":"session-1","structured_output":{"kind":"final","text":"bash is unavailable","calls":[],"agents":[]}}"#,
    ))
    .expect("Claude stream JSON");

    assert!(matches!(
        events.as_slice(),
        [
            ModelEvent::ResponseStarted { .. },
            ModelEvent::ToolCall {
                name,
                arguments,
                ..
            },
            ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: kurama_protocol::model::FinishReason::ToolCalls,
            }
        ] if name == "bash" && arguments["command"] == "pwd"
    ));
}

#[test]
fn claude_recovers_structured_output_tool_blocks_with_code_fences() {
    let events = ClaudeBridge::parse_fixture(concat!(
        r#"{"type":"system","subtype":"init","session_id":"session-1"}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"structured_1","name":"StructuredOutput","input":{"kind":"final","text":"```rust\nfn main() { println!(\"hello\"); }\n```","calls":[],"agents":[]}}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","session_id":"session-1","result":""}"#,
    ))
    .expect("Claude stream JSON");

    assert!(matches!(
        events.as_slice(),
        [
            ModelEvent::ResponseStarted { .. },
            ModelEvent::TextDelta { text },
            ModelEvent::ResponseCompleted {
                finish_reason: kurama_protocol::model::FinishReason::Stop,
                ..
            }
        ] if text == "```rust\nfn main() { println!(\"hello\"); }\n```"
    ));
}

#[test]
fn claude_surfaces_assistant_error_records_as_model_failures() {
    let error = ClaudeBridge::parse_fixture(concat!(
        r#"{"type":"system","subtype":"init","session_id":"session-1"}"#,
        "\n",
        r#"{"type":"assistant","error":"authentication_failed","message":{"content":[{"type":"text","text":"Failed to authenticate: OAuth session expired"}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","session_id":"session-1","result":"Failed to authenticate: OAuth session expired","structured_output":null}"#,
    ))
    .expect_err("Claude assistant error must fail the model request");

    assert_eq!(
        error.to_string(),
        "model error: Failed to authenticate: OAuth session expired"
    );
}

#[test]
fn control_schema_omits_delegation_when_disabled() {
    let disabled = control_schema(false);
    let enabled = control_schema(true);

    assert!(disabled.get("oneOf").is_none());
    assert!(disabled.get("$schema").is_none());
    assert_eq!(disabled["type"], "object");
    assert_eq!(
        disabled["properties"]["kind"]["enum"],
        serde_json::json!(["final", "tool_calls"])
    );
    assert_eq!(
        enabled["properties"]["kind"]["enum"],
        serde_json::json!(["final", "tool_calls", "delegate"])
    );
    assert_eq!(
        enabled["required"],
        serde_json::json!(["kind", "text", "calls", "agents"])
    );
}

#[test]
fn strict_control_parser_normalizes_tools_and_delegation() {
    let tools = parse_control(r#"{"kind":"tool_calls","text":"","calls":[{"call_id":"c1","name":"read","arguments":"{\"files\":[]}"}],"agents":[]}"#, true)
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
    assert!(
        parse_control(
            r#"{"kind":"final","text":"","calls":[],"agents":[{"objective":"stale","write_roots":[],"write_files":[],"depends_on":[]}]}"#,
            true,
        )
        .is_err()
    );
    let commentary = parse_control(
        r#"{"kind":"tool_calls","text":"I'll read it","calls":[{"call_id":"c1","name":"read","arguments":"{\"files\":[]}"}],"agents":[]}"#,
        true,
    )
    .expect("ignore commentary text on tool_calls");
    assert!(matches!(commentary.as_slice(), [ModelEvent::ToolCall { name, .. }] if name == "read"));
}

#[test]
fn control_parser_recovers_literal_backslashes_inside_strings() {
    let final_events = parse_control(
        r#"{"kind":"final","text":"Use \_emphasis\_ and \`code\`.","calls":[],"agents":[]}"#,
        false,
    )
    .expect("final control");
    assert!(matches!(
        final_events.as_slice(),
        [ModelEvent::TextDelta { text }] if text == r"Use \_emphasis\_ and \`code\`."
    ));

    let tool_events = parse_control(
        r#"{"kind":"tool_calls","text":"","calls":[{"call_id":"c1","name":"bash","arguments":"{\"command\":\"printf \\q\"}"}],"agents":[]}"#,
        false,
    )
    .expect("tool control");
    assert!(matches!(
        tool_events.as_slice(),
        [ModelEvent::ToolCall { arguments, .. }] if arguments["command"] == r"printf \q"
    ));

    assert!(
        parse_control(
            r#"{"kind":"final","text":"bad \q","calls":[],"agents":[],"extra":true}"#,
            false,
        )
        .is_err()
    );
}

#[test]
fn control_parser_recovers_literal_control_characters_in_code_blocks() {
    let control = concat!(
        "{\"kind\":\"final\",\"text\":\"```rust\n",
        "fn main() {\n",
        "\tprintln!(\\\"hello\\\");\n",
        "}\n",
        "```\",\"calls\":[],\"agents\":[]}",
    );

    let events = parse_control(control, false).expect("multiline final control");

    assert!(matches!(
        events.as_slice(),
        [ModelEvent::TextDelta { text }]
            if text == "```rust\nfn main() {\n\tprintln!(\"hello\");\n}\n```"
    ));
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
        Some(ModelEvent::ResponseCompleted {
            cursor: Some(BackendCursor { backend, value }),
            ..
        }) if backend == "codex_cli" && value == "thread-live"
    ));
}

#[tokio::test]
async fn long_completed_bridge_preserves_final_despite_late_nonzero_exit() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let executable = temporary.path().join("codex");
    write_executable(
        &executable,
        r#"#!/bin/sh
printf '%s\n' '{"type":"thread.started","thread_id":"thread-live"}'
index=0
while [ "$index" -lt 128 ]; do
  printf '%s\n' '{"type":"item.completed","item":{"type":"reasoning","text":"working"}}'
  index=$((index + 1))
done
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"{\"kind\":\"final\",\"text\":\"done\",\"calls\":[],\"agents\":[]}"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":4,"output_tokens":2,"cached_input_tokens":1}}'
exit 7
"#,
    );
    let bridge = CodexBridge::new(
        temporary.path().join("work"),
        temporary.path().join("control.json"),
    )
    .with_program(executable.display().to_string());

    let stream = bridge
        .stream(request(), &NeverCancel)
        .await
        .expect("bridge stream");
    let events = stream.collect::<Vec<_>>().await;

    assert!(events.iter().all(Result::is_ok), "{events:?}");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Ok(ModelEvent::TextDelta { text }) if text == "done"))
    );
    assert!(matches!(
        events.last(),
        Some(Ok(ModelEvent::ResponseCompleted { .. }))
    ));
}

#[tokio::test]
async fn codex_terminal_completion_ends_before_cli_eof() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let executable = temporary.path().join("codex");
    write_executable(
        &executable,
        r#"#!/bin/sh
printf '%s\n' '{"type":"thread.started","thread_id":"thread-live"}'
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"{\"kind\":\"final\",\"text\":\"done\",\"calls\":[],\"agents\":[]}"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":4,"output_tokens":2,"cached_input_tokens":1}}'
sleep 10
"#,
    );
    let bridge = CodexBridge::new(
        temporary.path().join("work"),
        temporary.path().join("control.json"),
    )
    .with_program(executable.display().to_string());
    let stream = bridge
        .stream(request(), &NeverCancel)
        .await
        .expect("bridge stream");

    let events = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        stream.collect::<Vec<_>>(),
    )
    .await
    .expect("terminal stream waited for CLI EOF");

    assert!(matches!(
        events.last(),
        Some(Ok(ModelEvent::ResponseCompleted { .. }))
    ));
}

#[tokio::test]
async fn silent_codex_bridge_times_out_after_inactivity() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let executable = temporary.path().join("codex");
    write_executable(&executable, "#!/bin/sh\nsleep 10\n");
    let mut command = CodexBridge::command_for(
        &request(),
        None,
        &temporary.path().join("work"),
        &temporary.path().join("control.json"),
    );
    command.program = executable.display().to_string();

    let result = tokio::time::timeout(
        Duration::from_millis(750),
        event_stream_with_inactivity(
            command,
            TestBridgeDecoder::default(),
            &NeverCancel,
            Vec::new(),
            Duration::from_millis(100),
        ),
    )
    .await
    .expect("silent bridge watchdog timed out");
    let error = match result {
        Ok(_) => panic!("silent bridge unexpectedly succeeded"),
        Err(error) => error,
    };
    let message = error.to_string();

    assert!(message.contains("inactive"), "{message}");
    assert!(message.contains("no diagnostic output"), "{message}");
    assert!(message.len() <= 16 * 1024 + 128, "{message}");
}

#[cfg(unix)]
#[tokio::test]
async fn bridge_stdout_eof_times_out_and_kills_the_process_group() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let executable = temporary.path().join("codex");
    let pids = temporary.path().join("pids");
    write_executable(
        &executable,
        &format!(
            r#"#!/bin/sh
trap '' TERM
sh -c 'trap "" TERM; while :; do sleep 1; done' >/dev/null 2>&1 &
descendant=$!
printf '%s %s\n' "$$" "$descendant" > '{}'
printf '%s\n' '{{"type":"heartbeat"}}'
exec >/dev/null
printf '%s\n' 'secret-value' >&2
sleep 10
"#,
            pids.display()
        ),
    );
    let mut command = CodexBridge::command_for(
        &request(),
        None,
        &temporary.path().join("work"),
        &temporary.path().join("control.json"),
    );
    command.program = executable.display().to_string();

    let bridge_task = tokio::spawn(async move {
        event_stream_with_inactivity(
            command,
            TestBridgeDecoder::default(),
            &NeverCancel,
            vec!["secret-value".into()],
            Duration::from_secs(3),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !pids.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("bridge process did not start");
    let result = tokio::time::timeout(Duration::from_secs(4), bridge_task)
        .await
        .expect("bridge waited forever after stdout EOF")
        .expect("bridge task panicked");
    let error = match result {
        Ok(_) => panic!("incomplete bridge unexpectedly succeeded"),
        Err(error) => error,
    };
    let message = error.to_string();

    assert!(message.contains("inactive"), "{message}");
    assert!(message.contains("[REDACTED]"), "{message}");
    assert!(!message.contains("secret-value"), "{message}");
    assert!(message.len() <= 16 * 1024 + 128, "{message}");

    let contents = fs::read_to_string(&pids).expect("process IDs");
    let mut process_ids = contents
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("numeric process ID"));
    let leader = process_ids.next().expect("leader process ID");
    let descendant = process_ids.next().expect("descendant process ID");
    let descendant_gone = wait_for_process_exit(descendant).await;
    if !descendant_gone {
        unsafe {
            libc::kill(-leader, libc::SIGKILL);
        }
    }
    assert!(
        descendant_gone,
        "bridge descendant survived stdout-EOF timeout"
    );
}

#[test]
fn bridge_inactivity_diagnostics_are_bounded_and_redacted() {
    let secret = "secret-value";
    let diagnostic = format!("{secret} {}", "x".repeat(20_000));
    let error = inactivity_error(
        "codex",
        Duration::from_secs(2),
        &diagnostic,
        &[secret.into()],
    );
    let message = error.to_string();

    assert!(message.contains("inactive"), "{message}");
    assert!(message.contains("[REDACTED]"), "{message}");
    assert!(!message.contains("secret-value"), "{message}");
    assert!(message.len() <= 16 * 1024 + 128, "{message}");
}

#[tokio::test]
async fn bridge_inactivity_watchdog_resets_on_each_nonempty_record() {
    let inactivity = Duration::from_millis(200);
    let mut watchdog = InactivityWatchdog::new(inactivity);
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(120)).await;
            sender
                .send(r#"{"type":"heartbeat"}"#)
                .expect("watchdog receiver");
        }
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        for _ in 0..4 {
            tokio::select! {
                _ = watchdog.wait() => panic!("active JSONL stream timed out"),
                record = receiver.recv() => {
                    assert!(watchdog.observe_record(record.expect("heartbeat record")));
                }
            }
        }
    })
    .await
    .expect("watchdog reset test timed out");
    assert!(!watchdog.observe_record(" \n"));
}

#[tokio::test]
async fn bridge_inactivity_does_not_wait_forever_for_stderr() {
    let mut stderr_task = tokio::spawn(std::future::pending::<Result<String, KuramaError>>());
    let started = Instant::now();

    let diagnostic =
        bounded_stderr_diagnostic("codex", &mut stderr_task, Duration::from_millis(25)).await;

    assert_eq!(diagnostic, None);
    assert!(started.elapsed() < Duration::from_millis(250));
    let join_error = tokio::time::timeout(Duration::from_millis(100), &mut stderr_task)
        .await
        .expect("aborted stderr task remained pending")
        .expect_err("stderr task unexpectedly completed");
    assert!(join_error.is_cancelled());
}

#[tokio::test]
async fn codex_stream_yields_before_process_exit() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let executable = temporary.path().join("codex");
    let completed = temporary.path().join("completed");
    write_executable(
        &executable,
        &format!(
            r#"#!/bin/sh
printf '%s\n' '{{"type":"thread.started","thread_id":"thread-live"}}'
sleep 1
: > '{}'
printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"kind\":\"final\",\"text\":\"done\"}}"}}}}'
printf '%s\n' '{{"type":"turn.completed","usage":{{"input_tokens":4,"output_tokens":2,"cached_input_tokens":1}}}}'
"#,
            completed.display()
        ),
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
    let first = tokio::time::timeout(std::time::Duration::from_millis(500), stream.next())
        .await
        .expect("first event before process exit")
        .expect("first event")
        .expect("valid event");

    assert!(matches!(first, ModelEvent::ResponseStarted { .. }));
    assert!(
        !completed.exists(),
        "process exited before the event was yielded"
    );
}

#[tokio::test]
async fn claude_stream_yields_before_process_exit() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let executable = temporary.path().join("claude");
    let completed = temporary.path().join("completed");
    write_executable(
        &executable,
        &format!(
            r#"#!/bin/sh
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"session-live"}}'
sleep 1
: > '{}'
printf '%s\n' '{{"type":"result","subtype":"success","session_id":"session-live","structured_output":{{"kind":"final","text":"done","calls":[],"agents":[]}},"usage":{{"input_tokens":4,"output_tokens":2,"cache_read_input_tokens":1}}}}'
"#,
            completed.display()
        ),
    );
    let bridge = ClaudeBridge::new(temporary.path().join("control.json"))
        .with_program(executable.display().to_string());

    let mut stream = bridge
        .stream(request(), &NeverCancel)
        .await
        .expect("bridge stream");
    let first = tokio::time::timeout(std::time::Duration::from_millis(500), stream.next())
        .await
        .expect("first event before process exit")
        .expect("first event")
        .expect("valid event");

    assert!(matches!(first, ModelEvent::ResponseStarted { .. }));
    assert!(
        !completed.exists(),
        "process exited before the event was yielded"
    );
}

#[tokio::test]
async fn bridge_rejects_oversized_unterminated_record_promptly() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let executable = temporary.path().join("codex");
    write_executable(
        &executable,
        r#"#!/bin/sh
dd if=/dev/zero bs=1048576 count=16 2>/dev/null
sleep 10
"#,
    );
    let bridge = CodexBridge::new(
        temporary.path().join("work"),
        temporary.path().join("control.json"),
    )
    .with_program(executable.display().to_string());

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        bridge.stream(request(), &NeverCancel),
    )
    .await
    .expect("oversized record rejection timed out");

    assert!(
        matches!(result, Err(KuramaError::Protocol(message)) if message.contains("oversized JSONL record"))
    );
}

#[tokio::test]
async fn bridge_drains_stderr_after_diagnostic_cap() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let executable = temporary.path().join("codex");
    write_executable(
        &executable,
        r#"#!/bin/sh
set -e
i=0
while [ "$i" -lt 6000 ]; do
  printf '%080d\n' "$i" >&2
  i=$((i + 1))
done
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
    let mut completed = false;
    while let Some(event) = stream.next().await {
        completed |= matches!(
            event.expect("valid event"),
            ModelEvent::ResponseCompleted { .. }
        );
    }

    assert!(completed);
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

#[cfg(unix)]
#[tokio::test]
async fn dropping_bridge_stream_kills_term_resistant_process_group() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let executable = temporary.path().join("claude");
    let pids = temporary.path().join("pids");
    write_executable(
        &executable,
        &format!(
            r#"#!/bin/sh
trap '' TERM
sh -c 'trap "" TERM; while :; do sleep 1; done' &
descendant=$!
printf '%s %s\n' "$$" "$descendant" > '{}'
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"session-live"}}'
while :; do sleep 1; done
"#,
            pids.display()
        ),
    );
    let bridge = ClaudeBridge::new(temporary.path().join("control.json"))
        .with_program(executable.display().to_string());

    let mut stream = bridge
        .stream(request(), &NeverCancel)
        .await
        .expect("bridge stream");
    let first = stream
        .next()
        .await
        .expect("first event")
        .expect("valid event");
    assert!(matches!(first, ModelEvent::ResponseStarted { .. }));
    drop(stream);

    let contents = fs::read_to_string(&pids).expect("process IDs");
    let mut process_ids = contents
        .split_whitespace()
        .map(|value| value.parse::<i32>().expect("numeric process ID"));
    let leader = process_ids.next().expect("leader process ID");
    let descendant = process_ids.next().expect("descendant process ID");
    let descendant_gone = wait_for_process_exit(descendant).await;
    if !descendant_gone {
        unsafe {
            libc::kill(-leader, libc::SIGKILL);
        }
    }

    assert!(
        descendant_gone,
        "TERM-resistant descendant survived stream cancellation"
    );
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

#[tokio::test]
async fn bridge_nonzero_errors_fall_back_to_jsonl_stdout() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let executable = temporary.path().join("codex");
    write_executable(
        &executable,
        r#"#!/bin/sh
printf '%s\n' '{"type":"error","message":"invalid_json_schema: oneOf is not permitted"}'
printf '%s\n' '{"type":"turn.failed","error":{"message":"invalid_json_schema: oneOf is not permitted"}}'
exit 1
"#,
    );
    let bridge = CodexBridge::new(
        temporary.path().join("work"),
        temporary.path().join("control.json"),
    )
    .with_program(executable.display().to_string());

    let error = match bridge.stream(request(), &NeverCancel).await {
        Ok(_) => panic!("nonzero process unexpectedly succeeded"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("invalid_json_schema"));
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

#[derive(Default)]
struct TestBridgeDecoder {
    completed: bool,
}

impl BridgeDecoder for TestBridgeDecoder {
    fn push_line(&mut self, line: &str) -> Result<Vec<ModelEvent>, KuramaError> {
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|error| KuramaError::Protocol(format!("invalid test JSONL: {error}")))?;
        if value.get("type").and_then(serde_json::Value::as_str) != Some("complete") {
            return Ok(Vec::new());
        }
        self.completed = true;
        Ok(vec![ModelEvent::ResponseCompleted {
            cursor: None,
            finish_reason: kurama_protocol::model::FinishReason::Stop,
        }])
    }

    fn finish(&self) -> Result<(), KuramaError> {
        if self.completed {
            Ok(())
        } else {
            Err(KuramaError::Protocol(
                "test JSONL ended before completion".into(),
            ))
        }
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

#[cfg(unix)]
async fn wait_for_process_exit(process_id: i32) -> bool {
    for _ in 0..50 {
        if unsafe { libc::kill(process_id, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    false
}
