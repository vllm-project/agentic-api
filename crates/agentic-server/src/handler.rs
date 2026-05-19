use agentic_core::proxy::{ProxyBody, ProxyRequest, ProxyResponse, ProxyState, error_response};
use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use http::StatusCode;

const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

fn convert_response(resp: ProxyResponse) -> Response {
    let mut builder = Response::builder().status(resp.status);
    for (name, value) in &resp.headers {
        builder = builder.header(name, value);
    }
    match resp.body {
        ProxyBody::Full(bytes) => builder.body(Body::from(bytes)).expect("valid response"),
        ProxyBody::Stream(stream) => builder.body(Body::from_stream(stream)).expect("valid response"),
    }
}

pub async fn proxy_responses(State(state): State<ProxyState>, req: axum::extract::Request) -> Response {
    let (parts, body) = req.into_parts();

    let Ok(body_bytes) = axum::body::to_bytes(body, MAX_BODY_SIZE).await else {
        return convert_response(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "Request body too large",
        ));
    };

    let proxy_req = ProxyRequest {
        headers: parts.headers,
        body: body_bytes,
        query: parts.uri.query().map(String::from),
    };

    convert_response(agentic_core::proxy::proxy_request(proxy_req, &state).await)
}
