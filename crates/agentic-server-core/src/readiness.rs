use std::time::Duration;

use tracing::info;

use crate::config::Config;
use crate::error::Error;

/// Maximum duration of one inference-service readiness probe.
pub const LLM_READINESS_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

fn checked_duration_seconds(name: &str, value: f64) -> Result<Duration, Error> {
    if !value.is_finite() || value <= 0.0 {
        return Err(Error::Config(format!(
            "{name} must be a finite number > 0 (got {value})"
        )));
    }
    Duration::try_from_secs_f64(value)
        .map_err(|_| Error::Config(format!("{name} must be representable as a Duration (got {value})")))
}

fn timeout_error(url: &str, timeout_s: f64) -> Error {
    Error::LlmTimeout {
        url: url.to_owned(),
        timeout_s,
    }
}

/// Result of a single bounded inference-service health probe.
#[derive(Debug)]
#[non_exhaustive]
pub enum LlmReadiness {
    Ready,
    Rejected(reqwest::StatusCode),
    Unreachable(reqwest::Error),
    TimedOut,
}

/// Build the dedicated HTTP client used for inference-service health probes.
///
/// The client rejects redirects so an authentication page or generic UI cannot
/// turn an unsuccessful `/health` response into a false-positive readiness result.
///
/// Connections are never pooled: the probe runs every few seconds, which is close
/// to the idle keep-alive timeout of common upstream servers (uvicorn closes idle
/// connections after 5 s). Reusing a pooled connection the upstream has already
/// closed fails with `hyper::Error(IncompleteMessage)` and flaps `/ready` even
/// though the upstream is healthy.
///
/// # Errors
///
/// Returns an error when the HTTP client cannot be constructed.
pub fn llm_readiness_client() -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(0)
        .build()
        .map_err(Error::HttpClient)
}

/// Probe the inference service's `/health` endpoint once.
///
/// # Errors
///
/// Returns an error when the configured bearer credential cannot be represented
/// as an HTTP header.
pub async fn probe_llm_readiness(
    client: &reqwest::Client,
    llm_api_base: &str,
    openai_api_key: Option<&str>,
    timeout: Duration,
) -> Result<LlmReadiness, Error> {
    let base = llm_api_base.trim_end_matches('/');
    let url = format!("{base}/health");
    let mut request = client.get(url);
    if let Some(key) = openai_api_key.map(str::trim).filter(|key| !key.is_empty()) {
        let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))?;
        request = request.header(reqwest::header::AUTHORIZATION, value);
    }

    Ok(match tokio::time::timeout(timeout, request.send()).await {
        Ok(Ok(response)) if response.status().is_success() => LlmReadiness::Ready,
        Ok(Ok(response)) => LlmReadiness::Rejected(response.status()),
        Ok(Err(error)) => LlmReadiness::Unreachable(error),
        Err(_) => LlmReadiness::TimedOut,
    })
}

/// Poll LLM `/health` until it responds successfully or the timeout is reached.
///
/// # Errors
///
/// Returns an error if the LLM does not become ready within the configured timeout.
pub async fn wait_llm_ready(config: &Config) -> Result<(), Error> {
    let base = config.llm_api_base.trim_end_matches('/');
    let url = format!("{base}/health");

    let client = llm_readiness_client()?;

    let timeout = checked_duration_seconds("llm_ready_timeout_s", config.llm_ready_timeout_s)?;
    let interval = checked_duration_seconds("llm_ready_interval_s", config.llm_ready_interval_s)?;
    let start = tokio::time::Instant::now();
    let mut last_notice = Duration::ZERO;

    loop {
        let remaining = timeout
            .checked_sub(start.elapsed())
            .ok_or_else(|| timeout_error(&url, config.llm_ready_timeout_s))?;
        if remaining.is_zero() {
            return Err(timeout_error(&url, config.llm_ready_timeout_s));
        }

        if matches!(
            probe_llm_readiness(
                &client,
                &config.llm_api_base,
                config.openai_api_key.as_deref(),
                LLM_READINESS_PROBE_TIMEOUT.min(remaining),
            )
            .await?,
            LlmReadiness::Ready
        ) {
            return Ok(());
        }

        let elapsed = start.elapsed();
        if elapsed.saturating_sub(last_notice) >= interval {
            last_notice = elapsed;
            info!("waiting for LLM ({}s elapsed): {url}", elapsed.as_secs());
        }

        let remaining = timeout
            .checked_sub(start.elapsed())
            .ok_or_else(|| timeout_error(&url, config.llm_ready_timeout_s))?;
        if remaining.is_zero() {
            return Err(timeout_error(&url, config.llm_ready_timeout_s));
        }

        tokio::time::sleep(interval.min(remaining)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::Router;
    use axum::http::HeaderMap;
    use axum::response::{IntoResponse, Redirect};
    use axum::routing::get;
    use http::StatusCode;
    use tokio::net::TcpListener;

    use super::{checked_duration_seconds, probe_llm_readiness, timeout_error, wait_llm_ready};

    fn test_config(llm_api_base: String) -> crate::config::Config {
        crate::config::Config {
            llm_api_base,
            openai_api_key: Some("test-key".to_owned()),
            llm_ready_timeout_s: 0.5,
            llm_ready_interval_s: 0.01,
            skip_llm_ready_check: false,
            db_url: None,
            postgres: crate::config::PostgresConfig::default(),
            sqlite: crate::config::SqliteConfig::default(),
            tools: crate::config::ToolRuntimeConfig::default(),
            responses: crate::config::ResponsesConfig::default(),
        }
    }

    async fn spawn_upstream(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn checked_duration_rejects_non_positive() {
        assert!(checked_duration_seconds("v", 0.0).is_err());
        assert!(checked_duration_seconds("v", -1.0).is_err());
    }

    #[test]
    fn checked_duration_rejects_nan() {
        assert!(checked_duration_seconds("v", f64::NAN).is_err());
    }

    #[test]
    fn checked_duration_rejects_infinite() {
        assert!(checked_duration_seconds("v", f64::INFINITY).is_err());
    }

    #[test]
    fn checked_duration_rejects_too_large_finite() {
        assert!(checked_duration_seconds("v", 1e50).is_err());
    }

    #[test]
    fn checked_duration_accepts_positive_finite() {
        let duration = checked_duration_seconds("v", 0.25).unwrap();
        assert_eq!(duration.as_millis(), 250);
    }

    #[test]
    fn timeout_error_preserves_inputs() {
        let err = timeout_error("http://127.0.0.1:8000/health", 0.5);
        match err {
            crate::error::Error::LlmTimeout { url, timeout_s } => {
                assert_eq!(url, "http://127.0.0.1:8000/health");
                assert!((timeout_s - 0.5).abs() < f64::EPSILON);
            }
            other => panic!("expected timeout error, got {other:?}"),
        }
    }

    #[test]
    fn interval_sleep_is_capped_by_remaining_timeout() {
        let interval = Duration::from_secs(2);
        let remaining = Duration::from_millis(100);
        assert_eq!(interval.min(remaining), Duration::from_millis(100));
    }

    /// Regression test for readiness flapping: the probe must not reuse a pooled
    /// keep-alive connection between runs, because upstreams such as uvicorn close
    /// idle connections on roughly the same cadence as the probe and a reused dead
    /// socket fails with `IncompleteMessage` while the upstream is healthy.
    #[tokio::test]
    async fn readiness_client_opens_a_new_connection_per_probe() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let upstream = tokio::spawn({
            let connections = Arc::clone(&connections);
            async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    connections.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(async move {
                        // Serve keep-alive responses for as many requests as arrive on this socket.
                        let mut buffer = [0_u8; 2048];
                        while let Ok(read) = socket.read(&mut buffer).await {
                            if read == 0 {
                                break;
                            }
                            let response = "HTTP/1.1 204 No Content\r\nconnection: keep-alive\r\n\r\n";
                            if socket.write_all(response.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                    });
                }
            }
        });

        let client = super::llm_readiness_client().unwrap();
        for _ in 0..3 {
            let readiness = probe_llm_readiness(&client, &url, None, Duration::from_secs(1))
                .await
                .unwrap();
            assert!(matches!(readiness, super::LlmReadiness::Ready), "{readiness:?}");
        }
        upstream.abort();

        assert_eq!(
            connections.load(Ordering::SeqCst),
            3,
            "each probe must open its own TCP connection instead of reusing a pooled one"
        );
    }

    #[tokio::test]
    async fn probe_rejects_invalid_bearer_header_before_network_io() {
        let error = probe_llm_readiness(
            &reqwest::Client::new(),
            "http://127.0.0.1:1",
            Some("invalid\nkey"),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, crate::error::Error::InvalidHeader(_)));
    }

    #[tokio::test]
    async fn wait_llm_ready_retries_with_authentication_until_success() {
        let requests = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/health",
            get({
                let requests = Arc::clone(&requests);
                move |headers: HeaderMap| {
                    let requests = Arc::clone(&requests);
                    async move {
                        if headers.get("authorization").and_then(|value| value.to_str().ok()) != Some("Bearer test-key")
                        {
                            return StatusCode::UNAUTHORIZED;
                        }
                        if requests.fetch_add(1, Ordering::SeqCst) == 0 {
                            StatusCode::SERVICE_UNAVAILABLE
                        } else {
                            StatusCode::NO_CONTENT
                        }
                    }
                }
            }),
        );
        let (url, upstream) = spawn_upstream(app).await;

        wait_llm_ready(&test_config(url)).await.unwrap();

        assert!(requests.load(Ordering::SeqCst) >= 2);
        upstream.abort();
    }

    #[tokio::test]
    async fn wait_llm_ready_rejects_redirects_until_final_timeout() {
        let app = Router::new()
            .route("/health", get(|| async { Redirect::temporary("/login") }))
            .route("/login", get(|| async { StatusCode::OK.into_response() }));
        let (url, upstream) = spawn_upstream(app).await;
        let mut config = test_config(url.clone());
        config.llm_ready_timeout_s = 0.05;

        let error = wait_llm_ready(&config).await.unwrap_err();

        match error {
            crate::error::Error::LlmTimeout {
                url: timed_out_url,
                timeout_s,
            } => {
                assert_eq!(timed_out_url, format!("{url}/health"));
                assert!((timeout_s - 0.05).abs() < f64::EPSILON);
            }
            other => panic!("expected timeout error, got {other:?}"),
        }
        upstream.abort();
    }
}
