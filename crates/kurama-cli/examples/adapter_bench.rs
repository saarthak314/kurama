//! Deterministic CPU/I/O workloads for the production adapters, without credentials.
//! Run the same release binary sequentially before/after a change; retain raw samples.
use std::{
    hint::black_box,
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use kurama_adapters::{BoundedOutput, OpenAiCompatBackend, ReadTool, SseDecoder, html_to_text};
use kurama_core::cancel::CancelToken;
use kurama_protocol::{
    agent::WriteScope,
    id::SessionId,
    model::{ModelItem, ModelProfile, ModelRequest},
    policy::ExecutionMode,
    tool::{ToolContext, ToolDescriptor, ToolInvocation, ToolLimits},
    traits::Tool,
};
use serde_json::{Value, json};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn measure(
    name: &str,
    iterations: usize,
    mut work: impl FnMut() -> Result<usize>,
) -> Result<Value> {
    let expected = work()?;
    let mut samples = Vec::with_capacity(5);
    for _ in 0..5 {
        let start = Instant::now();
        for _ in 0..iterations {
            if black_box(work()?) != expected {
                return Err(format!("{name}: output changed between repetitions").into());
            }
        }
        samples.push(start.elapsed().as_secs_f64() * 1000.0 / iterations as f64);
    }
    let mut sorted = samples.clone();
    sorted.sort_by(f64::total_cmp);
    Ok(json!({
        "name": name,
        "iterations_per_sample": iterations,
        "samples_ms": samples,
        "median_ms": sorted[2],
        "observable_size": expected,
    }))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut results = Vec::new();
    let fragmented = format!("data: {}\n\n", "x".repeat(32 * 1024));
    results.push(measure("sse_byte_fragmented_32k", 1, || {
        let mut decoder = SseDecoder::default();
        let mut bytes = 0;
        for byte in fragmented.as_bytes() {
            for event in decoder.push(std::slice::from_ref(byte))? {
                bytes += event.data.len();
            }
        }
        for event in decoder.finish()? {
            bytes += event.data.len();
        }
        if bytes != 32 * 1024 {
            return Err("fragmented SSE lost data".into());
        }
        Ok(bytes)
    })?);
    let burst = "data: {\"value\":1}\n\n".repeat(10_000);
    results.push(measure("sse_burst_10k_records", 3, || {
        let mut decoder = SseDecoder::default();
        let events = decoder.push(black_box(burst.as_bytes()))?;
        if events.len() != 10_000 {
            return Err("SSE burst lost events".into());
        }
        Ok(black_box(events).len())
    })?);
    let html = "<span>value</span>".repeat(120_000);
    results.push(measure("html_tag_dense_2mb", 3, || {
        Ok(black_box(html_to_text(black_box(&html))).len())
    })?);
    let request = ModelRequest {
        session_id: SessionId::from("benchmark"),
        agent_id: None,
        workspace_root: "/benchmark".into(),
        profile: ModelProfile::new("fixture", "fixture", 2_000_000, 8_000),
        system: "Fixture system instructions".into(),
        items: (0..200).map(|_| ModelItem::User { text: "x".repeat(8192) }).collect(),
        tools: (0..32).map(|index| ToolDescriptor {
            name: format!("fixture_{index}"),
            description: "Fixture tool".into(),
            parameters: json!({"type":"object", "description":"x".repeat(32768), "properties":{}}),
        }).collect(),
        delegation: None,
        continuation: None,
    };
    results.push(measure("compatible_large_request", 8, || {
        let body = OpenAiCompatBackend::request_body(black_box(&request), None);
        let count = body["messages"].as_array().ok_or("messages missing")?.len();
        if count != 201 || body["tools"].as_array().map(Vec::len) != Some(32) {
            return Err("request content missing".into());
        }
        black_box(body);
        Ok(count)
    })?);
    let staging = tempfile::tempdir()?;
    let path = staging.path().join("display");
    let small = "small output line\n".repeat(128);
    results.push(measure("small_staged_output", 32, || {
        let mut output = BoundedOutput::with_staging(ToolLimits::default(), &path)?;
        output.push(black_box(small.as_bytes()));
        let bounded = output.finish();
        if bounded.truncated || bounded.text != small {
            return Err("small output changed".into());
        }
        if let Some(staged) = &bounded.staged_path {
            std::fs::remove_file(staged)?;
        }
        Ok(black_box(bounded).text.len())
    })?);
    let large = "output line with several columns\n".repeat(32_768);
    results.push(measure("bounded_output_1mb", 8, || {
        let mut output = BoundedOutput::new(ToolLimits {
            max_bytes: 4096,
            max_lines: 64,
        });
        for chunk in black_box(large.as_bytes()).chunks(4096) {
            output.push(chunk);
        }
        let bounded = output.finish();
        if !bounded.truncated || bounded.text.len() > 4096 || bounded.total_bytes != large.len() {
            return Err("output limits or accounting changed".into());
        }
        Ok(black_box(bounded).total_bytes)
    })?);
    drop(request);
    drop(html);
    drop(burst);
    drop(fragmented);
    drop(large);
    drop(small);
    drop(staging);
    let workspace = tempfile::tempdir()?;
    let mut fixture = std::fs::File::create(workspace.path().join("dense.txt"))?;
    let block = b"x\n".repeat(32768);
    for _ in 0..256 {
        fixture.write_all(&block)?;
    }
    drop(fixture);
    let context = ToolContext {
        session_id: "read-benchmark".into(),
        agent_id: None,
        cwd: workspace.path().to_owned(),
        workspace_root: workspace.path().to_owned(),
        mode: ExecutionMode::Supervised,
        limits: ToolLimits::default(),
        write_scope: WriteScope::default(),
    };
    let invocation = ToolInvocation {
        call_id: "read-call".into(),
        name: "read".into(),
        arguments: json!({"files":[{"path":"dense.txt","start_line":1,"end_line":1}]}),
    };
    let cancel = CancelToken::new();
    let tool = ReadTool::default();
    let gap = Arc::new(AtomicU64::new(0));
    let observed_gap = gap.clone();
    let heartbeat = tokio::spawn(async move {
        let mut previous = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(1)).await;
            let now = Instant::now();
            observed_gap.fetch_max(
                now.duration_since(previous).as_nanos() as u64,
                Ordering::Relaxed,
            );
            previous = now;
        }
    });
    tokio::task::yield_now().await;
    let mut samples = Vec::new();
    for repetition in 0..6 {
        let mut elapsed_ms = 0.0;
        for _ in 0..3 {
            let start = Instant::now();
            let result = tool
                .execute(context.clone(), invocation.clone(), &cancel)
                .await?;
            elapsed_ms += start.elapsed().as_secs_f64() * 1000.0;
            if result.truncated
                || !result.output.ends_with("x\n")
                || result.metadata["files"][0]["total_bytes"] != 16 * 1024 * 1024
                || result.metadata["files"][0]["sha256"]
                    != "e1adfdd50cd93c4edeffd0568bff414a84029e711ac8389e8650ad71020c4841"
            {
                return Err("selected read content/hash changed".into());
            }
            // Give the clock an observation point, outside the measured call.
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        if repetition > 0 {
            samples.push(elapsed_ms / 3.0);
        }
    }
    heartbeat.abort();
    let _ = heartbeat.await;
    let mut sorted = samples.clone();
    sorted.sort_by(f64::total_cmp);
    results.push(
        json!({"name":"read_first_line_16mb", "iterations_per_sample":3,
        "samples_ms":samples,"median_ms":sorted[2],"observable_size":16*1024*1024,
        "max_scheduler_gap_ms":gap.load(Ordering::Relaxed) as f64 / 1_000_000.0}),
    );
    println!(
        "{}",
        serde_json::to_string(&json!({"schema_version":1,"benchmarks":results}))?
    );
    Ok(())
}
