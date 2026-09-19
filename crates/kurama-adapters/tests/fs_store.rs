#![cfg(feature = "fs-store")]

use std::io::Write;

use kurama_adapters::FsSessionStore;
use kurama_protocol::{
    KuramaError,
    id::{AgentId, SessionId},
    policy::ExecutionMode,
    session::{BlobRef, EventEnvelope, SessionEvent, SessionMetadata},
    traits::SessionStore,
};

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
            explicit_delegation: false,
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
fn create_lock_name_is_a_valid_session_id() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    store
        .create(&metadata(".create.lock", 1))
        .expect("dot-named session");
    store
        .append(&event(".create.lock", 0, 2, None))
        .expect("append");
    assert!(store.create(&metadata(".create.lock", 3)).is_err());
    store
        .create(&metadata("ordinary", 4))
        .expect("another session");
    assert_eq!(
        store
            .list()
            .expect("list")
            .into_iter()
            .map(|summary| summary.id)
            .collect::<Vec<_>>(),
        vec![SessionId::from("ordinary"), SessionId::from(".create.lock")],
    );
    assert_eq!(
        store
            .replay(&SessionId::from(".create.lock"))
            .expect("replay"),
        vec![event(".create.lock", 0, 2, None)],
    );
}

#[test]
fn preexisting_create_lock_session_remains_visible_and_does_not_block_creation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let session = temp.path().join("sessions/.create.lock");
    std::fs::create_dir_all(session.join("agents")).expect("legacy session layout");
    std::fs::write(
        session.join("metadata.json"),
        serde_json::to_vec(&metadata(".create.lock", 1)).expect("metadata"),
    )
    .expect("legacy metadata");
    let original = event(".create.lock", 0, 2, None);
    std::fs::write(
        session.join("events.jsonl"),
        format!("{}\n", serde_json::to_string(&original).expect("event")),
    )
    .expect("legacy event log");

    let store = FsSessionStore::open(temp.path().to_owned()).expect("open legacy store");
    assert_eq!(
        store.list().expect("list legacy session")[0].id,
        SessionId::from(".create.lock")
    );
    assert_eq!(
        store
            .replay(&SessionId::from(".create.lock"))
            .expect("legacy replay"),
        vec![original]
    );
    store
        .create(&metadata("new-session", 3))
        .expect("create beside legacy session");
    assert_eq!(
        store
            .list()
            .expect("list both")
            .into_iter()
            .map(|summary| summary.id)
            .collect::<Vec<_>>(),
        vec![
            SessionId::from("new-session"),
            SessionId::from(".create.lock")
        ],
    );
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

    let mut first = store.replay(&SessionId::from("session-a")).expect("repair");
    let repair = first.last().expect("committed repair in first replay");
    assert!(matches!(
        repair.event,
        SessionEvent::RecoveryRepair { removed_bytes: 19 }
    ));
    let next = event("session-a", repair.sequence + 1, 12, None);
    store.append(&next).expect("append using first replay");
    first.push(next);
    let repaired = store.replay(&SessionId::from("session-a")).expect("replay");
    assert_eq!(
        repaired, first,
        "a second replay must not add another repair"
    );

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
fn replayed_child_repair_is_immediately_appendable_without_advancing_parent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    let session_id = SessionId::from("child-repair");
    let agent_id = AgentId::from("child");
    store.create(&metadata("child-repair", 10)).expect("create");
    let parent = event("child-repair", 0, 11, None);
    store.append(&parent).expect("parent event");
    store
        .append(&event("child-repair", 0, 12, Some("child")))
        .expect("child event");
    std::fs::OpenOptions::new()
        .append(true)
        .open(temp.path().join("sessions/child-repair/agents/child.jsonl"))
        .expect("child log")
        .write_all(b"torn")
        .expect("torn child tail");

    let mut first = store.replay_agent(&session_id, &agent_id).expect("repair");
    let repair = first.last().expect("committed child repair");
    assert!(matches!(
        repair.event,
        SessionEvent::RecoveryRepair { removed_bytes: 4 }
    ));
    let next_child = event("child-repair", repair.sequence + 1, 13, Some("child"));
    store
        .append(&next_child)
        .expect("append using first child replay");
    first.push(next_child);
    assert_eq!(
        store
            .replay_agent(&session_id, &agent_id)
            .expect("child replay"),
        first
    );

    let next_parent = event("child-repair", 1, 14, None);
    store
        .append(&next_parent)
        .expect("parent sequence unchanged");
    assert_eq!(
        store.replay(&session_id).expect("parent replay"),
        vec![parent, next_parent]
    );
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
    let mut parent = event("session-b", 99, 24, None);
    let mut child = event("session-b", 99, 25, Some("agent-a"));
    store.append_next(&mut parent).expect("next parent");
    store.append_next(&mut child).expect("next child");
    assert_eq!((parent.sequence, child.sequence), (1, 1));
    assert_eq!(
        store
            .replay_agent(&SessionId::from("session-b"), &AgentId::from("agent-a"))
            .expect("child replay")
            .len(),
        2
    );
    assert!(
        temp.path()
            .join("sessions/session-b/agents/agent-a.jsonl")
            .is_file()
    );
}

#[test]
fn append_next_serializes_independent_handles_to_the_same_child_log() {
    const WRITERS: usize = 4;
    const EVENTS_PER_WRITER: usize = 16;
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    store.create(&metadata("concurrent", 10)).expect("create");
    let handles: Vec<_> = (0..WRITERS)
        .map(|_| FsSessionStore::open(temp.path().to_owned()).expect("independent handle"))
        .collect();
    let barrier = std::sync::Barrier::new(WRITERS);
    let mut assigned = std::thread::scope(|scope| {
        let workers: Vec<_> = handles
            .iter()
            .enumerate()
            .map(|(writer, handle)| {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    (0..EVENTS_PER_WRITER)
                        .map(|index| {
                            let unique_id = (writer * EVENTS_PER_WRITER + index) as u64;
                            let mut next = event("concurrent", unique_id, 11, Some("child"));
                            handle.append_next(&mut next).expect("allocate and append");
                            (next.sequence, next.event)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("writer"))
            .collect::<Vec<_>>()
    });
    assigned.sort_by_key(|(sequence, _)| *sequence);
    let replayed = store
        .replay_agent(&SessionId::from("concurrent"), &AgentId::from("child"))
        .expect("replay committed events");
    assert_eq!(replayed.len(), WRITERS * EVENTS_PER_WRITER);
    for (index, ((sequence, payload), replayed)) in assigned.iter().zip(&replayed).enumerate() {
        assert_eq!(*sequence, index as u64);
        assert_eq!(replayed.sequence, *sequence);
        assert_eq!(&replayed.event, payload);
    }
}

#[test]
fn append_next_rejects_invalid_tails_without_changing_the_log() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    store.create(&metadata("invalid-tail", 10)).expect("create");
    let log = temp.path().join("sessions/invalid-tail/events.jsonl");
    let mut unsupported = event("invalid-tail", 0, 11, None);
    unsupported.schema_version += 1;
    let invalid_events = [
        unsupported,
        event("wrong-session", 0, 11, None),
        event("invalid-tail", 0, 11, Some("wrong-agent")),
        event("invalid-tail", u64::MAX, 11, None),
    ];
    let mut invalid_tails: Vec<_> = invalid_events
        .iter()
        .map(|event| {
            let mut bytes = serde_json::to_vec(event).expect("encode invalid tail");
            bytes.push(b'\n');
            bytes
        })
        .collect();
    invalid_tails.extend([b"not-json\n".to_vec(), br#"{"schema_version":1"#.to_vec()]);
    for bytes in invalid_tails {
        std::fs::write(&log, &bytes).expect("invalid tail");
        let mut next = event("invalid-tail", 0, 12, None);
        assert!(matches!(
            store.append_next(&mut next),
            Err(KuramaError::Storage(_))
        ));
        assert_eq!(std::fs::read(&log).expect("unchanged log"), bytes);
    }
}

#[test]
fn append_next_validates_new_events_and_continues_after_tail_repair() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    let mut next = event("repaired-next", 99, 11, None);
    assert!(matches!(
        store.append_next(&mut next),
        Err(KuramaError::NotFound(_))
    ));
    store
        .create(&metadata("repaired-next", 10))
        .expect("create");
    next.schema_version += 1;
    assert!(matches!(
        store.append_next(&mut next),
        Err(KuramaError::Storage(_))
    ));
    next.schema_version -= 1;
    store.append_next(&mut next).expect("first event");
    assert_eq!(next.sequence, 0);
    let log = temp.path().join("sessions/repaired-next/events.jsonl");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .expect("open log")
        .write_all(b"torn")
        .expect("torn tail");
    assert!(matches!(
        store.append_next(&mut next),
        Err(KuramaError::Storage(_))
    ));
    store
        .replay(&SessionId::from("repaired-next"))
        .expect("repair");
    store.append_next(&mut next).expect("append after repair");
    assert_eq!(next.sequence, 2);
    let replayed = store
        .replay(&SessionId::from("repaired-next"))
        .expect("replay");
    assert!(matches!(
        replayed[1].event,
        SessionEvent::RecoveryRepair { removed_bytes: 4 }
    ));
    assert_eq!(replayed[2], next);
}

#[test]
fn append_next_reads_a_tail_record_larger_than_its_scan_buffer() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    store.create(&metadata("large-tail", 10)).expect("create");
    let mut large = event("large-tail", 99, 11, None);
    large.event = SessionEvent::UserMessage {
        text: "x".repeat(32 * 1024),
        explicit_delegation: false,
    };
    store.append_next(&mut large).expect("large event");
    let mut next = event("large-tail", 99, 12, None);
    store
        .append_next(&mut next)
        .expect("event following large tail");
    assert_eq!(next.sequence, 1);
    assert_eq!(
        store
            .replay(&SessionId::from("large-tail"))
            .expect("replay"),
        vec![large, next]
    );
}

#[cfg(unix)]
#[test]
fn append_next_rejects_symlinked_logs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    store.create(&metadata("symlink", 10)).expect("create");
    let target = temp.path().join("target.jsonl");
    std::fs::write(&target, b"").expect("target");
    std::os::unix::fs::symlink(
        &target,
        temp.path().join("sessions/symlink/agents/child.jsonl"),
    )
    .expect("symlink child log");
    let mut next = event("symlink", 0, 11, Some("child"));
    assert!(matches!(
        store.append_next(&mut next),
        Err(KuramaError::Storage(_))
    ));
    assert_eq!(std::fs::read(target).expect("untouched target"), b"");
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
fn blob_tails_match_verified_full_bytes_across_chunk_boundaries() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    let bytes: Vec<u8> = (0..3 * 64 * 1024 + 17)
        .map(|index| ((index * 31 + index / 251) % 256) as u8)
        .collect();
    let reference = store.put_blob(&bytes).expect("blob");
    let full = store.get_blob(&reference).expect("verified full blob");
    for max_bytes in [
        0,
        1,
        64 * 1024 - 1,
        64 * 1024,
        64 * 1024 + 1,
        full.len(),
        usize::MAX,
    ] {
        let tail = store
            .get_blob_tail(&reference, max_bytes)
            .expect("verified tail");
        assert_eq!(tail, full[full.len().saturating_sub(max_bytes)..]);
    }
    let empty = store.put_blob(b"").expect("empty blob");
    assert_eq!(store.get_blob_tail(&empty, 0).expect("zero limit"), b"");
    assert_eq!(
        store.get_blob_tail(&empty, usize::MAX).expect("empty tail"),
        b""
    );
}

#[test]
fn blob_tails_reject_corruption_outside_the_retained_suffix_even_with_zero_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    let mut bytes = vec![b'x'; 3 * 64 * 1024 + 17];
    let reference = store.put_blob(&bytes).expect("blob");
    bytes[0] = b'y';
    std::fs::write(temp.path().join("blobs").join(&reference.sha256), &bytes)
        .expect("corrupt discarded prefix without changing length");
    for max_bytes in [0, 17] {
        assert!(matches!(
            store.get_blob_tail(&reference, max_bytes),
            Err(KuramaError::Storage(_))
        ));
    }
}

#[test]
fn blob_tails_require_exact_length_and_valid_hash_paths() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    let reference = store.put_blob(b"complete bytes").expect("blob");
    for bytes in [reference.bytes - 1, reference.bytes + 1] {
        let wrong_length = BlobRef {
            bytes,
            ..reference.clone()
        };
        assert!(matches!(
            store.get_blob_tail(&wrong_length, 4),
            Err(KuramaError::Storage(_))
        ));
    }
    let oversized = BlobRef {
        bytes: u64::MAX,
        ..reference
    };
    assert!(matches!(
        store.get_blob_tail(&oversized, usize::MAX),
        Err(KuramaError::Storage(_))
    ));
    assert!(matches!(
        store.get_blob_tail(
            &BlobRef {
                sha256: "../state.json".into(),
                bytes: 0
            },
            0
        ),
        Err(KuramaError::Storage(_))
    ));
}

#[cfg(unix)]
#[test]
fn blob_tails_reject_symlinks_even_when_the_target_matches() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    let reference = store.put_blob(b"complete bytes").expect("blob");
    let path = temp.path().join("blobs").join(&reference.sha256);
    let target = temp.path().join("outside-blob");
    std::fs::rename(&path, &target).expect("move original bytes");
    std::os::unix::fs::symlink(&target, &path).expect("symlink");
    assert!(matches!(
        store.get_blob_tail(&reference, 4),
        Err(KuramaError::Storage(_))
    ));
    std::fs::remove_file(&target).expect("remove target");
    assert!(matches!(
        store.get_blob_tail(&reference, 0),
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

#[test]
fn list_includes_a_new_repair_in_the_first_summary() {
    for agent in [None, Some("child")] {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
        store
            .create(&metadata("summary-repair", 1))
            .expect("create");
        store
            .append(&event("summary-repair", 0, 2, agent))
            .expect("event");
        let relative_log = if agent.is_some() {
            "sessions/summary-repair/agents/child.jsonl"
        } else {
            "sessions/summary-repair/events.jsonl"
        };
        std::fs::OpenOptions::new()
            .append(true)
            .open(temp.path().join(relative_log))
            .expect("log")
            .write_all(b"torn")
            .expect("torn tail");

        let listed = store.list().expect("repair during list");
        let session_id = SessionId::from("summary-repair");
        let replay = match agent {
            Some(agent) => store.replay_agent(&session_id, &AgentId::from(agent)),
            None => store.replay(&session_id),
        }
        .expect("replay after list");
        let repair = replay.last().expect("repair");
        assert!(matches!(
            repair.event,
            SessionEvent::RecoveryRepair { removed_bytes: 4 }
        ));
        assert_eq!(listed[0].updated_at_ms, repair.timestamp_ms);
        assert_eq!(store.list().expect("second list"), listed);
    }
}

#[test]
fn concurrent_first_openers_and_duplicate_creators_publish_one_complete_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("new-store");
    let barrier = std::sync::Barrier::new(16);
    let winners = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..16)
            .map(|index| {
                let root = &root;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    let store = FsSessionStore::open(root.clone()).expect("concurrent first open");
                    let created = store.create(&metadata("same-session", index)).is_ok();
                    if created { Some(index) } else { None }
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().expect("creator"))
            .collect::<Vec<_>>()
    });
    assert_eq!(winners.len(), 1);
    let store = FsSessionStore::open(root.clone()).expect("reopen");
    let summaries = store.list().expect("list complete session");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].created_at_ms, winners[0]);
    assert!(
        store
            .replay(&SessionId::from("same-session"))
            .expect("complete empty log")
            .is_empty()
    );
    assert!(root.join("sessions/same-session/agents").is_dir());
    store
        .append(&event("same-session", 0, 20, None))
        .expect("append after racing creation");
    assert_eq!(
        store
            .replay(&SessionId::from("same-session"))
            .expect("replay")
            .len(),
        1
    );
}

#[test]
fn concurrent_repair_readers_append_exactly_one_repair() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    store.create(&metadata("repair-race", 1)).expect("session");
    store
        .append(&event("repair-race", 0, 2, None))
        .expect("event");
    std::fs::OpenOptions::new()
        .append(true)
        .open(temp.path().join("sessions/repair-race/events.jsonl"))
        .expect("log")
        .write_all(b"{\"torn\"")
        .expect("torn tail");
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let barrier = &barrier;
            let root = temp.path();
            scope.spawn(move || {
                let store = FsSessionStore::open(root.to_owned()).expect("independent reader");
                barrier.wait();
                let events = store
                    .replay(&SessionId::from("repair-race"))
                    .expect("concurrent repair");
                assert_eq!(events.len(), 2);
                assert!(matches!(
                    events[1].event,
                    SessionEvent::RecoveryRepair { .. }
                ));
            });
        }
    });
    store
        .append(&event("repair-race", 2, 3, None))
        .expect("append after repair");
    assert_eq!(
        store
            .replay(&SessionId::from("repair-race"))
            .expect("durable replay")
            .len(),
        3
    );
}

#[test]
fn list_uses_the_maximum_timestamp_not_the_final_record() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    store.create(&metadata("nonmonotonic", 1)).expect("session");
    for item in [
        event("nonmonotonic", 0, 500, None),
        event("nonmonotonic", 1, 2, None),
        event("nonmonotonic", 0, 900, Some("child")),
        event("nonmonotonic", 1, 3, Some("child")),
    ] {
        store.append(&item).expect("append");
    }
    assert_eq!(store.list().expect("list")[0].updated_at_ms, 900);
}

#[cfg(unix)]
#[test]
fn opened_store_is_not_redirected_by_a_replaced_root() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("root");
    let outside = temp.path().join("outside");
    let store = FsSessionStore::open(root.clone()).expect("store");
    let other = FsSessionStore::open(outside.clone()).expect("outside");
    store.create(&metadata("session", 1)).expect("session");
    other
        .create(&metadata("session", 999))
        .expect("outside session");
    let reference = store.put_blob(b"opened-root-bytes").expect("blob");
    let moved = temp.path().join("original");
    std::fs::rename(&root, &moved).expect("move root");
    symlink(&outside, &root).expect("replace root");
    store
        .append(&event("session", 0, 2, None))
        .expect("append to original");
    assert_eq!(
        store.get_blob(&reference).expect("original blob"),
        b"opened-root-bytes"
    );
    assert_eq!(
        store
            .replay(&SessionId::from("session"))
            .expect("original log")
            .len(),
        1
    );
    assert!(
        other
            .replay(&SessionId::from("session"))
            .expect("outside log")
            .is_empty()
    );
    assert_eq!(
        other.list().expect("outside metadata")[0].created_at_ms,
        999
    );
}

#[test]
fn deduplicated_put_checks_every_chunk_before_accepting_existing_blob() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = FsSessionStore::open(temp.path().to_owned()).expect("store");
    let bytes = vec![b'x'; 3 * 64 * 1024 + 1];
    let reference = store.put_blob(&bytes).expect("blob");
    for offset in [0, 64 * 1024, bytes.len() - 1] {
        let mut corrupt = bytes.clone();
        corrupt[offset] = b'y';
        std::fs::write(temp.path().join("blobs").join(&reference.sha256), corrupt)
            .expect("corrupt blob");
        assert!(store.put_blob(&bytes).is_err());
    }
    std::fs::write(temp.path().join("blobs").join(&reference.sha256), &bytes)
        .expect("restore blob");
    assert_eq!(store.put_blob(&bytes).expect("verified dedup"), reference);
}
