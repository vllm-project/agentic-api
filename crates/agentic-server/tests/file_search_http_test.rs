#[allow(dead_code)]
mod common;

use std::sync::Arc;

use agentic_core::executor::ExecutionContext;
use common::{spawn_gateway, test_config, test_state};
use http::StatusCode;
use serde_json::{Value, json};

struct Gateway {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn gateway() -> Gateway {
    let mut config = test_config("http://127.0.0.1:1");
    config.db_url = Some("sqlite::memory:".into());
    let mut state = test_state(&config);
    state.exec_ctx = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let (url, task) = spawn_gateway(state).await;
    Gateway { url, task }
}

fn multipart(filename: &str, text: &str) -> String {
    format!(
        "--upload\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nassistants\r\n--upload\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: text/plain\r\n\r\n{text}\r\n--upload--\r\n"
    )
}

#[tokio::test]
async fn uploaded_file_is_searchable_and_deletion_removes_its_chunks() {
    let server = gateway().await;
    let client = reqwest::Client::new();
    let text = "The northern office opens at seven. The southern office opens at nine.";
    let response = client
        .post(format!("{}/v1/files", server.url))
        .header("content-type", "multipart/form-data; boundary=upload")
        .body(multipart("offices.txt", text))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let file: Value = response.json().await.unwrap();
    assert_eq!(file["filename"], "offices.txt");
    assert_eq!(file["bytes"], text.len());
    let file_id = file["id"].as_str().unwrap();

    let store: Value = client
        .post(format!("{}/v1/vector_stores", server.url))
        .json(&json!({"name":"offices"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let store_id = store["id"].as_str().unwrap();
    let attachment: Value = client
        .post(format!("{}/v1/vector_stores/{store_id}/files", server.url))
        .json(&json!({"file_id":file_id,"attributes":{"region":"north"}}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(attachment["status"], "completed");

    let result: Value = client.post(format!("{}/v1/vector_stores/{store_id}/search", server.url))
        .json(&json!({"query":"northern office","search_mode":"keyword","filters":{"type":"eq","key":"region","value":"north"}}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(result["data"][0]["file_id"], file_id);
    assert_eq!(result["data"][0]["filename"], "offices.txt");
    assert!(
        result["data"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("seven")
    );

    let bytes = client
        .get(format!("{}/v1/files/{file_id}/content", server.url))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), text.as_bytes());
    let deleted: Value = client
        .delete(format!("{}/v1/files/{file_id}", server.url))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(deleted["deleted"], true);
    assert_eq!(
        client
            .get(format!("{}/v1/files/{file_id}", server.url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let result: Value = client
        .post(format!("{}/v1/vector_stores/{store_id}/search", server.url))
        .json(&json!({"query":"northern office","search_mode":"keyword"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["data"], json!([]));
}

#[tokio::test]
async fn file_search_http_rejects_invalid_upload_search_and_pagination() {
    let server = gateway().await;
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .post(format!("{}/v1/files", server.url))
            .header("content-type", "multipart/form-data; boundary=upload")
            .body("--upload--\r\n")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .get(format!("{}/v1/files?limit=0", server.url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .post(format!("{}/v1/vector_stores/missing/search", server.url))
            .json(&json!({"query":""}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
}
