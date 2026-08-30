use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use futures_util::stream;
use kurama_protocol::{
    KuramaError,
    agent::{AgentSnapshot, DelegationRequest, OrchestrationContext, SchedulePlan},
    id::{AgentId, CallId, OperationId, SessionId},
    model::{BackendCapabilities, ModelEvent, ModelRequest},
    policy::{PolicyContext, PolicyDecision},
    runtime::RuntimeEvent,
    session::{BlobRef, EventEnvelope, SessionMetadata, SessionSummary},
    tool::{Operation, ToolContext, ToolDescriptor, ToolInvocation, ToolResult},
    traits::{
        ApprovalPolicy, BoxFuture, CancelSignal, EventSink, IdGenerator, ModelBackend, ModelStream,
        Orchestrator, SessionStore, Tool,
    },
};

type EventLogs = BTreeMap<(SessionId, Option<AgentId>), Vec<EventEnvelope>>;

#[derive(Default)]
pub struct SequenceIds {
    next: AtomicU64,
}

impl SequenceIds {
    pub fn new(start: u64) -> Self {
        Self {
            next: AtomicU64::new(start),
        }
    }

    fn next(&self, prefix: &str) -> String {
        format!("{prefix}_{}", self.next.fetch_add(1, Ordering::Relaxed))
    }
}

impl IdGenerator for SequenceIds {
    fn session_id(&self) -> SessionId {
        self.next("s").into()
    }

    fn agent_id(&self) -> AgentId {
        self.next("a").into()
    }

    fn operation_id(&self) -> OperationId {
        self.next("o").into()
    }

    fn call_id(&self) -> CallId {
        self.next("c").into()
    }
}

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
        let key = format!("memory-{}", bytes.len());
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

#[derive(Default)]
pub struct CollectingSink {
    events: Mutex<Vec<RuntimeEvent>>,
}

impl CollectingSink {
    pub fn take(&self) -> Vec<RuntimeEvent> {
        std::mem::take(&mut *self.events.lock().expect("sink lock"))
    }
}

impl EventSink for CollectingSink {
    fn emit(&self, event: RuntimeEvent) -> Result<(), KuramaError> {
        self.events.lock().expect("sink lock").push(event);
        Ok(())
    }
}

pub struct ScriptedBackend {
    name: &'static str,
    streams: Mutex<VecDeque<Vec<Result<ModelEvent, KuramaError>>>>,
}

impl ScriptedBackend {
    pub fn new(streams: Vec<Vec<Result<ModelEvent, KuramaError>>>) -> Self {
        Self {
            name: "scripted",
            streams: Mutex::new(streams.into()),
        }
    }
}

impl ModelBackend for ScriptedBackend {
    fn backend_name(&self) -> &'static str {
        self.name
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::remote_default()
    }

    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ModelStream, KuramaError>> {
        Box::pin(async move {
            let events = self
                .streams
                .lock()
                .expect("scripted backend lock")
                .pop_front()
                .unwrap_or_default();
            Ok(Box::pin(stream::iter(events)) as ModelStream)
        })
    }
}

#[derive(Default)]
pub struct AllowAllPolicy;

impl ApprovalPolicy for AllowAllPolicy {
    fn decide(&self, _context: &PolicyContext, _operation: &Operation) -> PolicyDecision {
        PolicyDecision::Allow
    }
}

#[derive(Default)]
pub struct NoDelegation;

impl Orchestrator for NoDelegation {
    fn explicit_delegation(&self, _user_text: &str) -> bool {
        false
    }

    fn resolve(
        &self,
        _request: DelegationRequest,
        _context: &OrchestrationContext,
    ) -> Result<SchedulePlan, KuramaError> {
        Ok(SchedulePlan {
            ready: Vec::new(),
            queued: Vec::new(),
            blocked: Vec::new(),
        })
    }

    fn escalate(
        &self,
        _agent: &AgentSnapshot,
        _reason: &str,
        _context: &OrchestrationContext,
    ) -> Result<Option<String>, KuramaError> {
        Ok(None)
    }
}

#[derive(Default)]
pub struct NeverCancel;

impl CancelSignal for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

pub struct EchoTool;

impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "echo".into(),
            description: "echo test input".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    fn classify(
        &self,
        context: &ToolContext,
        _invocation: &ToolInvocation,
    ) -> Result<Operation, KuramaError> {
        Ok(Operation::Read {
            path: context.cwd.clone(),
            external: false,
        })
    }

    fn execute<'a>(
        &'a self,
        _context: ToolContext,
        invocation: ToolInvocation,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ToolResult, KuramaError>> {
        Box::pin(async move { Ok(ToolResult::success(invocation.call_id, "ok")) })
    }
}

pub fn model_backend_contract(backend: Arc<dyn ModelBackend>) {
    assert!(!backend.backend_name().is_empty());
    assert!(backend.capabilities().streaming);
}

pub fn tool_contract(tool: Arc<dyn Tool>) {
    let descriptor = tool.descriptor();
    assert!(!descriptor.name.is_empty());
    assert!(descriptor.parameters.is_object());
}

pub fn session_store_contract(store: Arc<dyn SessionStore>, metadata: &SessionMetadata) {
    store.create(metadata).expect("create session");
    assert!(
        store
            .replay(&metadata.id)
            .expect("replay session")
            .is_empty()
    );
}

pub fn policy_contract(policy: Arc<dyn ApprovalPolicy>, context: &PolicyContext) {
    let operation = Operation::Read {
        path: context.workspace_root.clone(),
        external: false,
    };
    let _ = policy.decide(context, &operation);
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurama_protocol::traits::{
        ApprovalPolicy, EventSink, IdGenerator, ModelBackend, SessionStore,
    };

    #[test]
    fn fakes_are_public_trait_objects() {
        let _: Arc<dyn IdGenerator> = Arc::new(SequenceIds::default());
        let _: Arc<dyn SessionStore> = Arc::new(MemoryStore::default());
        let _: Arc<dyn EventSink> = Arc::new(CollectingSink::default());
        let _: Arc<dyn ModelBackend> = Arc::new(ScriptedBackend::new(Vec::new()));
        let _: Arc<dyn ApprovalPolicy> = Arc::new(AllowAllPolicy);
    }
}
