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
    files: tempfile::TempDir,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn gateway() -> Gateway {
    let files = tempfile::tempdir().unwrap();
    let mut config = test_config("http://127.0.0.1:1");
    config.db_url = Some("sqlite::memory:".into());
    config.tools.file_search.files_storage_dir = Some(files.path().to_owned());
    let mut state = test_state(&config);
    state.exec_ctx = Arc::new(ExecutionContext::from_config(&config).await.unwrap());
    let (url, task) = spawn_gateway(state).await;
    Gateway { url, task, files }
}

fn multipart(filename: &str, text: &str) -> String {
    format!(
        "--upload\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nassistants\r\n--upload\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: text/plain\r\n\r\n{text}\r\n--upload--\r\n"
    )
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
    let bytes = client
        .get(format!("{file_url}/content"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), b"A retained document.");
    assert_eq!(tokio::fs::read(server.files.path().join(file_id)).await.unwrap(), bytes);
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

#[tokio::test]
async fn files_api_preserves_binary_uploads_without_requiring_searchable_content() {
    let server = gateway().await;
    let client = reqwest::Client::new();
    let bytes = [0, 255, 128, 10, 0, 42];
    let mut body = b"--upload\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nuser_data\r\n--upload\r\nContent-Disposition: form-data; name=\"file\"; filename=\"payload.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n".to_vec();
    body.extend_from_slice(&bytes);
    body.extend_from_slice(b"\r\n--upload--\r\n");
    let response = client
        .post(format!("{}/v1/files", server.url))
        .header("content-type", "multipart/form-data; boundary=upload")
        .body(body)
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
async fn files_contract_purposes_expiration_pagination_and_delete() {
    let server = gateway().await;
    let client = reqwest::Client::new();
    for purpose in ["assistants", "batch", "fine-tune", "vision", "user_data", "evals"] {
        let body = multipart("contract.bin", "bytes").replace("\r\nassistants\r\n", &format!("\r\n{purpose}\r\n"));
        let file = api_json(
            client
                .post(format!("{}/v1/files", server.url))
                .header("content-type", "multipart/form-data; boundary=upload")
                .body(body),
        )
        .await;
        if purpose == "batch" {
            assert_eq!(
                file["expires_at"].as_i64().unwrap() - file["created_at"].as_i64().unwrap(),
                2_592_000
            );
        }
        let list = api_json(client.get(format!("{}/v1/files?purpose={purpose}&limit=10000", server.url))).await;
        assert_eq!(list["data"].as_array().unwrap().len(), 1);
        assert_eq!(list["data"][0]["id"], file["id"]);
        let deleted =
            api_json(client.delete(format!("{}/v1/files/{}", server.url, file["id"].as_str().unwrap()))).await;
        assert_eq!(deleted["object"], "file");
    }
    let body = multipart("expiry.bin", "bytes").replace("--upload--\r\n", "--upload\r\nContent-Disposition: form-data; name=\"expires_after[anchor]\"\r\n\r\ncreated_at\r\n--upload\r\nContent-Disposition: form-data; name=\"expires_after[seconds]\"\r\n\r\n3600\r\n--upload--\r\n");
    let file = api_json(
        client
            .post(format!("{}/v1/files", server.url))
            .header("content-type", "multipart/form-data; boundary=upload")
            .body(body),
    )
    .await;
    assert_eq!(
        file["expires_at"].as_i64().unwrap() - file["created_at"].as_i64().unwrap(),
        3600
    );
}

#[tokio::test]
async fn files_stream_above_old_limit_and_accept_empty_content() {
    let server = gateway().await;
    let client = reqwest::Client::new();
    for len in [0, 21 * 1024 * 1024] {
        let bytes = "z".repeat(len);
        let file = api_json(
            client
                .post(format!("{}/v1/files", server.url))
                .header("content-type", "multipart/form-data; boundary=upload")
                .body(multipart("large.bin", &bytes)),
        )
        .await;
        assert_eq!(file["bytes"], len);
        let response = client
            .get(format!(
                "{}/v1/files/{}/content",
                server.url,
                file["id"].as_str().unwrap()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.content_length(), Some(len as u64));
        assert_eq!(response.bytes().await.unwrap().as_ref(), bytes.as_bytes());
    }
}

#[tokio::test]
async fn file_first_upload_validates_trailing_fields_before_publication() {
    let server = gateway().await;
    let client = reqwest::Client::new();
    let prefix = format!(
        "--upload\r\nContent-Disposition: form-data; name=\"file\"; filename=\"first.bin\"\r\n\r\n{}\r\n",
        "a".repeat(1024 * 1024)
    );
    for tail in [
        "--upload\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nassistants\r\n--upload\r\nContent-Disposition: form-data; name=\"unknown\"\r\n\r\nbad\r\n--upload--\r\n".to_owned(),
        "--upload\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nassistants\r\n--upload\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nassistants\r\n--upload--\r\n".to_owned(),
        format!("--upload\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\n{}\r\n--upload--\r\n", "x".repeat(1024 * 1024)),
        "--upload\r\nContent-Disposition: form-data; name=\"expires_after[seconds]\"\r\n\r\n3600\r\n--upload--\r\n".to_owned(),
    ] {
        let response = client.post(format!("{}/v1/files", server.url)).header("content-type", "multipart/form-data; boundary=upload").body(format!("{prefix}{tail}")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if std::fs::read_dir(server.files.path()).unwrap().count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        api_json(client.get(format!("{}/v1/files", server.url))).await["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let tail = "--upload\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nvision\r\n--upload--\r\n";
    let file = api_json(
        client
            .post(format!("{}/v1/files", server.url))
            .header("content-type", "multipart/form-data; boundary=upload")
            .body(format!("{prefix}{tail}")),
    )
    .await;
    assert_eq!(file["bytes"], 1024 * 1024);
}

#[tokio::test]
async fn disconnected_multipart_upload_cleans_staging() {
    use tokio::io::AsyncWriteExt as _;
    let server = gateway().await;
    let address = server.url.strip_prefix("http://").unwrap();
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    let headers = format!(
        "POST /v1/files HTTP/1.1\r\nHost: {address}\r\nContent-Type: multipart/form-data; boundary=upload\r\nContent-Length: 2097152\r\n\r\n--upload\r\nContent-Disposition: form-data; name=\"file\"; filename=\"disconnect.bin\"\r\n\r\n"
    );
    socket.write_all(headers.as_bytes()).await.unwrap();
    socket.write_all(&vec![7; 1024 * 1024]).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if std::fs::read_dir(server.files.path()).unwrap().count() > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(socket);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if std::fs::read_dir(server.files.path()).unwrap().count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let response = api_json(reqwest::Client::new().get(format!("{}/v1/files", server.url))).await;
    assert_eq!(response["data"], json!([]));
}

#[tokio::test]
async fn files_default_page_and_expiration_bounds_match_contract() {
    let server = gateway().await;
    let client = reqwest::Client::new();
    for index in 0..21 {
        api_json(
            client
                .post(format!("{}/v1/files", server.url))
                .header("content-type", "multipart/form-data; boundary=upload")
                .body(multipart(&format!("{index}.txt"), "x")),
        )
        .await;
    }
    let page = api_json(client.get(format!("{}/v1/files", server.url))).await;
    assert_eq!(page["data"].as_array().unwrap().len(), 21);
    assert_eq!(page["has_more"], false);
    for seconds in ["3599", "2592001", "not-a-number"] {
        let extra = format!(
            "--upload\r\nContent-Disposition: form-data; name=\"expires_after[anchor]\"\r\n\r\ncreated_at\r\n--upload\r\nContent-Disposition: form-data; name=\"expires_after[seconds]\"\r\n\r\n{seconds}\r\n--upload--\r\n"
        );
        let body = multipart("invalid.txt", "x").replace("--upload--\r\n", &extra);
        let response = client
            .post(format!("{}/v1/files", server.url))
            .header("content-type", "multipart/form-data; boundary=upload")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    for limit in [0, 10001] {
        let response = client
            .get(format!("{}/v1/files?limit={limit}", server.url))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

async fn oversized_framing_is_rejected_before_body_finishes(prefix: &[u8]) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let server = gateway().await;
    let address = server.url.strip_prefix("http://").unwrap();
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    let headers = format!(
        "POST /v1/files HTTP/1.1\r\nHost: {address}\r\nContent-Type: multipart/form-data; boundary=upload\r\nContent-Length: 536870912\r\n\r\n"
    );
    socket.write_all(headers.as_bytes()).await.unwrap();
    socket.write_all(prefix).await.unwrap();
    // Deliberately leave the declared 512 MiB body unfinished. Framing must
    // fail after this small prefix rather than await/buffer the remaining body.
    socket.write_all(&vec![b'x'; 16 * 1024]).await.unwrap();
    let mut response = [0; 1024];
    let length = tokio::time::timeout(std::time::Duration::from_secs(2), socket.read(&mut response))
        .await
        .expect("multipart framing must be rejected before the body finishes")
        .unwrap();
    assert!(
        std::str::from_utf8(&response[..length])
            .unwrap()
            .starts_with("HTTP/1.1 413")
    );
    assert_eq!(std::fs::read_dir(server.files.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn oversized_multipart_preamble_is_rejected_early() {
    oversized_framing_is_rejected_before_body_finishes(b"unbounded preamble ").await;
}

#[tokio::test]
async fn oversized_multipart_part_headers_are_rejected_early() {
    oversized_framing_is_rejected_before_body_finishes(
        b"--upload\r\nContent-Disposition: form-data; name=\"file\"; filename=\"",
    )
    .await;
}
