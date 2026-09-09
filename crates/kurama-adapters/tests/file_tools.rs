use std::{
    fs,
    future::pending,
    path::{Path, PathBuf},
};

use kurama_adapters::{BoundedOutput, ReadTool, WriteTool, html_to_text};
use kurama_protocol::{
    KuramaError,
    agent::WriteScope,
    id::{AgentId, CallId, SessionId},
    policy::ExecutionMode,
    tool::{Operation, ToolContext, ToolInvocation, ToolLimits},
    traits::{BoxFuture, CancelSignal, Tool},
};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
}

#[test]
fn read_descriptor_requires_complete_line_or_byte_ranges() {
    let descriptor = ReadTool::default().descriptor();
    let variants = descriptor.parameters["properties"]["files"]["items"]["oneOf"]
        .as_array()
        .expect("read range variants");

    assert_eq!(variants.len(), 2);
    assert!(
        variants.iter().any(|variant| {
            variant["required"] == serde_json::json!(["start_line", "end_line"])
        })
    );
    assert!(
        variants.iter().any(|variant| {
            variant["required"] == serde_json::json!(["start_byte", "end_byte"])
        })
    );
}

impl Fixture {
    fn empty() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("workspace");
        fs::create_dir(&root).expect("workspace");
        Self { _temp: temp, root }
    }

    fn with_file(path: &str, bytes: &[u8]) -> Self {
        let fixture = Self::empty();
        let target = fixture.path(path);
        fs::create_dir_all(target.parent().expect("parent")).expect("parent dirs");
        fs::write(target, bytes).expect("fixture file");
        fixture
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

fn invocation(name: &str, arguments: serde_json::Value) -> ToolInvocation {
    ToolInvocation {
        call_id: CallId::from("call-1"),
        name: name.into(),
        arguments,
    }
}

fn normal_limits() -> ToolLimits {
    ToolLimits {
        max_bytes: 65_536,
        max_lines: 2_000,
    }
}

#[tokio::test]
async fn read_requires_an_explicit_range_and_returns_file_metadata() {
    let fixture = Fixture::with_file("src/lib.rs", b"one\ntwo\nthree\n");
    let call = invocation(
        "read",
        serde_json::json!({
            "files": [{"path": "src/lib.rs", "start_line": 2, "end_line": 3}]
        }),
    );

    let result = ReadTool::default()
        .execute(fixture.context(normal_limits()), call, &NeverCancel)
        .await
        .expect("read range");

    assert!(result.output.contains("two\nthree\n"));
    assert_eq!(
        result.metadata["files"][0]["sha256"],
        "b6285c57e8797db5d4c51c80d6f11938afda9b11c6a003549709189e9b4b92a2"
    );
    assert_eq!(result.metadata["files"][0]["total_bytes"], 14);
    assert_eq!(result.metadata["files"][0]["utf8"], true);
    assert!(result.metadata.get("_display_staging").is_none());

    let missing_end = invocation(
        "read",
        serde_json::json!({"files": [{"path": "src/lib.rs", "start_line": 1}]}),
    );
    let error = ReadTool::default()
        .execute(fixture.context(normal_limits()), missing_end, &NeverCancel)
        .await
        .expect_err("partial ranges are invalid");
    assert!(matches!(error, KuramaError::Tool(_)));
}

#[tokio::test]
async fn read_supports_binary_byte_ranges_and_bounds_visible_output() {
    let fixture = Fixture::with_file("bytes.bin", b"HEAD\xffmiddle\nTAIL\n");
    let call = invocation(
        "read",
        serde_json::json!({
            "files": [{"path": "bytes.bin", "start_byte": 0, "end_byte": 17}]
        }),
    );
    let limits = ToolLimits {
        max_bytes: 10,
        max_lines: 2,
    };

    let result = ReadTool::default()
        .execute(fixture.context(limits), call, &NeverCancel)
        .await
        .expect("binary read");

    assert!(result.truncated);
    assert_eq!(result.metadata["files"][0]["utf8"], false);
    assert_eq!(result.metadata["files"][0]["lossy"], true);
    assert!(
        result.metadata["files"][0]["omitted_bytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    let staged_path = PathBuf::from(
        result.metadata["_display_staging"]["output"]
            .as_str()
            .expect("staged read output"),
    );
    assert_eq!(
        fs::read(&staged_path).expect("complete staged read output"),
        b"== bytes.bin ==\nHEAD\xffmiddle\nTAIL\n"
    );
    fs::remove_file(staged_path).expect("remove staged read output");

    let mut bounded = BoundedOutput::new(ToolLimits {
        max_bytes: 64,
        max_lines: 2,
    });
    bounded.push(b"head1\nhead2\nmiddle\ntail1\ntail2\n");
    let bounded = bounded.finish();
    assert!(bounded.truncated);
    assert!(bounded.text.starts_with("head1"));
    assert!(bounded.text.contains("omitted 19 bytes / 3 lines"));
    assert!(bounded.text.ends_with("tail2\n"));
    assert!(bounded.text.len() <= 64);
}

#[test]
fn bounded_output_marks_omissions_without_truncating_the_staged_blob() {
    let temp = tempfile::tempdir().expect("tempdir");
    let staged_path = temp.path().join("capture.blob");
    let full_output = b"head1\nhead2\nmiddle\ntail1\ntail2\n";
    let mut bounded = BoundedOutput::with_staging(
        ToolLimits {
            max_bytes: 64,
            max_lines: 2,
        },
        &staged_path,
    )
    .expect("staged bounded output");

    bounded.push(full_output);
    let bounded = bounded.finish();

    assert!(bounded.truncated);
    assert!(bounded.text.contains("omitted 19 bytes / 3 lines"));
    assert_eq!(fs::read(&staged_path).expect("staged output"), full_output);
    assert_eq!(bounded.blob_ref.expect("blob reference").bytes, 31);
}

#[test]
fn bounded_output_does_not_preallocate_the_configured_ceiling() {
    let output = BoundedOutput::new(ToolLimits {
        max_bytes: usize::MAX,
        max_lines: usize::MAX,
    });

    drop(output);
}

#[tokio::test]
async fn write_requires_current_hash_and_preserves_mode_when_patching() {
    let fixture = Fixture::with_file("src/lib.rs", b"old\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            fixture.path("src/lib.rs"),
            fs::Permissions::from_mode(0o640),
        )
        .expect("set mode");
    }

    let stale = invocation(
        "write",
        serde_json::json!({
            "path": "src/lib.rs",
            "expected_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
            "content": "new\n"
        }),
    );
    let error = WriteTool::default()
        .execute(fixture.context(normal_limits()), stale, &NeverCancel)
        .await
        .expect_err("stale hash");
    assert!(matches!(error, KuramaError::Tool(_)));
    assert_eq!(fs::read(fixture.path("src/lib.rs")).unwrap(), b"old\n");

    let patch = invocation(
        "write",
        serde_json::json!({
            "path": "src/lib.rs",
            "expected_sha256": "01d09d19c2139a46aebfb577780d123d7396e97201bc7ead210a2ebff8239dee",
            "patch": "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n"
        }),
    );
    let result = WriteTool::default()
        .execute(fixture.context(normal_limits()), patch, &NeverCancel)
        .await
        .expect("atomic patch");

    assert_eq!(fs::read(fixture.path("src/lib.rs")).unwrap(), b"new\n");
    assert_eq!(result.metadata["created"], false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(fixture.path("src/lib.rs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }
}

#[tokio::test]
async fn write_rejects_symlink_escapes_and_ambiguous_payloads() {
    let fixture = Fixture::empty();

    let ambiguous = invocation(
        "write",
        serde_json::json!({"path": "new.txt", "content": "x", "patch": "y"}),
    );
    let error = WriteTool::default()
        .execute(fixture.context(normal_limits()), ambiguous, &NeverCancel)
        .await
        .expect_err("one payload only");
    assert!(matches!(error, KuramaError::Tool(_)));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let outside = fixture._temp.path().join("outside");
        fs::create_dir(&outside).expect("outside dir");
        fs::write(outside.join("visible.txt"), "outside").expect("outside file");
        symlink(&outside, fixture.path("escape")).expect("symlink");
        let classified = ReadTool::default()
            .classify(
                &fixture.context(normal_limits()),
                &invocation(
                    "read",
                    serde_json::json!({
                        "files": [{"path": "escape/visible.txt", "start_byte": 0, "end_byte": 7}]
                    }),
                ),
            )
            .expect("classify escaped read");
        assert!(matches!(classified, Operation::Read { external: true, .. }));

        let escaped = invocation(
            "write",
            serde_json::json!({"path": "escape/pwned.txt", "content": "no"}),
        );
        let error = WriteTool::default()
            .execute(fixture.context(normal_limits()), escaped, &NeverCancel)
            .await
            .expect_err("scope escape");
        assert!(matches!(error, KuramaError::Policy(_)));
        assert!(!outside.join("pwned.txt").exists());
    }
}

#[tokio::test]
async fn write_ignores_unknown_fields() {
    let fixture = Fixture::empty();
    let extra = invocation(
        "write",
        serde_json::json!({
            "path": "note.txt",
            "content": "ok\n",
            "items": [{"id": "track_sdk_work", "content": "work", "status": "pending"}]
        }),
    );
    let result = WriteTool::default()
        .execute(fixture.context(normal_limits()), extra, &NeverCancel)
        .await
        .expect("ignore extra fields");
    assert!(!result.is_error);
    assert_eq!(fs::read(fixture.path("note.txt")).unwrap(), b"ok\n");
}

#[tokio::test]
async fn yolo_writes_outside_workspace() {
    let fixture = Fixture::empty();
    let outside = fixture._temp.path().join("outside");
    fs::create_dir(&outside).expect("outside dir");
    let target = outside.join("created.txt");
    let canonical_target = outside
        .canonicalize()
        .expect("canonical outside")
        .join("created.txt");
    let call = invocation(
        "write",
        serde_json::json!({"path": target, "content": "unrestricted\n"}),
    );
    let context = fixture.context_with_mode(normal_limits(), ExecutionMode::Yolo);

    let operation = WriteTool::default()
        .classify(&context, &call)
        .expect("classify external write");
    assert!(matches!(operation, Operation::Write { external: true, .. }));

    let result = WriteTool::default()
        .execute(context, call, &NeverCancel)
        .await
        .expect("write outside workspace");

    assert_eq!(fs::read(&target).unwrap(), b"unrestricted\n");
    assert_eq!(
        result.metadata["absolute_path"].as_str(),
        canonical_target.to_str()
    );
    assert_eq!(result.metadata["external"], true);
}

#[tokio::test]
async fn yolo_overwrites_existing_files_without_a_hash_for_parent_and_child() {
    for agent_id in [None, Some(AgentId::from("child-1"))] {
        let fixture = Fixture::with_file("existing.txt", b"old\n");
        let call = invocation(
            "write",
            serde_json::json!({"path": "existing.txt", "content": "new\n"}),
        );
        let mut context = fixture.context_with_mode(normal_limits(), ExecutionMode::Yolo);
        context.agent_id = agent_id;

        WriteTool::default()
            .execute(context, call, &NeverCancel)
            .await
            .expect("YOLO overwrite without expected hash");

        assert_eq!(fs::read(fixture.path("existing.txt")).unwrap(), b"new\n");
    }
}

#[tokio::test]
async fn non_yolo_overwrites_still_require_a_hash_for_parent_and_child() {
    for mode in [ExecutionMode::Supervised, ExecutionMode::Auto] {
        for agent_id in [None, Some(AgentId::from("child-1"))] {
            let fixture = Fixture::with_file("existing.txt", b"old\n");
            let call = invocation(
                "write",
                serde_json::json!({"path": "existing.txt", "content": "new\n"}),
            );
            let mut context = fixture.context_with_mode(normal_limits(), mode);
            context.agent_id = agent_id;

            let error = WriteTool::default()
                .execute(context, call, &NeverCancel)
                .await
                .expect_err("non-YOLO overwrite without expected hash");

            assert!(matches!(error, KuramaError::Tool(_)));
            assert_eq!(fs::read(fixture.path("existing.txt")).unwrap(), b"old\n");
        }
    }
}

#[tokio::test]
async fn write_contains_external_paths_outside_yolo() {
    let fixture = Fixture::empty();
    let outside = fixture._temp.path().join("outside");
    fs::create_dir(&outside).expect("outside dir");

    for mode in [ExecutionMode::Supervised, ExecutionMode::Auto] {
        let target = outside.join(format!("{mode:?}.txt"));
        let call = invocation(
            "write",
            serde_json::json!({"path": target, "content": "contained\n"}),
        );
        let error = WriteTool::default()
            .execute(
                fixture.context_with_mode(normal_limits(), mode),
                call,
                &NeverCancel,
            )
            .await
            .expect_err("external write must remain contained");

        assert!(matches!(error, KuramaError::Policy(_)));
        assert!(!target.exists());
    }
}

#[test]
fn html_extraction_drops_active_content_and_preserves_preformatted_lines() {
    let html = "<!--gone--><div>A   B</div><script>secret()</script><pre>x  y\nz</pre>\
                <style>hidden</style><ul><li>one</li><li>two &#x1F600; &amp; &#169;</li></ul>";

    assert_eq!(html_to_text(html), "A B\nx  y\nz\none\ntwo 😀 & ©");
}
