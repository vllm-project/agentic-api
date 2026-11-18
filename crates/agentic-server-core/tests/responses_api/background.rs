//! Replays the `background` recordings. A background create returns `queued` and its job calls the model later, so
//! the mocked model answers each prompt with its recorded completion and holds the long prompt open until the gateway
//! drops it, as a cancel does.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::{Json, Router, routing::post};
use serde_json::Value;
use tokio::net::TcpListener;

use super::{cassette, counterpart, replay_turn, spawn_gateway};

/// The recorder's prompt for a turn that runs until it is cancelled.
const LONG_PROMPT: &str = "Write a story";
const WAIT: Duration = Duration::from_secs(10);

/// The text of the last user message in a request's `input`.
fn last_user_text(input: &Value) -> String {
    let message = input
        .as_array()
        .and_then(|items| items.iter().rev().find(|item| item["role"] == "user"))
        .expect("a user message");
    let content = &message["content"];
    content
        .as_str()
        .or_else(|| content[0]["text"].as_str())
        .expect("user text")
        .to_owned()
}

fn is_terminal(response: &Value) -> bool {
    matches!(response["status"].as_str(), Some("completed" | "incomplete"))
}

/// Each prompt's recorded completion: a foreground response, or a background one as a later retrieval returned it.
fn completions(turns: &[Value]) -> HashMap<String, Value> {
    let mut prompts = HashMap::new();
    let mut completions = HashMap::new();
    for turn in turns {
        let (request, body) = (&turn["request"], &turn["response"]["body"]);
        if turn["response"]["status_code"] != 200 {
            continue;
        }
        if request["method"] == "POST" && request["path"] == "/v1/responses" {
            let prompt = last_user_text(&request["body"]["input"]);
            prompts.insert(body["id"].as_str().unwrap().to_owned(), prompt.clone());
            if is_terminal(body) {
                completions.insert(prompt, body.clone());
            }
        } else if request["method"] == "GET"
            && is_terminal(body)
            && let Some(prompt) = body["id"].as_str().and_then(|id| prompts.get(id))
        {
            completions.insert(prompt.clone(), body.clone());
        }
    }
    completions
}

/// A model that answers by prompt and gives every answer new item IDs, as a real model does.
async fn spawn_model(completions: HashMap<String, Value>) -> (String, tokio::task::JoinHandle<()>) {
    let completions = Arc::new(completions);
    let calls = Arc::new(AtomicUsize::new(0));
    let router = Router::new().route(
        "/v1/responses",
        post(move |Json(request): Json<Value>| {
            let (completions, calls) = (Arc::clone(&completions), Arc::clone(&calls));
            async move {
                let prompt = last_user_text(&request["input"]);
                if prompt.starts_with(LONG_PROMPT) {
                    std::future::pending::<()>().await;
                }
                let mut body = completions
                    .get(&prompt)
                    .unwrap_or_else(|| panic!("no recorded completion for {prompt:?}"))
                    .clone();
                let call = calls.fetch_add(1, Ordering::SeqCst);
                for item in body["output"].as_array_mut().unwrap() {
                    if let Some(id) = item["id"].as_str() {
                        item["id"] = format!("{id}{call}").into();
                    }
                }
                Json(body)
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, server)
}

/// Poll the gateway until a background response leaves `queued` and `in_progress`, as the recorder polled the provider
/// before its next call.
async fn wait_finished(client: &reqwest::Client, url: &str, id: &str) {
    let polled = tokio::time::timeout(WAIT, async {
        loop {
            let text = client
                .get(format!("{url}/v1/responses/{id}"))
                .bearer_auth("test-key")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            let response: Value = serde_json::from_str(&text).unwrap();
            if !matches!(response["status"].as_str(), Some("queued" | "in_progress")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(polled.is_ok(), "{id} did not finish");
}

async fn replay_background(provider: &str, turns: &[Value]) {
    let (model_url, model) = spawn_model(completions(turns)).await;
    let home = std::env::temp_dir().join(format!("responses-replay-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir(&home).unwrap();
    let client = reqwest::Client::builder().timeout(WAIT).build().unwrap();
    let (url, mut gateway) = spawn_gateway(&model_url, &home, &client).await;
    let reference = cassette("background", "openai").expect("OpenAI reference is required for error contracts");
    let mut ids = HashMap::<String, String>::new();
    for (index, turn) in turns.iter().enumerate() {
        let location = format!("background/{provider} t{}", index + 1);
        let expected = if provider == "openai" {
            turn
        } else {
            counterpart(&reference, index, turn)
        };
        replay_turn(&client, &url, turn, expected, &mut ids, &location).await;
        let request = &turn["request"];
        if request["body"]["background"] == true
            && turn["response"]["status_code"] == 200
            && !last_user_text(&request["body"]["input"]).starts_with(LONG_PROMPT)
        {
            let recorded = turn["response"]["body"]["id"].as_str().unwrap();
            wait_finished(&client, &url, &ids[recorded]).await;
        }
    }
    gateway.kill().await.unwrap();
    gateway.wait().await.unwrap();
    model.abort();
    let _ = model.await;
    std::fs::remove_dir_all(&home).unwrap();
}

/// The recorded `store: false` background request, the last turn, is not replayed: `OpenAI` queues it, while the
/// gateway rejects it with `400` until it can keep an unstored response for polling.
#[tokio::test]
async fn replay_background_openai() {
    let turns = cassette("background", "openai").expect("required Responses recording missing");
    let last = turns.last().unwrap();
    assert_eq!(last["request"]["body"]["store"], false);
    assert_eq!(last["response"]["body"]["status"], "queued");
    replay_background("openai", &turns[..turns.len() - 1]).await;
}
