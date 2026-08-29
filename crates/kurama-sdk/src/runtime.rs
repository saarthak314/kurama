use std::{collections::BTreeMap, fmt, sync::Arc};

use kurama_protocol::{
    model::ModelProfile,
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
