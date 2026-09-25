//! Responses transport selection. Opaque replay cannot inherit caller client state.

use std::{sync::Arc, time::Duration};

use super::{response_text_limited, send_request_with_policy};
use crate::executor::error::{ExecutorError, ExecutorResult};

#[derive(Debug, Clone, Copy)]
pub(super) enum ResponsePolicy {
    Compatible,
    Opaque,
}

/// Immutable client and error policy; no per-request credential is retained here.
#[derive(Debug, Clone)]
pub(crate) struct ResponsesTransport {
    client: Arc<reqwest::Client>,
    pub(super) response_policy: ResponsePolicy,
    #[cfg(test)]
    fixture_address: Option<std::net::SocketAddr>,
}

fn opaque_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(600))
        .pool_max_idle_per_host(1)
}

impl ResponsesTransport {
    pub(crate) fn shared(client: Arc<reqwest::Client>) -> Self {
        Self {
            client,
            response_policy: ResponsePolicy::Compatible,
            #[cfg(test)]
            fixture_address: None,
        }
    }

    /// The pinned profile uses direct HTTPS with only its per-request bearer
    /// credential. No redirects, environment proxies, retries, cookies, or caller
    /// default headers can silently change the qualified routing identity.
    pub(crate) fn opaque() -> ExecutorResult<Self> {
        let client = opaque_client_builder()
            .build()
            .map_err(|_| ExecutorError::LLMTransport {
                status: http::StatusCode::INTERNAL_SERVER_ERROR,
                message: "failed to initialize opaque Responses transport",
            })?;
        Ok(Self {
            client: Arc::new(client),
            response_policy: ResponsePolicy::Opaque,
            #[cfg(test)]
            fixture_address: None,
        })
    }

    pub(super) async fn send(
        &self,
        url: &str,
        body: String,
        auth: Option<&str>,
        chunk_timeout: Duration,
    ) -> ExecutorResult<reqwest::Response> {
        #[cfg(test)]
        let fixture_url = self.fixture_address.map(|address| {
            assert_eq!(
                url,
                crate::types::reasoning_profile::OpaqueReasoningProfile::OpenAiGpt54_20260305V1.endpoint()
            );
            format!("http://{address}/v1/responses")
        });
        #[cfg(test)]
        let url = fixture_url.as_deref().unwrap_or(url);
        send_request_with_policy(&self.client, url, body, auth, None, chunk_timeout, self.response_policy).await
    }

    pub(crate) async fn fetch_json(
        &self,
        url: &str,
        body: String,
        auth: Option<&str>,
        max_bytes: usize,
    ) -> ExecutorResult<String> {
        let response = self.send(url, body, auth, Duration::ZERO).await?;
        // The opaque client's nonzero read timeout also bounds headers/body reads
        // when the caller disables the separate streaming chunk timeout.
        response_text_limited(response, Duration::ZERO, max_bytes).await
    }
}

#[cfg(test)]
impl ResponsesTransport {
    pub(in crate::executor) fn is_replay_fixture(&self) -> bool {
        self.fixture_address.is_some()
    }

    /// No arbitrary endpoint, credential, DNS override or runtime enablement flag.
    pub(in crate::executor) fn replay_fixture(address: std::net::SocketAddr) -> Self {
        assert!(address.ip().is_loopback());
        Self {
            client: Arc::new(opaque_client_builder().https_only(false).build().unwrap()),
            response_policy: ResponsePolicy::Opaque,
            fixture_address: Some(address),
        }
    }
}

#[cfg(test)]
mod tests;
