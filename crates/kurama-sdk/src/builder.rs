use std::{collections::BTreeMap, sync::Arc};

use kurama_protocol::{
    KuramaError,
    agent::{OrchestrationContext, WriteScope},
    model::ModelProfile,
    policy::AutoBoundaries,
    traits::{
        ApprovalPolicy, EventSink, IdGenerator, ModelBackend, Orchestrator, SessionStore, Tool,
    },
};

use crate::runtime::{AgentRuntime, RuntimeParts};

#[derive(Default)]
pub struct AgentBuilder {
    profiles: BTreeMap<String, (ModelProfile, Arc<dyn ModelBackend>)>,
    active_profile: Option<String>,
    tools: BTreeMap<String, Arc<dyn Tool>>,
    policy: Option<Arc<dyn ApprovalPolicy>>,
    store: Option<Arc<dyn SessionStore>>,
    sink: Option<Arc<dyn EventSink>>,
    orchestrator: Option<Arc<dyn Orchestrator>>,
    ids: Option<Arc<dyn IdGenerator>>,
    command_capacity: usize,
    event_capacity: usize,
    write_scope: Option<WriteScope>,
    auto: AutoBoundaries,
    orchestration: Option<OrchestrationContext>,
    provider_retry_delays_ms: Vec<u64>,
    error: Option<String>,
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            command_capacity: 32,
            event_capacity: 128,
            provider_retry_delays_ms: vec![250, 1_000],
            ..Self::default()
        }
    }

    pub fn profile(mut self, profile: ModelProfile, backend: Arc<dyn ModelBackend>) -> Self {
        if self.profiles.contains_key(&profile.name) {
            self.error = Some(format!("duplicate profile: {}", profile.name));
            return self;
        }
        if self.active_profile.is_none() {
            self.active_profile = Some(profile.name.clone());
        }
        self.profiles
            .insert(profile.name.clone(), (profile, backend));
        self
    }

    pub fn active_profile(mut self, name: impl Into<String>) -> Self {
        self.active_profile = Some(name.into());
        self
    }

    pub fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        let name = tool.descriptor().name;
        if self.tools.contains_key(&name) {
            self.error = Some(format!("duplicate tool: {name}"));
            return self;
        }
        self.tools.insert(name, tool);
        self
    }

    pub fn policy(mut self, policy: Arc<dyn ApprovalPolicy>) -> Self {
        self.policy = Some(policy);
        self
    }

    pub fn store(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn orchestrator(mut self, orchestrator: Arc<dyn Orchestrator>) -> Self {
        self.orchestrator = Some(orchestrator);
        self
    }

    pub fn ids(mut self, ids: Arc<dyn IdGenerator>) -> Self {
        self.ids = Some(ids);
        self
    }

    pub fn channel_capacities(mut self, command_capacity: usize, event_capacity: usize) -> Self {
        self.command_capacity = command_capacity;
        self.event_capacity = event_capacity;
        self
    }

    pub fn write_scope(mut self, write_scope: WriteScope) -> Self {
        self.write_scope = Some(write_scope);
        self
    }

    pub fn auto_boundaries(mut self, auto: AutoBoundaries) -> Self {
        self.auto = auto;
        self
    }

    pub fn orchestration_context(mut self, orchestration: OrchestrationContext) -> Self {
        self.orchestration = Some(orchestration);
        self
    }

    pub fn provider_retry_delays_ms(mut self, delays: Vec<u64>) -> Self {
        self.provider_retry_delays_ms = delays;
        self
    }

    pub fn build(self) -> Result<AgentRuntime, KuramaError> {
        if let Some(error) = self.error {
            return Err(KuramaError::Configuration(error));
        }
        if self.command_capacity == 0 || self.event_capacity == 0 {
            return Err(KuramaError::Configuration(
                "channel capacities must be non-zero".into(),
            ));
        }

        let active_profile = self.active_profile.ok_or_else(|| {
            KuramaError::Configuration("an active model profile is required".into())
        })?;
        if !self.profiles.contains_key(&active_profile) {
            return Err(KuramaError::Configuration(format!(
                "unknown active profile: {active_profile}"
            )));
        }

        Ok(AgentRuntime::new(RuntimeParts {
            profiles: self.profiles,
            active_profile,
            tools: self.tools,
            policy: self
                .policy
                .ok_or_else(|| KuramaError::Configuration("approval policy is required".into()))?,
            store: self
                .store
                .ok_or_else(|| KuramaError::Configuration("session store is required".into()))?,
            sink: self
                .sink
                .ok_or_else(|| KuramaError::Configuration("event sink is required".into()))?,
            orchestrator: self
                .orchestrator
                .ok_or_else(|| KuramaError::Configuration("orchestrator is required".into()))?,
            ids: self
                .ids
                .ok_or_else(|| KuramaError::Configuration("ID generator is required".into()))?,
            command_capacity: self.command_capacity,
            event_capacity: self.event_capacity,
            write_scope: self.write_scope,
            auto: self.auto,
            orchestration: self.orchestration,
            provider_retry_delays_ms: self.provider_retry_delays_ms,
        }))
    }
}
