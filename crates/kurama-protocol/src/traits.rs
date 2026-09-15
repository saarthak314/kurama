use crate::{
    KuramaError,
    agent::{AgentSnapshot, DelegationRequest, OrchestrationContext, SchedulePlan},
    id::{AgentId, CallId, OperationId, SessionId},
    model::{BackendCapabilities, ModelEvent, ModelRequest},
    policy::{PolicyContext, PolicyDecision},
    runtime::RuntimeEvent,
    session::{BlobRef, EventEnvelope, SessionMetadata, SessionSummary},
    tool::{Operation, ToolContext, ToolDescriptor, ToolInvocation, ToolResult},
};

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
pub type ModelStream =
    std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<ModelEvent, KuramaError>> + Send>>;

pub trait CancelSignal: Send + Sync {
    fn is_cancelled(&self) -> bool;
    fn cancelled(&self) -> BoxFuture<'_, ()>;
}

pub trait ModelBackend: Send + Sync {
    fn backend_name(&self) -> &'static str;
    fn capabilities(&self) -> BackendCapabilities;
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ModelStream, KuramaError>>;
}

pub trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;
    fn classify(
        &self,
        context: &ToolContext,
        invocation: &ToolInvocation,
    ) -> Result<Operation, KuramaError>;
    fn execute<'a>(
        &'a self,
        context: ToolContext,
        invocation: ToolInvocation,
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ToolResult, KuramaError>>;
}

pub trait ApprovalPolicy: Send + Sync {
    fn decide(&self, context: &PolicyContext, operation: &Operation) -> PolicyDecision;
}

pub trait SessionStore: Send + Sync {
    fn create(&self, metadata: &SessionMetadata) -> Result<(), KuramaError>;
    fn append(&self, event: &EventEnvelope) -> Result<(), KuramaError>;

    /// Assigns the next sequence in this event's session or agent log and appends it.
    /// On success, `event.sequence` is the committed sequence. Concurrent callers of
    /// `append_next` are serialized; callers of `append` must coordinate separately.
    /// The compatibility implementation locks replay and append process-wide.
    /// Stores shared across processes should override this with a storage-level
    /// transaction or lock, and can avoid replaying the entire log that way.
    fn append_next(&self, event: &mut EventEnvelope) -> Result<(), KuramaError> {
        static APPEND_NEXT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = APPEND_NEXT_LOCK
            .lock()
            .map_err(|_| KuramaError::Storage("append_next lock poisoned".into()))?;
        let events = match &event.agent_id {
            Some(agent_id) => self.replay_agent(&event.session_id, agent_id)?,
            None => self.replay(&event.session_id)?,
        };
        event.sequence = match events.last() {
            Some(prior) => prior
                .sequence
                .checked_add(1)
                .ok_or_else(|| KuramaError::Storage("event sequence exceeds u64".into()))?,
            None => 0,
        };
        self.append(event)
    }

    fn replay(&self, session_id: &SessionId) -> Result<Vec<EventEnvelope>, KuramaError>;
    fn replay_agent(
        &self,
        session_id: &SessionId,
        agent_id: &AgentId,
    ) -> Result<Vec<EventEnvelope>, KuramaError>;
    fn list(&self) -> Result<Vec<SessionSummary>, KuramaError>;
    fn put_blob(&self, bytes: &[u8]) -> Result<BlobRef, KuramaError>;
    fn get_blob(&self, reference: &BlobRef) -> Result<Vec<u8>, KuramaError>;
}

pub trait EventSink: Send + Sync {
    fn emit(&self, event: RuntimeEvent) -> Result<(), KuramaError>;
}

pub trait Orchestrator: Send + Sync {
    fn explicit_delegation(&self, user_text: &str) -> bool;
    fn resolve(
        &self,
        request: DelegationRequest,
        context: &OrchestrationContext,
    ) -> Result<SchedulePlan, KuramaError>;
    fn escalate(
        &self,
        agent: &AgentSnapshot,
        reason: &str,
        context: &OrchestrationContext,
    ) -> Result<Option<String>, KuramaError>;
}

pub trait IdGenerator: Send + Sync {
    fn session_id(&self) -> SessionId;
    fn agent_id(&self) -> AgentId;
    fn operation_id(&self) -> OperationId;
    fn call_id(&self) -> CallId;
}
