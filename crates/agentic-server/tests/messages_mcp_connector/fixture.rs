use std::fmt::Write as _;
use std::process::Stdio;
use std::sync::Arc;

use axum::routing::post;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

const MCP_SERVER: &str = r"
import http.server, ssl, sys, json
observations = []
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def send_json(self, status, value):
        data = json.dumps(value).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_GET(self):
        if self.path == '/observations': self.send_json(200, observations)
        else: self.send_json(405, {})
    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        authorized = self.headers.get('Authorization') == 'Bearer connector-secret'
        observations.append({'method':request['method'], 'params':request.get('params'), 'authorized':authorized})
        if not authorized:
            self.send_json(401, {'error':'unauthorized'})
            return
        if 'id' not in request:
            self.send_json(202, {})
            return
        method = request['method']
        if method == 'initialize':
            result = {'protocolVersion':'2025-06-18', 'capabilities':{'tools':{}}, 'serverInfo':{'name':'fixture','version':'1'}}
        elif method == 'tools/list':
            result = {'tools':[{'name':name, 'description':name, 'inputSchema':{'type':'object','properties':{'text':{'type':'string'}}}} for name in ['echo','fail','disabled']]}
        elif method == 'tools/call':
            failed = request['params']['name'] == 'fail'
            result = {'content':[{'type':'text','text':'fixture error' if failed else 'fixture output: '+request['params']['arguments']['text']}], 'isError':failed}
        else:
            self.send_json(400, {})
            return
        self.send_json(200, {'jsonrpc':'2.0','id':request['id'],'result':result})
server = http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(sys.argv[1], sys.argv[2])
server.socket = context.wrap_socket(server.socket,server_side=True)
print(server.server_port,flush=True)
server.serve_forever()
";

pub struct HttpsMcp {
    pub url: String,
    pub certificate: Vec<u8>,
    child: Child,
    _files: tempfile::TempDir,
}

impl HttpsMcp {
    pub async fn start() -> Self {
        let files = tempfile::tempdir().unwrap();
        let cert = files.path().join("cert.pem");
        let key = files.path().join("key.pem");
        let der = files.path().join("cert.der");
        generate_certificate(files.path()).await;
        let mut child = Command::new("python3")
            .args(["-u", "-c", MCP_SERVER])
            .arg(cert)
            .arg(key)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
        let port = tokio::time::timeout(std::time::Duration::from_secs(10), output.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        Self {
            url: format!("https://127.0.0.1:{port}/mcp"),
            certificate: tokio::fs::read(der).await.unwrap(),
            child,
            _files: files,
        }
    }

    pub async fn observations(&self) -> Vec<Value> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .add_root_certificate(reqwest::Certificate::from_der(&self.certificate).unwrap())
            .build()
            .unwrap();
        client
            .get(self.url.replace("/mcp", "/observations"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    pub async fn stop(mut self) {
        self.child.kill().await.unwrap();
        let _ = self.child.wait().await;
    }
}

async fn openssl(directory: &std::path::Path, args: &[&str]) {
    let output = Command::new("openssl")
        .args(args)
        .current_dir(directory)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "certificate generation: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn generate_certificate(directory: &std::path::Path) {
    openssl(
        directory,
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "2",
            "-subj",
            "/CN=MCP fixture CA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
            "-keyout",
            "ca.key",
            "-out",
            "ca.pem",
        ],
    )
    .await;
    openssl(
        directory,
        &[
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=localhost",
            "-keyout",
            "key.pem",
            "-out",
            "cert.csr",
        ],
    )
    .await;
    tokio::fs::write(directory.join("extensions.cnf"), "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n").await.unwrap();
    openssl(
        directory,
        &[
            "x509",
            "-req",
            "-in",
            "cert.csr",
            "-CA",
            "ca.pem",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-days",
            "2",
            "-extfile",
            "extensions.cnf",
            "-out",
            "cert.pem",
        ],
    )
    .await;
    openssl(
        directory,
        &["x509", "-in", "ca.pem", "-outform", "DER", "-out", "cert.der"],
    )
    .await;
}

pub type Requests = Arc<Mutex<Vec<Value>>>;

pub async fn inference() -> (String, Requests, tokio::task::JoinHandle<()>) {
    let requests = Requests::default();
    let capture = Arc::clone(&requests);
    let count_capture = Arc::clone(&requests);
    let app = axum::Router::new()
        .route("/v1/messages", post(move |axum::Json(body):axum::Json<Value>| {
            let capture = Arc::clone(&capture);
            async move {
                capture.lock().await.push(body.clone());
                let content = content(&body);
                let stop = if body["model"].as_str().unwrap().starts_with("truncated") { "max_tokens" } else if content.iter().any(|b| b["type"] == "tool_use") { "tool_use" } else { "end_turn" };
                if body["stream"] == true {
                    axum::response::IntoResponse::into_response(([(http::header::CONTENT_TYPE,"text/event-stream")], frames(content,stop,body["model"].as_str().unwrap())))
                } else {
                    axum::response::IntoResponse::into_response(axum::Json(json!({"id":"msg", "type":"message", "role":"assistant", "model":"test", "content":content,"stop_reason":stop,"usage":{"input_tokens":2,"output_tokens":3}})))
                }
            }
        }))
        .route("/v1/messages/count_tokens", post(move |axum::Json(body):axum::Json<Value>| {
            let capture = Arc::clone(&count_capture);
            async move {
                capture.lock().await.push(body);
                axum::Json(json!({"input_tokens":42}))
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}"), requests, task)
}

fn content(body: &Value) -> Vec<Value> {
    if body["messages"].as_array().unwrap().iter().any(|message| {
        message["content"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_result"))
    }) {
        if body["model"] == "mixed" {
            let results = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|message| message["content"].as_array().into_iter().flatten())
                .filter(|block| block["type"] == "tool_result")
                .collect::<Vec<_>>();
            assert!(results.iter().any(|block| block["tool_use_id"] == "client"));
            assert!(
                results.iter().any(|block| block["tool_use_id"] == "call"),
                "MCP output must be present before inference"
            );
            for call in body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|message| message["content"].as_array().into_iter().flatten())
                .filter(|block| block["type"] == "tool_use")
            {
                assert!(
                    results.iter().any(|result| result["tool_use_id"] == call["id"]),
                    "every call needs its output before inference"
                );
            }
        }
        return vec![json!({"type":"text","text":"done"})];
    }
    let failed = body["model"] == "fail";
    let name = if failed {
        "mcp__counter__fail"
    } else {
        "mcp__counter__echo"
    };
    let mut blocks = Vec::new();
    if body["model"] == "deferred" {
        assert!(
            body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == name && tool["defer_loading"] == true)
        );
        assert!(
            body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "tool_search_tool_regex")
        );
        blocks.extend([
            json!({"type":"server_tool_use","id":"search","name":"tool_search_tool_regex","input":{"pattern":"echo"}}),
            json!({"type":"tool_search_tool_result","tool_use_id":"search","content":{"type":"tool_search_tool_search_result","tool_references":[{"type":"tool_reference","tool_name":name}]}})
        ]);
    }
    blocks.push(json!({"type":"tool_use","id":"call","name":name,"input":{"text":"hello"}}));
    if matches!(body["model"].as_str(), Some("truncated_mixed" | "mixed")) {
        blocks.push(json!({"type":"tool_use", "id":"client", "name":"client_echo", "input":{}}));
    }
    blocks
}

fn frames(content: Vec<Value>, stop: &str, mode: &str) -> String {
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"msg","type":"message","role":"assistant","content":[],"usage":{"input_tokens":2,"output_tokens":0}}}),
    ];
    for (index, block) in content.into_iter().enumerate() {
        let has_input = matches!(block["type"].as_str(), Some("tool_use" | "server_tool_use"));
        let mut start = block.clone();
        if has_input {
            start["input"] = json!({});
        }
        events.push(json!({"type":"content_block_start","index":index,"content_block":start}));
        if has_input {
            let input = block["input"].to_string();
            let (left, right) = input.split_at(input.len() / 2);
            for partial in [left, right] {
                events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":partial}}));
            }
        }
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":3}}));
    events.push(json!({"type":"message_stop"}));
    match mode {
        "missing_stop" => {
            events.pop();
        }
        "open_block" => events.retain(|event| event["type"] != "content_block_stop"),
        "missing_start" => {
            events.remove(0);
        }
        "duplicate_block" => {
            events.insert(2, events[1].clone());
        }
        "delta_after_stop" => {
            let position = events
                .iter()
                .position(|event| event["type"] == "content_block_stop")
                .unwrap();
            events.insert(
                position + 1,
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
            );
        }
        _ => {}
    }
    let mut frames = String::new();
    for event in events {
        write!(frames, "event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()).unwrap();
    }
    frames
}
