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
        self.metadata
            .lock()
            .expect("memory metadata lock")
            .insert(metadata.id.clone(), metadata.clone());
        Ok(())
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
        Ok(metadata
            .values()
            .map(|metadata| SessionSummary {
                id: metadata.id.clone(),
                created_at_ms: metadata.created_at_ms,
                updated_at_ms: metadata.created_at_ms,
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
    use super::MemoryStore;
    use kurama_protocol::traits::SessionStore;

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
