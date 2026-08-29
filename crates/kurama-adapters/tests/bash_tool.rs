use std::{
    fs,
    future::pending,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use kurama_adapters::BashTool;
use kurama_protocol::{
    KuramaError,
    agent::WriteScope,
    id::{CallId, SessionId},
    policy::ExecutionMode,
    runtime::RuntimeEvent,
    tool::{CommandClass, Operation, ToolContext, ToolInvocation, ToolLimits},
    traits::{BoxFuture, CancelSignal, EventSink, Tool},
};
use tempfile::TempDir;
use tokio::sync::Notify;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn empty() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("workspace");
        fs::create_dir(&root).expect("workspace");
        Self { _temp: temp, root }
    }

    fn path(&self, path: impl AsRef<Path>) -> PathBuf {
        self.root.join(path)
    }

    fn context(&self, limits: ToolLimits) -> ToolContext {
        self.context_with_mode(limits, ExecutionMode::Supervised)
    }

    fn context_with_mode(&self, limits: ToolLimits, mode: ExecutionMode) -> ToolContext {
        ToolContext {
            session_id: SessionId::from("test-session"),
            agent_id: None,
            cwd: self.root.clone(),
            workspace_root: self.root.clone(),
            mode,
            limits,
            write_scope: WriteScope {
                roots: vec![self.root.clone()],
                files: Vec::new(),
            },
        }
    }
}

#[tokio::test]
async fn yolo_uses_external_cwd() {
    let fixture = Fixture::empty();
    let outside = fixture._temp.path().join("outside");
    fs::create_dir(&outside).expect("outside dir");
    let canonical_outside = outside.canonicalize().expect("canonical outside");
    let call = invocation(serde_json::json!({
        "command": "pwd",
        "cwd": outside,
        "timeout_ms": 1000
    }));
    let context = fixture.context_with_mode(limits(), ExecutionMode::Yolo);

    let operation = BashTool::default()
        .classify(&context, &call)
        .expect("classify external cwd");
    assert!(matches!(
        operation,
        Operation::Bash { cwd, .. } if cwd == canonical_outside
    ));

    let result = BashTool::default()
        .execute(context, call, &NeverCancel)
        .await
        .expect("run in external cwd");

    assert_eq!(result.metadata["cwd"].as_str(), canonical_outside.to_str());
    assert_eq!(
        result.metadata["stdout"],
        format!("{}\n", canonical_outside.display())
    );
}

#[tokio::test]
async fn bash_contains_external_cwd_outside_yolo() {
    let fixture = Fixture::empty();
    let outside = fixture._temp.path().join("outside");
    fs::create_dir(&outside).expect("outside dir");

    for mode in [ExecutionMode::Supervised, ExecutionMode::Auto] {
        let call = invocation(serde_json::json!({
            "command": "pwd",
            "cwd": outside,
            "timeout_ms": 1000
        }));
        let error = BashTool::default()
            .execute(
                fixture.context_with_mode(limits(), mode),
                call,
                &NeverCancel,
            )
            .await
            .expect_err("external cwd must remain contained");

        assert!(matches!(error, KuramaError::Policy(_)));
    }
}

#[derive(Default)]
struct NeverCancel;

impl CancelSignal for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(pending())
    }
}

#[derive(Default)]
struct ManualCancel {
    cancelled: AtomicBool,
    notify: Notify,
}

#[derive(Default)]
struct RecordingSink {
    chunks: Mutex<Vec<String>>,
}

impl EventSink for RecordingSink {
    fn emit(&self, event: RuntimeEvent) -> Result<(), KuramaError> {
        if let RuntimeEvent::ToolOutputDelta { chunk, .. } = event {
            self.chunks.lock().unwrap().push(chunk);
        }
        Ok(())
    }
}

impl ManualCancel {
    fn trigger(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

impl CancelSignal for ManualCancel {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            while !self.is_cancelled() {
                self.notify.notified().await;
            }
        })
    }
}

fn invocation(arguments: serde_json::Value) -> ToolInvocation {
    ToolInvocation {
        call_id: CallId::from("call-1"),
        name: "bash".into(),
        arguments,
    }
}

fn limits() -> ToolLimits {
    ToolLimits {
        max_bytes: 65_536,
        max_lines: 2_000,
    }
}

#[tokio::test]
async fn bash_uses_deterministic_environment_and_reports_failed_commands() {
    let fixture = Fixture::empty();
    let call = invocation(serde_json::json!({
        "command": "printf '%s|%s|%s|%s|%s|%s' \"$LC_ALL\" \"$LANG\" \"$TERM\" \"$NO_COLOR\" \"$PAGER\" \"$GIT_PAGER\"; printf err >&2; exit 7",
        "cwd": ".",
        "timeout_ms": 1000
    }));

    let result = BashTool::default()
        .execute(fixture.context(limits()), call, &NeverCancel)
        .await
        .expect("command result");

    assert!(result.is_error);
    assert_eq!(result.metadata["exit_code"], 7);
    assert_eq!(result.metadata["stdout"], "C|C|dumb|1|cat|cat");
    assert_eq!(result.metadata["stderr"], "err");
}

#[tokio::test]
async fn bash_bounds_stdout_and_stderr_with_head_and_tail() {
    let fixture = Fixture::empty();
    let call = invocation(serde_json::json!({
        "command": "printf 'head1\\nhead2\\nmiddle\\ntail1\\ntail2\\n'; printf 'errhead\\nerrmiddle\\nerrtail\\n' >&2",
        "cwd": ".",
        "timeout_ms": 1000
    }));
    let small = ToolLimits {
        max_bytes: 64,
        max_lines: 2,
    };

    let result = BashTool::default()
        .execute(fixture.context(small), call, &NeverCancel)
        .await
        .expect("bounded command");

    assert!(result.truncated);
    assert!(
        result.metadata["stdout"]
            .as_str()
            .unwrap()
            .starts_with("head1")
    );
    assert!(
        result.metadata["stdout"]
            .as_str()
            .unwrap()
            .contains("omitted 19 bytes / 3 lines")
    );
    assert!(
        result.metadata["stdout"]
            .as_str()
            .unwrap()
            .ends_with("tail2\n")
    );
    assert!(
        result.metadata["stderr"]
            .as_str()
            .unwrap()
            .starts_with("errhead")
    );
    assert!(
        result.metadata["stderr"]
            .as_str()
            .unwrap()
            .contains("omitted 10 bytes / 1 lines")
    );
    assert!(
        result.metadata["stderr"]
            .as_str()
            .unwrap()
            .ends_with("errtail\n")
    );
}

#[tokio::test]
async fn bash_stream_events_remain_bounded_after_lossy_utf8_conversion() {
    let fixture = Fixture::empty();
    let sink = Arc::new(RecordingSink::default());
    let tool = BashTool::with_event_sink("/bin/bash", sink.clone());
    let call = invocation(serde_json::json!({
        "command": "printf '\\377%.0s' {1..8000}",
        "cwd": ".",
        "timeout_ms": 1000
    }));

    tool.execute(fixture.context(limits()), call, &NeverCancel)
        .await
        .expect("binary output");

    let chunks = sink.chunks.lock().unwrap();
    assert!(!chunks.is_empty());
    assert!(chunks.iter().all(|chunk| chunk.len() <= 4 * 1024));
}

#[tokio::test]
async fn bash_timeout_and_cancellation_terminate_descendants() {
    let fixture = Fixture::empty();
    let timeout_marker = fixture.path("timeout-child-finished");
    let timeout_call = invocation(serde_json::json!({
        "command": format!("(sleep 0.3; touch '{}') & wait", timeout_marker.display()),
        "cwd": ".",
        "timeout_ms": 30
    }));

    let error = BashTool::default()
        .execute(fixture.context(limits()), timeout_call, &NeverCancel)
        .await
        .expect_err("timeout");
    assert!(matches!(error, KuramaError::Tool(_)), "{error:?}");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!timeout_marker.exists());

    let cancel_marker = fixture.path("cancel-child-finished");
    let cancel_call = invocation(serde_json::json!({
        "command": format!("(sleep 0.3; touch '{}') & wait", cancel_marker.display()),
        "cwd": ".",
        "timeout_ms": 1000
    }));
    let cancel = Arc::new(ManualCancel::default());
    let trigger = Arc::clone(&cancel);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        trigger.trigger();
    });

    let error = BashTool::default()
        .execute(fixture.context(limits()), cancel_call, cancel.as_ref())
        .await
        .expect_err("cancelled");
    assert!(matches!(error, KuramaError::Cancelled));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!cancel_marker.exists());
}

#[tokio::test]
async fn bash_timeout_still_applies_after_the_shell_exits() {
    let fixture = Fixture::empty();
    let marker = fixture.path("detached-child-finished");
    let call = invocation(serde_json::json!({
        "command": format!("(sleep 0.3; touch '{}') &", marker.display()),
        "cwd": ".",
        "timeout_ms": 30
    }));

    let error = BashTool::default()
        .execute(fixture.context(limits()), call, &NeverCancel)
        .await
        .expect_err("detached child must remain inside the command timeout");
    assert!(matches!(error, KuramaError::Tool(_)));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!marker.exists());
}

#[test]
fn bash_classification_is_conservative() {
    let fixture = Fixture::empty();
    let tool = BashTool::default();

    let read = tool
        .classify(
            &fixture.context(limits()),
            &invocation(serde_json::json!({
                "command": "git status --short",
                "cwd": ".",
                "timeout_ms": 1000
            })),
        )
        .expect("classify read");
    assert!(matches!(
        read,
        Operation::Bash {
            class: CommandClass::ReadOnly,
            ..
        }
    ));

    let mutating = tool
        .classify(
            &fixture.context(limits()),
            &invocation(serde_json::json!({
                "command": "rm file",
                "cwd": ".",
                "timeout_ms": 1000
            })),
        )
        .expect("classify mutation");
    assert!(matches!(
        mutating,
        Operation::Bash {
            class: CommandClass::Mutating,
            ..
        }
    ));

    let sed_in_place = tool
        .classify(
            &fixture.context(limits()),
            &invocation(serde_json::json!({
                "command": "sed -i '' s/a/b/ file",
                "cwd": ".",
                "timeout_ms": 1000
            })),
        )
        .expect("classify in-place sed");
    assert!(matches!(
        sed_in_place,
        Operation::Bash {
            class: CommandClass::Mutating,
            ..
        }
    ));

    let unknown = tool
        .classify(
            &fixture.context(limits()),
            &invocation(serde_json::json!({
                "command": "python script.py",
                "cwd": ".",
                "timeout_ms": 1000
            })),
        )
        .expect("classify unknown");
    assert!(matches!(
        unknown,
        Operation::Bash {
            class: CommandClass::Unknown,
            ..
        }
    ));
}

#[tokio::test]
async fn bash_rejects_unknown_fields_and_symlink_cwds() {
    let fixture = Fixture::empty();
    let invalid = invocation(serde_json::json!({
        "command": "pwd",
        "cwd": ".",
        "timeout_ms": 1000,
        "extra": true
    }));
    let error = BashTool::default()
        .execute(fixture.context(limits()), invalid, &NeverCancel)
        .await
        .expect_err("strict schema");
    assert!(matches!(error, KuramaError::Tool(_)));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let outside = fixture._temp.path().join("outside");
        fs::create_dir(&outside).expect("outside dir");
        symlink(&outside, fixture.path("escape")).expect("symlink");
        let escaped = invocation(serde_json::json!({
            "command": "pwd",
            "cwd": "escape",
            "timeout_ms": 1000
        }));
        let error = BashTool::default()
            .execute(fixture.context(limits()), escaped, &NeverCancel)
            .await
            .expect_err("escaped cwd");
        assert!(matches!(error, KuramaError::Policy(_)));
    }
}
