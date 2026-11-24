//! The model-facing output of one `web_search` call.
//!
//! The executor serializes it once per call. The Messages adapter reads it back
//! and writes a native call's model-facing text with the same type, so the two
//! cannot drift; a native call shows the model its results without the
//! provider's `metadata`.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use super::provider::{WebSearchProviderMetadata, WebSearchResult};

/// Web and news results, each in provider order.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct WebSearchResultSections {
    pub(crate) web: Vec<WebSearchResult>,
    pub(crate) news: Vec<WebSearchResult>,
}

/// Model-facing `web_search` tool output; field order is the wire contract.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WebSearchToolOutput<'a> {
    pub(crate) query: Cow<'a, str>,
    pub(crate) queries: Cow<'a, [String]>,
    pub(crate) results: WebSearchResultSections,
    /// Per-query provider metadata; never read back.
    #[serde(skip_serializing_if = "Option::is_none", skip_deserializing)]
    pub(crate) metadata: Option<Vec<WebSearchProviderMetadata>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WebSearchProviderKind;

    fn sections() -> WebSearchResultSections {
        WebSearchResultSections {
            web: vec![WebSearchResult {
                url: "https://example.com/potato".to_owned(),
                title: Some("Potato".to_owned()),
                description: Some("A tuber".to_owned()),
                snippets: vec!["Starchy.".to_owned()],
                page_age: None,
                contents: None,
            }],
            news: vec![WebSearchResult::default()],
        }
    }

    #[test]
    fn web_search_output_serializes_in_wire_order() {
        let queries = ["potato".to_owned()];
        let output = WebSearchToolOutput {
            query: Cow::Borrowed("potato"),
            queries: Cow::Borrowed(&queries),
            results: sections(),
            metadata: Some(vec![WebSearchProviderMetadata {
                provider: WebSearchProviderKind::You,
                query: "potato".to_owned(),
                search_uuid: Some("s1".to_owned()),
                latency: Some(0.12),
            }]),
        };
        let text = serde_json::to_string(&output).unwrap();
        assert_eq!(
            text,
            concat!(
                r#"{"query":"potato","queries":["potato"],"#,
                r#""results":{"web":[{"url":"https://example.com/potato","title":"Potato","#,
                r#""description":"A tuber","snippets":["Starchy."]}],"news":[{}]},"#,
                r#""metadata":[{"query":"potato","search_uuid":"s1","latency":0.12}]}"#
            )
        );

        // Read back without its metadata, it serializes as a native call shows it.
        let read: WebSearchToolOutput<'_> = serde_json::from_str(&text).unwrap();
        assert!(read.metadata.is_none());
        assert_eq!(read.queries.as_ref(), ["potato".to_owned()]);
        assert_eq!(
            serde_json::to_string(&read).unwrap(),
            concat!(
                r#"{"query":"potato","queries":["potato"],"#,
                r#""results":{"web":[{"url":"https://example.com/potato","title":"Potato","#,
                r#""description":"A tuber","snippets":["Starchy."]}],"news":[{}]}}"#
            )
        );
    }
}
