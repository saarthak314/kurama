//! Native Rust SDK reference for scripts/check-sdk.py's cross-language scenarios.
use std::{env, error::Error, path::PathBuf, sync::Arc};

use kurama_adapters::{
    BashTool, FsSessionStore, HttpClient, OpenAiCompatBackend, ReadTool, WebSearchTool, WriteTool,
};
use kurama_sdk::{Agent, Event, KuramaError, ModelProfile};
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let workspace = PathBuf::from(env::var("SDK_WORKSPACE")?).canonicalize()?;
    let store = Arc::new(FsSessionStore::open(PathBuf::from(env::var("SDK_STATE")?))?);
    let http = HttpClient::try_new()?;
    let backend =
        OpenAiCompatBackend::from_endpoint(http.clone(), &env::var("SDK_ENDPOINT")?, None)?;
    let mut agent = Agent::new()
        .workspace(workspace.clone())
        .profile(
            ModelProfile::new("fixture", "fixture-model", 32_000, 4_000),
            Arc::new(backend),
        )
        .store(store)
        .tool(BashTool::default())
        .tool(ReadTool::default())
        .tool(WebSearchTool::new(http, None))
        .tool(WriteTool::default())
        .orchestrate()
        .build()?;
    let simple = agent.prompt("SDK_SIMPLE").await?;
    assert_eq!(simple.text, "SDK_SIMPLE_OK");
    let session_id = simple.session_id;
    let mut replies = vec![simple.text];
    let mut streamed = String::new();
    {
        let mut turn = agent.turn("SDK_STREAM").await?;
        while let Some(event) = turn.next().await? {
            match event {
                Event::Text(text) => streamed.push_str(&text),
                Event::Done(_) => break,
                Event::Error(message) => return Err(message.into()),
                _ => {}
            }
        }
    }
    assert_eq!(streamed, "stream 世界\nfinished");
    replies.push(streamed);
    let mut approvals = 0;
    let mut written = String::new();
    {
        let mut turn = agent.turn("SDK_WRITE").await?;
        while let Some(event) = turn.next().await? {
            match event {
                Event::Approval(request) => {
                    approvals += 1;
                    turn.approve_once(request.operation_id).await?;
                }
                Event::Text(text) => written.push_str(&text),
                Event::Done(_) => break,
                Event::Error(message) => return Err(message.into()),
                _ => {}
            }
        }
    }
    assert_eq!(approvals, 1);
    assert_eq!(written, "SDK_WRITE_DONE");
    assert_eq!(
        std::fs::read_to_string(workspace.join("sdk-result.txt"))?,
        "written\n"
    );
    replies.push(written);
    assert!(matches!(
        agent.prompt("SDK_NEEDS_APPROVAL").await,
        Err(KuramaError::Policy(_))
    ));
    let mut cancelled = false;
    {
        let mut turn = agent.turn("SDK_CANCEL").await?;
        while let Some(event) = turn.next().await? {
            if let Event::Text(text) = event
                && text.contains("SDK_CANCEL_BEGIN")
            {
                turn.cancel().await?;
                turn.drain().await;
                cancelled = true;
                break;
            }
        }
    }
    assert!(cancelled);
    let after = agent.prompt("SDK_AFTER_CANCEL").await?;
    assert_eq!(after.text, "SDK_AFTER_CANCEL_OK");
    replies.push(after.text);
    let delegated = agent.delegate("SDK_AGENTS").await?;
    assert_eq!(delegated.text, "SDK_AGENTS_DONE");
    replies.push(delegated.text);
    agent.resume(session_id.to_string()).await?;
    let resumed = agent.prompt("SDK_RESUME").await?;
    assert_eq!(resumed.session_id, session_id);
    assert_eq!(resumed.text, "SDK_RESUME_OK");
    replies.push(resumed.text);
    println!(
        "{}",
        json!({"language":"rust","session_id":session_id,"replies":replies,"manual_approvals":approvals})
    );
    Ok(())
}
