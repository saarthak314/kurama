use std::sync::Arc;

use kurama_protocol::traits::{
    ApprovalPolicy, EventSink, ModelBackend, Orchestrator, SessionStore, Tool,
};

fn accepts_objects(
    _: Arc<dyn ModelBackend>,
    _: Arc<dyn Tool>,
    _: Arc<dyn ApprovalPolicy>,
    _: Arc<dyn SessionStore>,
    _: Arc<dyn EventSink>,
    _: Arc<dyn Orchestrator>,
) {
}

type ExtensionObjects = fn(
    Arc<dyn ModelBackend>,
    Arc<dyn Tool>,
    Arc<dyn ApprovalPolicy>,
    Arc<dyn SessionStore>,
    Arc<dyn EventSink>,
    Arc<dyn Orchestrator>,
);

#[test]
fn public_extension_points_are_dyn_compatible() {
    let function: ExtensionObjects = accepts_objects;
    let _ = function;
}
