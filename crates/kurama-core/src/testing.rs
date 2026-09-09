use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use futures_util::stream;
use kurama_protocol::{
    KuramaError,
    agent::{AgentResult, OrchestrationContext, WriteScope},
    id::{AgentId, CallId, OperationId, SessionId},
    model::{BackendCapabilities, ModelEvent, ModelProfile, ModelRequest},
    policy::{PolicyContext, PolicyDecision},
    runtime::RuntimeEvent,
    session::SessionMetadata,
    tool::{Operation, ToolContext, ToolDescriptor, ToolInvocation, ToolResult},
    traits::{
        ApprovalPolicy, BoxFuture, CancelSignal, EventSink, IdGenerator, ModelBackend, ModelStream,
        SessionStore, Tool,
    },
};

use crate::agent_manager::{ChildRunContext, ChildRunner};

pub use crate::orchestrator::NoDelegation;
pub use crate::store::MemoryStore;

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

pub struct ImmediateChildRunner {
    pub summary: String,
}

impl ChildRunner for ImmediateChildRunner {
    fn run(
        &self,
        context: ChildRunContext,
    ) -> BoxFuture<'static, Result<AgentResult, KuramaError>> {
        let summary = self.summary.clone();
        Box::pin(async move {
            Ok(AgentResult {
                agent_id: context.agent_id,
                summary,
                changed_files: Vec::new(),
                evidence_refs: Vec::new(),
            })
        })
    }
}

pub fn orchestration_context() -> OrchestrationContext {
    let parent_profile = ModelProfile::new("parent", "p", 100_000, 10_000);
    OrchestrationContext {
        profiles: BTreeMap::from([("parent".into(), parent_profile.clone())]),
        parent_profile,
        role_routes: BTreeMap::new(),
        role_escalations: BTreeMap::new(),
        profile_escalations: BTreeMap::new(),
        parent_write_scope: WriteScope {
            roots: vec![PathBuf::from(".")],
            files: Vec::new(),
        },
        max_concurrency: 4,
        depth: 0,
        yolo: false,
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
