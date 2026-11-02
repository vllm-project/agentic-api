//! W3C trace context and baggage carried with MCP `tools/call` requests.
//!
//! MCP requests are sent by a transport task spawned when the pooled connection
//! was opened, so the HTTP layer cannot read the calling span from `Span::current()`.
//! The context is captured when the call is made and travels in the message's
//! `params._meta`: stdio servers read it there, and the HTTP transport restores it
//! and sends it as request headers.

use std::collections::HashMap;

use http::header::{HeaderName, HeaderValue};
use opentelemetry::propagation::Injector;
use rmcp::model::{ClientJsonRpcMessage, ClientRequest, JsonRpcMessage, Meta};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::utils::common::{deserialize_from_value_opt, deserialize_from_value_or_custom_default, serialize_to_value};

const TRACEPARENT: HeaderName = HeaderName::from_static("traceparent");
const TRACESTATE: HeaderName = HeaderName::from_static("tracestate");
const BAGGAGE: HeaderName = HeaderName::from_static("baggage");

/// The propagation fields an MCP call carries. Fields other propagators set are
/// dropped, so `_meta` never holds anything the HTTP transport would not forward.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct W3cPropagation {
    #[serde(
        default,
        deserialize_with = "lenient_string",
        skip_serializing_if = "Option::is_none"
    )]
    traceparent: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient_string",
        skip_serializing_if = "Option::is_none"
    )]
    tracestate: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient_string",
        skip_serializing_if = "Option::is_none"
    )]
    baggage: Option<String>,
}

/// A field that is not a string is dropped on its own, so one malformed value does
/// not discard the rest of the caller's context.
fn lenient_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<Value>::deserialize(deserializer)?.and_then(deserialize_from_value_opt))
}

impl Injector for W3cPropagation {
    fn set(&mut self, key: &str, value: String) {
        // The trace-context propagator writes an empty `tracestate` when there is none.
        if value.is_empty() {
            return;
        }
        match key {
            "traceparent" => self.traceparent = Some(value),
            "tracestate" => self.tracestate = Some(value),
            "baggage" => self.baggage = Some(value),
            _ => {}
        }
    }
}

impl W3cPropagation {
    /// The calling span's context, as the global propagator encodes it.
    pub(crate) fn capture() -> Self {
        let context = Span::current().context();
        let mut propagation = Self::default();
        opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&context, &mut propagation);
        });
        propagation
    }

    /// `_meta` for a `tools/call`, or `None` when there is nothing to propagate.
    pub(crate) fn into_meta(self) -> Option<Meta> {
        match serialize_to_value(&self) {
            Ok(Value::Object(map)) if !map.is_empty() => Some(Meta(map)),
            _ => None,
        }
    }

    /// The context a `tools/call` carries in `_meta`; empty for any other message.
    pub(crate) fn from_message(message: &ClientJsonRpcMessage) -> Self {
        let JsonRpcMessage::Request(request) = message else {
            return Self::default();
        };
        let ClientRequest::CallToolRequest(call) = &request.request else {
            return Self::default();
        };
        let Some(meta) = call.params.meta.as_ref() else {
            return Self::default();
        };
        // The transport still sends `message`, so the deserializer gets a copy of `_meta`.
        deserialize_from_value_or_custom_default(
            Value::Object(meta.0.clone()),
            "ignoring malformed MCP propagation metadata",
            |propagation| propagation,
            Self::default(),
        )
    }

    /// Replace any configured propagation headers with this context.
    ///
    /// Any field the caller's context carries removes the configured value first, and
    /// a new `traceparent` also drops a configured `tracestate`, which belongs to the
    /// trace being replaced. Values that are not valid header values (baggage
    /// properties may decode to control characters) are then left out rather than
    /// falling back to the configured value, which would misattribute the call, so
    /// telemetry never prevents the tool call either.
    pub(crate) fn apply_to(self, headers: &mut HashMap<HeaderName, HeaderValue>) {
        let mut replace = |name: HeaderName, value: Option<String>| {
            let Some(value) = value else { return };
            headers.remove(&name);
            if name == TRACEPARENT {
                headers.remove(&TRACESTATE);
            }
            if let Ok(value) = HeaderValue::try_from(value) {
                headers.insert(name, value);
            }
        };
        replace(TRACEPARENT, self.traceparent);
        replace(TRACESTATE, self.tracestate);
        replace(BAGGAGE, self.baggage);
    }
}

#[cfg(test)]
mod tests {
    use rmcp::model::{CallToolRequest, CallToolRequestParams, NumberOrString};

    use super::*;

    fn call_with(propagation: W3cPropagation) -> ClientJsonRpcMessage {
        let mut params = CallToolRequestParams::new("echo");
        params.meta = propagation.into_meta();
        ClientJsonRpcMessage::request(
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            NumberOrString::Number(1),
        )
    }

    #[test]
    fn meta_round_trips_through_a_tools_call() {
        let propagation = W3cPropagation {
            traceparent: Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".to_owned()),
            tracestate: None,
            baggage: Some("tenant=acme".to_owned()),
        };
        let restored = W3cPropagation::from_message(&call_with(propagation.clone()));
        assert_eq!(restored, propagation);
        assert!(W3cPropagation::default().into_meta().is_none());
    }

    fn call_with_meta(meta: Value) -> ClientJsonRpcMessage {
        let mut params = CallToolRequestParams::new("echo");
        let Value::Object(map) = meta else {
            panic!("meta is an object")
        };
        params.meta = Some(Meta(map));
        ClientJsonRpcMessage::request(
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            NumberOrString::Number(1),
        )
    }

    #[test]
    fn missing_null_and_malformed_fields_are_dropped_one_by_one() {
        let restored = W3cPropagation::from_message(&call_with_meta(serde_json::json!({
            "traceparent": "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            "tracestate": null,
            "baggage": 7,
            "progressToken": "unrelated",
        })));
        assert_eq!(
            restored,
            W3cPropagation {
                traceparent: Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".to_owned()),
                tracestate: None,
                baggage: None,
            }
        );
        assert_eq!(
            W3cPropagation::from_message(&call_with_meta(serde_json::json!({}))),
            W3cPropagation::default()
        );
    }

    #[test]
    fn replaces_configured_context_and_its_stale_tracestate() {
        let mut headers = HashMap::from([
            (TRACEPARENT, HeaderValue::from_static("00-stale-stale-01")),
            (TRACESTATE, HeaderValue::from_static("vendor=stale")),
            (BAGGAGE, HeaderValue::from_static("tenant=stale")),
            (HeaderName::from_static("x-api-key"), HeaderValue::from_static("kept")),
        ]);
        W3cPropagation {
            traceparent: Some("00-current-current-01".to_owned()),
            tracestate: None,
            baggage: Some("tenant=acme".to_owned()),
        }
        .apply_to(&mut headers);
        assert_eq!(headers[&TRACEPARENT], "00-current-current-01");
        assert!(!headers.contains_key(&TRACESTATE));
        assert_eq!(headers[&BAGGAGE], "tenant=acme");
        assert_eq!(headers[&HeaderName::from_static("x-api-key")], "kept");
    }

    #[test]
    fn invalid_values_are_left_out_instead_of_keeping_configured_ones() {
        let mut headers = HashMap::from([(BAGGAGE, HeaderValue::from_static("tenant=stale"))]);
        W3cPropagation {
            traceparent: Some("00-current-current-01".to_owned()),
            tracestate: None,
            baggage: Some("tenant=acme;note=\nfoo".to_owned()),
        }
        .apply_to(&mut headers);
        assert_eq!(headers[&TRACEPARENT], "00-current-current-01");
        assert!(!headers.contains_key(&BAGGAGE));
    }
}
