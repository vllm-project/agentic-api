//! Stateful gateway usage — demonstrates both storage-backed multi-turn flows.
//!
//! **Part A — Responses flow** (`previous_response_id`):
//!   Turn 1 non-streaming: plant a keyword.
//!   Turn 2 streaming:     recall it via `previous_response_id`.
//!
//! **Part B — Conversation flow** (`conversation_id`):
//!   Create a conversation, then two turns (non-streaming + streaming)
//!   sharing the same `conversation_id`.
//!
//! # Step 1 — start the gateway (pointing at your vLLM host):
//! ```bash
//! cargo run -p agentic-server -- --llm-api-base http://localhost:8000
//! ```
//!
//! # Step 2 — run the example against the running gateway:
//! ```bash
//! GATEWAY_URL=http://localhost:9000 \
//! MODEL=Qwen/Qwen3-30B-A3B-FP8 \
//!     cargo run --example stateful_gateway -p agentic-server
//! ```

use futures::StreamExt;

/// Send a stored, non-streaming request; return the full response JSON.
async fn post_blocking(
    client: &reqwest::Client,
    gateway: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    Ok(client
        .post(format!("{gateway}/v1/responses"))
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Consume an SSE `bytes_stream` from the gateway.
///
/// The gateway sends one complete `response` JSON object then `[DONE]`.
/// Buffers across TCP chunk boundaries. Returns `(response_id, reply_text)`.
async fn drain_sse(
    mut stream: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let mut buf = String::new();
    let mut response_id = String::new();
    let mut reply = String::new();

    while let Some(chunk) = stream.next().await {
        buf.push_str(std::str::from_utf8(&chunk?).unwrap_or_default());

        while let Some(pos) = buf.find('\n') {
            let line: String = buf.drain(..=pos).collect();
            let line = line.trim_end_matches(['\r', '\n']);

            if line == "data: [DONE]" {
                return Ok((response_id, reply));
            }
            if let Some(data) = line.strip_prefix("data: ")
                && let Ok(json) = serde_json::from_str::<serde_json::Value>(data)
            {
                // Gateway returns the accumulated complete response as one event.
                if json["object"].as_str() == Some("response") {
                    json["id"].as_str().unwrap_or_default().clone_into(&mut response_id);
                    json["output"][0]["content"][0]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .clone_into(&mut reply);
                }
            }
        }
    }

    Ok((response_id, reply))
}

/// Send a stored, streaming request; return `(response_id, reply_text)`.
async fn post_streaming(
    client: &reqwest::Client,
    gateway: &str,
    body: serde_json::Value,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let stream = client
        .post(format!("{gateway}/v1/responses"))
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .bytes_stream();

    drain_sse(stream).await
}

async fn responses_flow(
    client: &reqwest::Client,
    gateway: &str,
    model: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("══ Part A — responses flow (previous_response_id) ══\n");

    // Turn 1: non-streaming — plant the keyword
    println!("--- turn 1 (non-streaming) ---");
    let t1 = post_blocking(
        client,
        gateway,
        serde_json::json!({
            "model": model,
            "input": [{"type": "message", "role": "user", "content": "Please remember the keyword MANGO. Acknowledge with exactly: OK"}],
            "store": true,
            "stream": false
        }),
    )
    .await?;

    let t1_id = t1["id"].as_str().unwrap_or_default().to_owned();
    println!("response_id : {t1_id}");
    println!("reply       : {}\n", t1["output"][0]["content"][0]["text"]);

    // Turn 2: streaming — recall the keyword
    println!("--- turn 2 (streaming) ---");
    let (t2_id, t2_reply) = post_streaming(
        client,
        gateway,
        serde_json::json!({
            "model": model,
            "input": [{"type": "message", "role": "user", "content": "What keyword did I ask you to remember?"}],
            "store": true,
            "stream": true,
            "previous_response_id": t1_id
        }),
    )
    .await?;

    println!("reply       : {t2_reply}");
    println!("response_id : {t2_id}");

    Ok(())
}

async fn conversation_flow(
    client: &reqwest::Client,
    gateway: &str,
    model: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("\n══ Part B — conversation flow (conversation_id) ══\n");

    // Create conversation
    println!("--- create conversation ---");
    let conv: serde_json::Value = client
        .post(format!("{gateway}/v1/conversations"))
        .json(&serde_json::json!({ "model": model }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let conv_id = conv["id"].as_str().unwrap_or_default().to_owned();
    println!("conversation_id : {conv_id}\n");

    // Turn 1: non-streaming — plant the keyword
    println!("--- turn 1 (non-streaming) ---");
    let t1 = post_blocking(
        client,
        gateway,
        serde_json::json!({
            "model": model,
            "input": [{"type": "message", "role": "user", "content": "Please remember the keyword PAPAYA. Acknowledge with exactly: OK"}],
            "store": true,
            "stream": false,
            "conversation_id": conv_id
        }),
    )
    .await?;

    let t1_id = t1["id"].as_str().unwrap_or_default().to_owned();
    println!("response_id : {t1_id}");
    println!("reply       : {}\n", t1["output"][0]["content"][0]["text"]);

    // Turn 2: streaming — recall the keyword
    println!("--- turn 2 (streaming) ---");
    let (t2_id, t2_reply) = post_streaming(
        client,
        gateway,
        serde_json::json!({
            "model": model,
            "input": [{"type": "message", "role": "user", "content": "What keyword did I ask you to remember?"}],
            "store": true,
            "stream": true,
            "conversation_id": conv_id
        }),
    )
    .await?;

    println!("reply       : {t2_reply}");
    println!("response_id : {t2_id}");

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gateway = std::env::var("GATEWAY_URL").unwrap_or_else(|_| "http://localhost:9000".into());
    let model = std::env::var("MODEL").unwrap_or_else(|_| "default".into());

    let client = reqwest::Client::new();

    responses_flow(&client, &gateway, &model).await?;
    conversation_flow(&client, &gateway, &model).await?;

    Ok(())
}
