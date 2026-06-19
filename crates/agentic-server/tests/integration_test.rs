#[allow(dead_code)]
mod common;

use common::{spawn_vllm_recording, start_gateway_with_ogx_base};
use serde::Deserialize;
use serde_json::Value;

const FILE_SEARCH_VLLM_CASSETTE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/cassettes/file_search/vllm-file-search-openai-gpt-oss-20b.json"
);

#[derive(Debug, Deserialize)]
struct VllmCassette {
    turns: Vec<VllmTurn>,
}

#[derive(Debug, Deserialize)]
struct VllmTurn {
    response: VllmResponse,
}

#[derive(Debug, Deserialize)]
struct VllmResponse {
    status_code: u16,
    body: Value,
}

fn ogx_base_url() -> Option<String> {
    std::env::var("OGX_BASE_URL").ok()
}

fn load_file_search_vllm_responses() -> Vec<Value> {
    let text = std::fs::read_to_string(FILE_SEARCH_VLLM_CASSETTE)
        .unwrap_or_else(|err| panic!("failed to read cassette {FILE_SEARCH_VLLM_CASSETTE}: {err}"));
    let cassette: VllmCassette = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("failed to parse cassette {FILE_SEARCH_VLLM_CASSETTE}: {err}"));
    cassette
        .turns
        .into_iter()
        .map(|turn| {
            assert_eq!(turn.response.status_code, 200, "cassette response should be successful");
            turn.response.body
        })
        .collect()
}

fn output_text(body: &Value) -> String {
    body["output"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .filter_map(|content| content["text"].as_str())
        .collect()
}

async fn find_embedding_model(client: &reqwest::Client, ogx_url: &str) -> (String, u64) {
    let models_resp = client.get(format!("{ogx_url}/v1/models")).send().await.unwrap();
    let models: serde_json::Value = models_resp.json().await.unwrap();
    let embedding_model = models["data"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|m| m["custom_metadata"]["model_type"].as_str() == Some("embedding"))
        })
        .expect("OGx should have at least one embedding model")
        .clone();
    let model_id = embedding_model["id"].as_str().unwrap().to_owned();
    let dim = embedding_model["custom_metadata"]["embedding_dimension"]
        .as_u64()
        .unwrap();
    (model_id, dim)
}

async fn create_vector_store(client: &reqwest::Client, ogx_url: &str, model_id: &str, dim: u64) -> String {
    let vs_resp = client
        .post(format!("{ogx_url}/v1/vector_stores"))
        .json(&serde_json::json!({
            "name": "integration-test-docs",
            "metadata": { "embedding_model": model_id, "embedding_dimension": dim }
        }))
        .send()
        .await
        .unwrap();
    assert!(vs_resp.status().is_success(), "Failed to create vector store");
    let vs: serde_json::Value = vs_resp.json().await.unwrap();
    vs["id"].as_str().unwrap().to_owned()
}

async fn upload_and_attach(client: &reqwest::Client, ogx_url: &str, vs_id: &str) {
    let file_content = "Rust enforces memory safety without a garbage collector through its ownership system with borrowing and lifetimes. The borrow checker ensures references do not outlive the data they point to.";

    let form = reqwest::multipart::Form::new().text("purpose", "assistants").part(
        "file",
        reqwest::multipart::Part::text(file_content.to_owned())
            .file_name("rust-memory-safety.txt")
            .mime_str("text/plain")
            .unwrap(),
    );

    let file_resp = client
        .post(format!("{ogx_url}/v1/files"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert!(file_resp.status().is_success(), "Failed to upload file");

    let file: serde_json::Value = file_resp.json().await.unwrap();
    let file_id = file["id"].as_str().unwrap();
    eprintln!("Uploaded file: {file_id}");

    let attach_resp = client
        .post(format!("{ogx_url}/v1/vector_stores/{vs_id}/files"))
        .json(&serde_json::json!({"file_id": file_id}))
        .send()
        .await
        .unwrap();
    assert!(attach_resp.status().is_success(), "Failed to attach file");

    let attach: serde_json::Value = attach_resp.json().await.unwrap();
    let status = attach["status"].as_str().unwrap_or("unknown");
    assert_eq!(
        status,
        "completed",
        "File attachment failed: {}",
        attach
            .get("last_error")
            .map_or("none".to_owned(), std::string::ToString::to_string)
    );
}

async fn assert_gateway_file_search_uses_ogx(client: &reqwest::Client, ogx_url: &str, vs_id: &str) {
    let (vllm_port, requests, _vllm_handle) = spawn_vllm_recording(load_file_search_vllm_responses()).await;
    let (gateway_addr, _) = start_gateway_with_ogx_base(vllm_port, ogx_url, None).await;

    let gateway_resp = client
        .post(format!("http://{gateway_addr}/v1/responses"))
        .json(&serde_json::json!({
            "model": "model-a",
            "input": "How does Rust provide memory safety?",
            "tools": [{"type": "file_search", "vector_store_ids": [vs_id]}]
        }))
        .send()
        .await
        .unwrap();
    let gateway_status = gateway_resp.status();
    let gateway_body: Value = gateway_resp.json().await.unwrap();
    assert!(
        gateway_status.is_success(),
        "gateway file_search failed: {gateway_body}"
    );

    let answer = output_text(&gateway_body);
    assert!(
        answer.contains("ownership"),
        "gateway should return the recorded final vLLM answer, got: {answer}"
    );

    let requests = requests.lock().await;
    assert_eq!(
        requests.len(),
        2,
        "gateway should call vLLM before and after OGX search"
    );

    let first_tools = requests[0]["tools"]
        .as_array()
        .expect("first request should include tools");
    assert_eq!(first_tools[0]["type"], "function");
    assert_eq!(first_tools[0]["name"], "file_search");

    let second_input = requests[1]["input"]
        .as_array()
        .expect("second request should include input items");
    let tool_output = second_input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .expect("gateway should append function_call_output");
    let output = tool_output["output"]
        .as_str()
        .expect("tool output should be a JSON string");
    let output_json: serde_json::Value = serde_json::from_str(output).expect("tool output should parse as JSON");
    let gateway_results = output_json["results"]
        .as_array()
        .expect("tool output should include results");
    assert!(
        !gateway_results.is_empty(),
        "gateway should pass OGX search results back to vLLM"
    );
}

#[tokio::test]
async fn test_vector_search_with_ogx() {
    let Some(ogx_url) = ogx_base_url() else {
        eprintln!("Skipping: OGX_BASE_URL not set");
        return;
    };

    let client = reqwest::Client::new();

    let (model_id, dim) = find_embedding_model(&client, &ogx_url).await;
    eprintln!("Using embedding model: {model_id} (dim={dim})");

    let vs_id = create_vector_store(&client, &ogx_url, &model_id, dim).await;
    eprintln!("Created vector store: {vs_id}");

    upload_and_attach(&client, &ogx_url, &vs_id).await;

    let search_resp = client
        .post(format!("{ogx_url}/v1/vector_stores/{vs_id}/search"))
        .json(&serde_json::json!({
            "query": "memory safety ownership",
            "max_num_results": 2
        }))
        .send()
        .await
        .unwrap();
    assert!(search_resp.status().is_success(), "Search failed");

    let results: serde_json::Value = search_resp.json().await.unwrap();
    let data = results["data"].as_array().expect("search should return data array");
    assert!(!data.is_empty(), "search should return at least one result");

    let top_result = &data[0];
    let score = top_result["score"].as_f64().unwrap_or(0.0);
    assert!(score > 0.0, "top result should have a positive score");

    let content = top_result["content"]
        .as_array()
        .and_then(|c| c.first())
        .and_then(|c| c["text"].as_str())
        .unwrap_or("");
    assert!(!content.is_empty(), "top result should have content text");

    eprintln!("Search returned {} results, top score: {score:.3}", data.len());
    eprintln!("Top result: {content}");

    assert_gateway_file_search_uses_ogx(&client, &ogx_url, &vs_id).await;
}
