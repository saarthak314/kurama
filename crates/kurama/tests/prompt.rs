use kurama::Kurama;
use kurama_core::testing::ScriptedBackend;
use kurama_protocol::model::{FinishReason, ModelEvent};

#[tokio::test]
async fn from_backend_prompt_returns_text() {
    let backend = ScriptedBackend::new(vec![vec![
        Ok(ModelEvent::TextDelta {
            text: "done".into(),
        }),
        Ok(ModelEvent::ResponseCompleted {
            cursor: None,
            finish_reason: FinishReason::Stop,
        }),
    ]]);
    let reply = Kurama::from_backend(backend)
        .no_tools()
        .ephemeral()
        .prompt("hello")
        .await
        .expect("prompt");
    assert_eq!(reply.text, "done");
}

#[test]
fn openai_constructor_keeps_the_named_profile() {
    let agent = Kurama::openai("sk-test")
        .expect("openai")
        .model("gpt-5.6")
        .no_tools()
        .ephemeral()
        .build()
        .expect("build");
    assert_eq!(agent.active_profile(), "openai");
}
