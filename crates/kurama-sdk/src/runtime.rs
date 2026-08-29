use std::{collections::BTreeMap, fmt, sync::Arc};

use kurama_protocol::{
    KuramaError,
    agent::WriteScope,
    model::ModelProfile,
    policy::AutoBoundaries,
    session::{EventEnvelope, SessionMetadata},
    traits::{
        ApprovalPolicy, EventSink, IdGenerator, ModelBackend, Orchestrator, SessionStore, Tool,
    },
};

pub struct RuntimeParts {
    pub profiles: BTreeMap<String, (ModelProfile, Arc<dyn ModelBackend>)>,
    pub active_profile: String,
    pub tools: BTreeMap<String, Arc<dyn Tool>>,
    pub policy: Arc<dyn ApprovalPolicy>,
    pub store: Arc<dyn SessionStore>,
    pub sink: Arc<dyn EventSink>,
    pub orchestrator: Arc<dyn Orchestrator>,
    pub ids: Arc<dyn IdGenerator>,
    pub command_capacity: usize,
    pub event_capacity: usize,
}

pub struct AgentRuntime {
    parts: RuntimeParts,
}

impl AgentRuntime {
    pub(crate) fn new(parts: RuntimeParts) -> Self {
        Self { parts }
    }

    pub fn active_profile(&self) -> &str {
        &self.parts.active_profile
    }

    pub fn registered_tools(&self) -> Vec<&str> {
        self.parts.tools.keys().map(String::as_str).collect()
    }

    pub fn parts(&self) -> &RuntimeParts {
        &self.parts
    }

    pub fn into_parts(self) -> RuntimeParts {
        self.parts
    }

    pub fn start(
        &self,
        session: SessionMetadata,
        replay: Vec<EventEnvelope>,
    ) -> Result<
        (
            kurama_core::engine::EngineHandle,
            kurama_core::engine::RuntimeEvents,
        ),
        KuramaError,
    > {
        let (profile, backend) = self
            .parts
            .profiles
            .get(&self.parts.active_profile)
            .ok_or_else(|| KuramaError::Configuration("active profile disappeared".into()))?;
        let workspace_root = std::path::PathBuf::from(&session.project_root);
        kurama_core::engine::Engine::spawn(
            kurama_core::engine::EngineConfig {
                session,
                profile: profile.clone(),
                backend: backend.clone(),
                tools: self.parts.tools.values().cloned().collect(),
                policy: self.parts.policy.clone(),
                store: self.parts.store.clone(),
                sink: self.parts.sink.clone(),
                orchestrator: self.parts.orchestrator.clone(),
                ids: self.parts.ids.clone(),
                context_policy: kurama_core::context::ContextPolicy {
                    max_input_tokens: profile.max_input_tokens,
                    reserve_output_tokens: profile.max_output_tokens,
                    ..kurama_core::context::ContextPolicy::default()
                },
                workspace_root: workspace_root.clone(),
                write_scope: WriteScope {
                    roots: vec![workspace_root],
                    files: Vec::new(),
                },
                auto: AutoBoundaries::default(),
                agent_id: None,
                orchestration: None,
                provider_retry_delays_ms: vec![250, 1_000],
            },
            replay,
        )
    }
}

impl fmt::Debug for AgentRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentRuntime")
            .field("active_profile", &self.parts.active_profile)
            .field("profiles", &self.parts.profiles.keys().collect::<Vec<_>>())
            .field("tools", &self.parts.tools.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}
