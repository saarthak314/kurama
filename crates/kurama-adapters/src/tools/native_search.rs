use futures_util::StreamExt;
use kurama_protocol::{KuramaError, model::ModelEvent, traits::CancelSignal};

use crate::{SearchResult, tools::web_search::parse_search_results};

use crate::bridges::{BridgeCommand, BridgeDecoder, MAX_JSONL_LINE_BYTES, event_stream};

pub(super) fn search_prompt(query: &str, limit: usize) -> String {
    format!(
        "Perform a live public web search for the query in the JSON below. Use the native web search tool; do not answer from memory. Do not read local files, run commands, or use other tools except structured output. Return up to {limit} relevant results as JSON with a results array of title, url, and snippet strings. Use actual source URLs returned by search; return an empty array only if the search found no relevant results.\n{}",
        serde_json::json!({"query": query, "limit": limit})
    )
}

pub(super) async fn run_search(
    command: BridgeCommand,
    decoder: impl BridgeDecoder,
    limit: usize,
    cancel: &dyn CancelSignal,
) -> Result<Vec<SearchResult>, KuramaError> {
    let mut stream = event_stream(command, decoder, cancel, Vec::new()).await?;
    let mut output = String::new();
    while let Some(event) = stream.next().await {
        if let ModelEvent::TextDelta { text } = event? {
            if output.len().saturating_add(text.len()) > MAX_JSONL_LINE_BYTES {
                return Err(KuramaError::Tool(
                    "native search output exceeds 1 MiB".into(),
                ));
            }
            output.push_str(&text);
        }
    }
    parse_search_results(&output, limit)
}
