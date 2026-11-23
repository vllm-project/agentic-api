#[allow(dead_code)]
mod common;

use std::num::NonZeroUsize;
use std::sync::Arc;

use agentic_core::executor::ExecutionContext;
use common::{spawn_gateway, test_config, test_state};
use http::StatusCode;
use serde_json::{Value, json};

struct Gateway {
    url: String,
    task: tokio::task::JoinHandle<()>,
    files: tempfile::TempDir,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn gateway() -> Gateway {
    gateway_with(None, None).await
}

async fn gateway_with(api_key: Option<&str>, body_limit: Option<NonZeroUsize>) -> Gateway {
    let files = tempfile::tempdir().unwrap();
    let mut config = test_config("http://127.0.0.1:1");
    config.openai_api_key = api_key.map(str::to_owned);
    config.db_url = Some("sqlite::memory:".into());
    config.tools.file_search.files_storage_dir = Some(files.path().to_owned());
    let mut state = test_state(&config);
    if let Some(limit) = body_limit {
        state.max_request_body_size = limit;
    }
    state.exec_ctx = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let (url, task) = spawn_gateway(state).await;
    Gateway { url, task, files }
}

fn multipart(filename: &str, text: &str) -> Vec<u8> {
    multipart_with(filename, "text/plain", "assistants", text.as_bytes())
}

fn multipart_with(filename: &str, content_type: &str, purpose: &str, body: &[u8]) -> Vec<u8> {
    let mut encoded = format!(
        "--upload\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\n{purpose}\r\n--upload\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
    )
    .into_bytes();
    encoded.extend_from_slice(body);
    encoded.extend_from_slice(b"\r\n--upload--\r\n");
    encoded
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "covers authentication across upload, download, search, and deletion"
)]
async fn local_file_search_routes_require_the_configured_api_key() {
    let server = gateway_with(Some("secret"), None).await;
    let client = reqwest::Client::new();
    let files_url = format!("{}/v1/files", server.url);
    assert_eq!(
        client.get(&files_url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(&files_url)
            .bearer_auth("incorrect")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(&files_url)
            .bearer_auth("secret")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    let file = api_json(
        client
            .post(&files_url)
            .bearer_auth("secret")
            .header("content-type", "multipart/form-data; boundary=upload")
            .body(multipart("private.txt", "private content")),
    )
    .await;
    let file_resource_url = format!("{files_url}/{}", file["id"].as_str().unwrap());
    let store = api_json(
        client
            .post(format!("{}/v1/vector_stores", server.url))
            .bearer_auth("secret")
            .json(&json!({"file_ids":[file["id"]]})),
    )
    .await;
    let search_url = format!(
        "{}/v1/vector_stores/{}/search",
        server.url,
        store["id"].as_str().unwrap()
    );
    assert_eq!(
        client
            .get(format!("{file_resource_url}/content"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(&search_url)
            .json(&json!({"query":"private"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client.delete(&file_resource_url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let content = client
        .get(format!("{file_resource_url}/content"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap();
    assert_eq!(content.status(), StatusCode::BAD_REQUEST);
    let content: Value = content.json().await.unwrap();
    assert_eq!(
        content["error"]["message"],
        "Not allowed to download files of purpose: assistants"
    );
    assert_eq!(
        client
            .post(&search_url)
            .bearer_auth("secret")
            .json(&json!({"query":"private"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        client
            .delete(&file_resource_url)
            .bearer_auth("secret")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn local_file_search_routes_enforce_the_configured_body_limit() {
    let server = gateway_with(None, NonZeroUsize::new(256)).await;
    let client = reqwest::Client::new();
    let oversized_file = client
        .post(format!("{}/v1/files", server.url))
        .header("content-type", "multipart/form-data; boundary=upload")
        .body(multipart("large.txt", &"x".repeat(300)))
        .send()
        .await
        .unwrap();
    assert_eq!(oversized_file.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let oversized_file: Value = oversized_file.json().await.unwrap();
    assert_eq!(oversized_file["error"]["type"], "invalid_request_error");
    assert_eq!(oversized_file["error"]["code"], "body_too_large");
    assert_eq!(
        client
            .post(format!("{}/v1/vector_stores", server.url))
            .json(&json!({"name":"x".repeat(300)}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        client
            .post(format!("{}/v1/vector_stores", server.url))
            .json(&json!({"name":"small"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

async fn api_json(request: reqwest::RequestBuilder) -> Value {
    request
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn detaching_or_deleting_a_vector_store_preserves_the_uploaded_file() {
    let server = gateway().await;
    let client = reqwest::Client::new();
    let file = api_json(
        client
            .post(format!("{}/v1/files", server.url))
            .header("content-type", "multipart/form-data; boundary=upload")
            .body(multipart("retained.txt", "A retained document.")),
    )
    .await;
    let file_id = file["id"].as_str().unwrap();
    let file_url = format!("{}/v1/files/{file_id}", server.url);
    assert_eq!(api_json(client.get(&file_url)).await, file);
    assert_eq!(
        api_json(client.get(format!("{}/v1/files", server.url))).await["data"][0],
        file
    );

    let store = api_json(
        client
            .post(format!("{}/v1/vector_stores", server.url))
            .json(&json!({"name":"temporary collection","file_ids":[file_id]})),
    )
    .await;
    let store_id = store["id"].as_str().unwrap();
    let store_url = format!("{}/v1/vector_stores/{store_id}", server.url);
    assert_eq!(api_json(client.get(&store_url)).await, store);
    assert_eq!(
        api_json(client.get(format!("{}/v1/vector_stores", server.url))).await["data"][0],
        store
    );
    let attachment_url = format!("{store_url}/files/{file_id}");
    assert_eq!(api_json(client.get(&attachment_url)).await["id"], file_id);
    assert_eq!(
        api_json(client.get(format!("{store_url}/files"))).await["data"][0]["id"],
        file_id
    );
    assert_eq!(api_json(client.delete(&attachment_url)).await["deleted"], true);
    assert_eq!(
        api_json(client.get(format!("{store_url}/files"))).await["data"],
        json!([])
    );
    assert_eq!(api_json(client.get(&file_url)).await, file);

    api_json(
        client
            .post(format!("{store_url}/files"))
            .json(&json!({"file_id":file_id})),
    )
    .await;
    assert_eq!(api_json(client.delete(&store_url)).await["deleted"], true);
    assert_eq!(api_json(client.get(&file_url)).await, file);
    assert_eq!(
        client.get(format!("{file_url}/content")).send().await.unwrap().status(),
        StatusCode::BAD_REQUEST,
        "assistants files are not downloadable"
    );
    assert_eq!(
        tokio::fs::read(server.files.path().join(file_id)).await.unwrap(),
        b"A retained document."
    );
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
    assert_eq!(
        tokio::fs::read(server.files.path().join(file_id)).await.unwrap(),
        text.as_bytes()
    );

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

    let downloaded = client
        .get(format!("{}/v1/files/{file_id}/content", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(downloaded.status(), StatusCode::BAD_REQUEST);
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
    assert!(!server.files.path().join(file_id).exists());
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
    let missing = client
        .get(format!("{}/v1/files/file-{}", server.url, uuid::Uuid::nil()))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let missing: Value = missing.json().await.unwrap();
    assert_eq!(missing["error"]["type"], "invalid_request_error");
    assert_eq!(missing["error"]["code"], Value::Null);
    assert_eq!(missing["error"]["param"], "id");
    assert_eq!(
        missing["error"]["message"],
        format!("No such File object: file-{}", uuid::Uuid::nil())
    );
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
    // `purpose` belongs to GET /v1/files only; the shared ListParams must not
    // let it pass unnoticed on the two vector-store list routes.
    for url in [
        format!("{}/v1/vector_stores?purpose=vision", server.url),
        format!("{}/v1/vector_stores/vs_missing/files?purpose=vision", server.url),
    ] {
        let response = client.get(url).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }
    assert_eq!(
        client
            .get(format!("{}/v1/files?purpose=vision", server.url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn files_api_preserves_binary_uploads_without_requiring_searchable_content() {
    let server = gateway().await;
    let client = reqwest::Client::new();
    let bytes = [0, 255, 128, 10, 0, 42];
    let response = client
        .post(format!("{}/v1/files", server.url))
        .header("content-type", "multipart/form-data; boundary=upload")
        .body(multipart_with("payload.png", "image/png", "vision", &bytes))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "Files API upload does not perform search ingestion"
    );
    let file: Value = response.json().await.unwrap();
    let file_id = file["id"].as_str().unwrap();
    let downloaded = client
        .get(format!("{}/v1/files/{file_id}/content", server.url))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(downloaded.as_ref(), &bytes);
    client
        .delete(format!("{}/v1/files/{file_id}", server.url))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}

#[tokio::test]
async fn files_api_rows_follow_the_recorded_upload_lookup_and_delete_rules() {
    let server = gateway().await;
    let client = reqwest::Client::new();
    let upload = |purpose: &str, filename: &str, content_type: &str, body: &[u8]| {
        client
            .post(format!("{}/v1/files", server.url))
            .header("content-type", "multipart/form-data; boundary=upload")
            .body(multipart_with(filename, content_type, purpose, body))
            .send()
    };
    let error = |body: &Value| body["error"].clone();

    let response = upload("vision", "notes.txt", "text/plain", b"notes").await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.unwrap();
    assert_eq!(error(&body)["type"], "invalid_request_error");
    assert_eq!(error(&body)["code"], Value::Null);
    assert_eq!(error(&body)["param"], Value::Null);
    assert_eq!(
        error(&body)["message"],
        "Invalid file format text/plain. Supported formats: \"gif\", \"jpeg\", \"jpg\", \"png\", \"webp\""
    );

    let response = upload("user_data", "empty.txt", "text/plain", b"").await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.unwrap();
    assert_eq!(error(&body)["message"], "File is empty.");

    let response = upload("bogus", "notes.txt", "text/plain", b"notes").await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.unwrap();
    assert_eq!(error(&body)["param"], "purpose");
    assert_eq!(
        error(&body)["message"],
        "'bogus' is not one of ['fine-tune', 'assistants', 'batch', 'user_data', 'vision', 'evals'] - 'purpose'"
    );

    let response = client
        .post(format!("{}/v1/files", server.url))
        .header("content-type", "multipart/form-data; boundary=upload")
        .body(
            "--upload\r\nContent-Disposition: form-data; name=\"file\"; filename=\"notes.txt\"\r\n\
             Content-Type: text/plain\r\n\r\nnotes\r\n--upload--\r\n",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.unwrap();
    assert_eq!(error(&body)["param"], Value::Null);
    assert_eq!(error(&body)["message"], "'purpose' is a required property");

    let response = client
        .post(format!("{}/v1/files", server.url))
        .form(&[("purpose", "user_data")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.unwrap();
    assert_eq!(error(&body)["message"], "'file' is a required property");

    let file: Value = upload("vision", "colors.png", "image/png", b"png")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let file_id = file["id"].as_str().unwrap();
    let deleted = api_json(client.delete(format!("{}/v1/files/{file_id}", server.url))).await;
    assert_eq!(deleted["object"], "file");
    assert_eq!(deleted["deleted"], true);

    let missing = format!("file-{}", uuid::Uuid::nil());
    let response = client
        .get(format!("{}/v1/files?limit=1&after={missing}", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = response.json().await.unwrap();
    assert_eq!(error(&body)["param"], "after");
    assert_eq!(error(&body)["message"], format!("No such File object: {missing}"));
}
