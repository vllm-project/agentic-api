use std::time::Duration;

use tracing::info;

use crate::config::Config;
use crate::error::Error;

fn checked_duration_seconds(name: &str, value: f64) -> Result<Duration, Error> {
    if !value.is_finite() || value <= 0.0 {
        return Err(Error::Config(format!(
            "{name} must be a finite number > 0 (got {value})"
        )));
    }
    Duration::try_from_secs_f64(value)
        .map_err(|_| Error::Config(format!("{name} must be representable as a Duration (got {value})")))
}

/// Poll LLM `/health` until it responds 200 or the timeout is reached.
///
/// # Errors
///
/// Returns an error if the LLM does not become ready within the configured timeout.
pub async fn wait_llm_ready(config: &Config) -> Result<(), Error> {
    let base = config.llm_api_base.trim_end_matches('/');
    let url = format!("{base}/health");

    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(key) = config.openai_api_key.as_deref() {
        let trimmed = key.trim();
        if !trimmed.is_empty() {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {trimmed}"))?,
            );
        }
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .default_headers(headers)
        .build()
        .map_err(Error::HttpClient)?;

    let timeout = checked_duration_seconds("llm_ready_timeout_s", config.llm_ready_timeout_s)?;
    let interval = checked_duration_seconds("llm_ready_interval_s", config.llm_ready_interval_s)?;
    let start = tokio::time::Instant::now();
    let mut last_notice = Duration::ZERO;

    loop {
        let elapsed = start.elapsed();
        if elapsed > timeout {
            return Err(Error::LlmTimeout {
                url,
                timeout_s: config.llm_ready_timeout_s,
            });
        }

        match client.get(&url).send().await {
            Ok(resp) if resp.status().as_u16() == 200 => return Ok(()),
            _ => {}
        }

        if elapsed.saturating_sub(last_notice) >= interval {
            last_notice = elapsed;
            info!("waiting for LLM ({}s elapsed): {url}", elapsed.as_secs());
        }

        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::checked_duration_seconds;

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
}
