use std::sync::Arc;

use kurama_core::testing::{
    AllowAllPolicy, CollectingSink, EchoTool, MemoryStore, NoDelegation, ScriptedBackend,
    SequenceIds,
};
use kurama_sdk::{AgentBuilder, ModelProfile};

#[test]
fn sdk_accepts_custom_runtime_parts() {
    let runtime = AgentBuilder::new()
        .profile(
            ModelProfile::new("custom", "frontier", 32_000, 4_000),
            Arc::new(ScriptedBackend::new(Vec::new())),
        )
        .tool(Arc::new(EchoTool))
        .policy(Arc::new(AllowAllPolicy))
        .store(Arc::new(MemoryStore::default()))
        .sink(Arc::new(CollectingSink::default()))
        .orchestrator(Arc::new(NoDelegation))
        .ids(Arc::new(SequenceIds::default()))
        .build()
        .expect("runtime");

    assert_eq!(runtime.active_profile(), "custom");
    assert_eq!(runtime.registered_tools(), vec!["echo"]);
}

#[test]
fn duplicate_tools_are_rejected() {
    let error = AgentBuilder::new()
        .tool(Arc::new(EchoTool))
        .tool(Arc::new(EchoTool))
        .build()
        .expect_err("duplicate tool");
    assert!(error.to_string().contains("duplicate tool"));
}
