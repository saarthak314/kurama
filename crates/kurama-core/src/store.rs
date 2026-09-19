use std::{
    collections::BTreeMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use sha2::{Digest, Sha256};

use kurama_protocol::{
    KuramaError,
    id::{AgentId, OperationId, SessionId},
    session::{BlobRef, EventEnvelope, SessionMetadata, SessionSummary},
    traits::SessionStore,
};

type EventLogs = BTreeMap<(SessionId, Option<AgentId>), Vec<EventEnvelope>>;

#[derive(Default)]
pub struct MemoryStore {
    metadata: Mutex<BTreeMap<SessionId, SessionMetadata>>,
    events: Mutex<EventLogs>,
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    blob_reads: AtomicU64,
    blob_read_bytes: AtomicU64,
}

impl MemoryStore {
    pub fn events(&self, session_id: impl Into<SessionId>) -> Vec<EventEnvelope> {
        self.events
            .lock()
            .expect("memory events lock")
            .get(&(session_id.into(), None))
            .cloned()
            .unwrap_or_default()
    }

    pub fn operation_completion_count(
        &self,
        session_id: impl Into<SessionId>,
        operation_id: impl Into<OperationId>,
    ) -> usize {
        let operation_id = operation_id.into();
        self.events(session_id)
            .iter()
            .filter(|event| {
                matches!(
                    &event.event,
                    kurama_protocol::session::SessionEvent::ToolCompleted {
                        operation_id: completed,
                        ..
                    } if completed == &operation_id
                )
            })
            .count()
    }

    pub fn blob_reads(&self) -> u64 {
        self.blob_reads.load(Ordering::Relaxed)
    }

    pub fn blob_read_bytes(&self) -> u64 {
        self.blob_read_bytes.load(Ordering::Relaxed)
    }
}

impl SessionStore for MemoryStore {
    fn create(&self, metadata: &SessionMetadata) -> Result<(), KuramaError> {
        let mut sessions = self.metadata.lock().expect("memory metadata lock");
        match sessions.entry(metadata.id.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(metadata.clone());
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => Err(KuramaError::Storage(format!(
                "session {} already exists",
                metadata.id
            ))),
        }
    }

    fn append(&self, event: &EventEnvelope) -> Result<(), KuramaError> {
        self.events
            .lock()
            .expect("memory events lock")
            .entry((event.session_id.clone(), event.agent_id.clone()))
            .or_default()
            .push(event.clone());
        Ok(())
    }

    fn append_next(&self, event: &mut EventEnvelope) -> Result<(), KuramaError> {
        let mut events = self
            .events
            .lock()
            .map_err(|_| KuramaError::Storage("memory events lock poisoned".into()))?;
        let log = events
            .entry((event.session_id.clone(), event.agent_id.clone()))
            .or_default();
        event.sequence = match log.last() {
            Some(prior) => prior
                .sequence
                .checked_add(1)
                .ok_or_else(|| KuramaError::Storage("event sequence exceeds u64".into()))?,
            None => 0,
        };
        log.push(event.clone());
        Ok(())
    }

    fn replay(&self, session_id: &SessionId) -> Result<Vec<EventEnvelope>, KuramaError> {
        Ok(self
            .events
            .lock()
            .expect("memory events lock")
            .get(&(session_id.clone(), None))
            .cloned()
            .unwrap_or_default())
    }

    fn replay_agent(
        &self,
        session_id: &SessionId,
        agent_id: &AgentId,
    ) -> Result<Vec<EventEnvelope>, KuramaError> {
        Ok(self
            .events
            .lock()
            .expect("memory events lock")
            .get(&(session_id.clone(), Some(agent_id.clone())))
            .cloned()
            .unwrap_or_default())
    }

    fn list(&self) -> Result<Vec<SessionSummary>, KuramaError> {
        let metadata = self.metadata.lock().expect("memory metadata lock");
        let events = self.events.lock().expect("memory events lock");
        Ok(metadata
            .values()
            .map(|metadata| SessionSummary {
                id: metadata.id.clone(),
                created_at_ms: metadata.created_at_ms,
                updated_at_ms: events
                    .range((metadata.id.clone(), None)..)
                    .take_while(|((session_id, _), _)| session_id == &metadata.id)
                    .flat_map(|(_, log)| log.iter().map(|event| event.timestamp_ms))
                    .max()
                    .unwrap_or(metadata.created_at_ms)
                    .max(metadata.created_at_ms),
                project_root: metadata.project_root.clone(),
                profile: metadata.profile.clone(),
                mode: metadata.mode,
            })
            .collect())
    }

    fn put_blob(&self, bytes: &[u8]) -> Result<BlobRef, KuramaError> {
        let key = lowercase_hex(&Sha256::digest(bytes));
        self.blobs
            .lock()
            .expect("memory blobs lock")
            .insert(key.clone(), bytes.to_vec());
        Ok(BlobRef {
            sha256: key,
            bytes: bytes.len() as u64,
        })
    }

    fn get_blob(&self, reference: &BlobRef) -> Result<Vec<u8>, KuramaError> {
        let bytes = self
            .blobs
            .lock()
            .expect("memory blobs lock")
            .get(&reference.sha256)
            .cloned()
            .ok_or_else(|| KuramaError::NotFound(reference.sha256.clone()))?;
        self.blob_reads.fetch_add(1, Ordering::Relaxed);
        self.blob_read_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[(byte >> 4) as usize]));
        output.push(char::from(DIGITS[(byte & 0x0f) as usize]));
    }
    output
}

#[cfg(test)]
mod tests {
    #[test]
    fn duplicate_create_preserves_metadata_and_history() {
        let store = MemoryStore::default();
        let original = SessionMetadata {
            id: "session".into(),
            created_at_ms: 10,
            project_root: "/original".into(),
            profile: "first".into(),
            mode: kurama_protocol::policy::ExecutionMode::Supervised,
            redaction_best_effort: false,
        };
        store.create(&original).expect("create session");
        let first_summary = store.list().expect("list");
        assert_eq!(first_summary[0].updated_at_ms, 10);
        let mut parent = event(0, None);
        parent.timestamp_ms = 20;
        store.append(&parent).expect("append parent");
        let replacement = SessionMetadata {
            profile: "replacement".into(),
            project_root: "/other".into(),
            ..original.clone()
        };
        assert!(matches!(
            store.create(&replacement),
            Err(KuramaError::Storage(_))
        ));
        assert_eq!(store.replay(&original.id).expect("replay"), vec![parent]);
        let listed = store.list().expect("list");
        assert_eq!(listed[0].profile, original.profile);
        assert_eq!(listed[0].project_root, original.project_root);
        assert_eq!(listed[0].updated_at_ms, 20);
        let mut child = event(0, Some("child".into()));
        child.timestamp_ms = 30;
        store.append(&child).expect("append child");
        // A later append with an older timestamp must not move updated_at backwards.
        store
            .append(&event(1, Some("child".into())))
            .expect("older event");
        assert_eq!(store.list().expect("list")[0].updated_at_ms, 30);
    }

    use super::MemoryStore;
    use kurama_protocol::traits::SessionStore;
    use kurama_protocol::{
        KuramaError,
        id::{AgentId, SessionId},
        session::{BlobRef, EventEnvelope, SessionEvent, SessionMetadata, SessionSummary},
    };

    struct CompatibilityStore(MemoryStore);

    impl SessionStore for CompatibilityStore {
        fn create(&self, metadata: &SessionMetadata) -> Result<(), KuramaError> {
            self.0.create(metadata)
        }

        fn append(&self, event: &EventEnvelope) -> Result<(), KuramaError> {
            self.0.append(event)
        }

        fn replay(&self, session_id: &SessionId) -> Result<Vec<EventEnvelope>, KuramaError> {
            self.0.replay(session_id)
        }

        fn replay_agent(
            &self,
            session_id: &SessionId,
            agent_id: &AgentId,
        ) -> Result<Vec<EventEnvelope>, KuramaError> {
            self.0.replay_agent(session_id, agent_id)
        }

        fn list(&self) -> Result<Vec<SessionSummary>, KuramaError> {
            self.0.list()
        }

        fn put_blob(&self, bytes: &[u8]) -> Result<BlobRef, KuramaError> {
            self.0.put_blob(bytes)
        }

        fn get_blob(&self, reference: &BlobRef) -> Result<Vec<u8>, KuramaError> {
            self.0.get_blob(reference)
        }
    }

    fn event(sequence: u64, agent_id: Option<AgentId>) -> EventEnvelope {
        EventEnvelope::new(
            sequence,
            1,
            SessionId::from("session"),
            agent_id,
            SessionEvent::UserMessage {
                text: sequence.to_string(),
                explicit_delegation: false,
            },
        )
    }

    #[test]
    fn memory_and_compatible_append_next_allocate_contiguous_sequences_concurrently() {
        let memory = MemoryStore::default();
        let compatibility = CompatibilityStore(MemoryStore::default());
        for store in [&memory as &dyn SessionStore, &compatibility] {
            let mut parent = event(99, None);
            store.append_next(&mut parent).expect("parent event");
            let barrier = std::sync::Barrier::new(4);
            std::thread::scope(|scope| {
                for writer in 0..4 {
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        for index in 0..16 {
                            let mut next = event(writer * 16 + index, Some(AgentId::from("child")));
                            store.append_next(&mut next).expect("child event");
                        }
                    });
                }
            });
            let replayed = store
                .replay_agent(&SessionId::from("session"), &AgentId::from("child"))
                .expect("replay child");
            assert_eq!(
                replayed
                    .iter()
                    .map(|event| event.sequence)
                    .collect::<Vec<_>>(),
                (0..64).collect::<Vec<_>>()
            );
            assert_eq!(
                store.replay(&SessionId::from("session")).expect("parent"),
                vec![parent]
            );
        }
    }

    #[test]
    fn memory_and_compatible_append_next_reject_sequence_overflow() {
        let memory = MemoryStore::default();
        let compatibility = CompatibilityStore(MemoryStore::default());
        for store in [&memory as &dyn SessionStore, &compatibility] {
            let last = event(u64::MAX, None);
            store.append(&last).expect("exhausted log");
            let mut next = event(0, None);
            assert!(matches!(
                store.append_next(&mut next),
                Err(KuramaError::Storage(_))
            ));
            assert_eq!(
                store
                    .replay(&SessionId::from("session"))
                    .expect("unchanged log"),
                vec![last]
            );
        }
    }

    #[test]
    fn memory_append_next_returns_an_error_for_a_poisoned_log() {
        let store = MemoryStore::default();
        let _ = std::panic::catch_unwind(|| {
            let _guard = store.events.lock().expect("events lock");
            panic!("poison log");
        });
        assert!(matches!(
            store.append_next(&mut event(0, None)),
            Err(KuramaError::Storage(_))
        ));
    }

    #[test]
    fn same_length_blobs_stay_distinct() {
        let store = MemoryStore::default();
        let first = store.put_blob(b"aaaa").expect("first");
        let second = store.put_blob(b"bbbb").expect("second");
        assert_ne!(first.sha256, second.sha256);
        assert_eq!(store.get_blob(&first).expect("first bytes"), b"aaaa");
        assert_eq!(store.get_blob(&second).expect("second bytes"), b"bbbb");
    }
}
