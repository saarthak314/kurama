#![cfg(feature = "fs-store")]

#[path = "../src/storage/mod.rs"]
mod storage;

use std::io::Write;

use kurama_protocol::{
    KuramaError,
    id::{AgentId, SessionId},
    policy::ExecutionMode,
    session::{BlobRef, EventEnvelope, SessionEvent, SessionMetadata},
    traits::SessionStore,
};
use storage::FsSessionStore;

fn metadata(id: &str, created_at_ms: u64) -> SessionMetadata {
    SessionMetadata {
        id: SessionId::from(id),
        created_at_ms,
        project_root: "/workspace/project".into(),
        profile: "primary".into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    }
}

fn event(session: &str, sequence: u64, timestamp_ms: u64, agent: Option<&str>) -> EventEnvelope {
    EventEnvelope::new(
        sequence,
        timestamp_ms,
        SessionId::from(session),
        agent.map(AgentId::from),
        SessionEvent::UserMessage {
            text: format!("message-{sequence}"),
        },
    )
}

#[test]
fn open_creates_owner_only_layout_without_overwriting_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join(".kurama");
    std::fs::create_dir(&root).expect("root");
    std::fs::write(root.join("config.toml"), "existing = true\n").expect("config");

    let store = FsSessionStore::open(root.clone()).expect("store");

    assert_eq!(store.root(), root);
    assert_eq!(
        std::fs::read_to_string(root.join("config.toml")).expect("read config"),
        "existing = true\n"
    );
    for directory in ["sessions", "blobs", "cache"] {
        assert!(root.join(directory).is_dir(), "missing {directory}");
    }
    assert!(root.join("state.json").is_file());
    store.create(&metadata("permissions", 1)).expect("session");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            std::fs::metadata(&root)
                .expect("root metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(root.join("config.toml"))
                .expect("config metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(root.join("state.json"))
                .expect("state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(root.join("sessions/permissions"))
                .expect("session metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(root.join("sessions/permissions/events.jsonl"))
                .expect("event log metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn replay_repairs_only_an_incomplete_trailing_record() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    store.create(&metadata("session-a", 10)).expect("create");
    store
        .append(&event("session-a", 0, 11, None))
        .expect("append");

    let log = temp.path().join("sessions/session-a/events.jsonl");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .expect("open")
        .write_all(br#"{"schema_version":1"#)
        .expect("write torn tail");

    let first = store.replay(&SessionId::from("session-a")).expect("repair");
    assert_eq!(first.len(), 1);
    let repaired = store.replay(&SessionId::from("session-a")).expect("replay");
    assert_eq!(repaired.len(), 2);
    assert!(matches!(
        repaired[1].event,
        SessionEvent::RecoveryRepair { removed_bytes } if removed_bytes == 19
    ));

    std::fs::OpenOptions::new()
        .append(true)
        .open(log)
        .expect("open")
        .write_all(b"not-json\n")
        .expect("write malformed line");
    assert!(matches!(
        store.replay(&SessionId::from("session-a")),
        Err(KuramaError::Storage(_))
    ));
}

#[test]
fn parent_and_child_logs_validate_sequences_independently() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    store.create(&metadata("session-b", 20)).expect("create");

    store
        .append(&event("session-b", 0, 21, None))
        .expect("parent append");
    store
        .append(&event("session-b", 0, 22, Some("agent-a")))
        .expect("child append");

    assert!(matches!(
        store.append(&event("session-b", 2, 23, Some("agent-a"))),
        Err(KuramaError::Storage(_))
    ));
    assert_eq!(
        store
            .replay_agent(&SessionId::from("session-b"), &AgentId::from("agent-a"))
            .expect("child replay")
            .len(),
        1
    );
    assert!(
        temp.path()
            .join("sessions/session-b/agents/agent-a.jsonl")
            .is_file()
    );
}

#[test]
fn replaying_a_new_child_log_returns_empty_history() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    store
        .create(&metadata("session-child", 20))
        .expect("create");

    let events = store
        .replay_agent(
            &SessionId::from("session-child"),
            &AgentId::from("agent-new"),
        )
        .expect("new child history");

    assert!(events.is_empty());
}

#[test]
fn blobs_are_content_addressed_deduplicated_and_verified() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");

    let first = store.put_blob(b"same bytes").expect("first blob");
    let second = store.put_blob(b"same bytes").expect("second blob");
    assert_eq!(first, second);
    assert_eq!(
        first.sha256,
        "58100dc8fc06562ce3e578231dc948e083520ee49c4b4ee5a5a28bb4b4003feb"
    );
    assert_eq!(store.get_blob(&first).expect("read blob"), b"same bytes");

    std::fs::write(temp.path().join("blobs").join(&first.sha256), b"tampered")
        .expect("corrupt blob");
    assert!(matches!(
        store.get_blob(&first),
        Err(KuramaError::Storage(_))
    ));
    assert!(matches!(
        store.get_blob(&BlobRef {
            sha256: "../state.json".into(),
            bytes: 0,
        }),
        Err(KuramaError::Storage(_))
    ));
}

#[test]
fn list_uses_durable_metadata_and_latest_event_time() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    store.create(&metadata("older", 10)).expect("older");
    store.create(&metadata("newer", 20)).expect("newer");
    store.append(&event("older", 0, 30, None)).expect("event");

    let reopened = FsSessionStore::open(temp.path().to_owned()).expect("reopen");
    let sessions = reopened.list().expect("list");
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].id, SessionId::from("older"));
    assert_eq!(sessions[0].updated_at_ms, 30);
    assert_eq!(sessions[1].id, SessionId::from("newer"));
    assert_eq!(sessions[1].updated_at_ms, 20);
}
