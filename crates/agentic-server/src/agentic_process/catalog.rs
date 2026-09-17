//! Resolve harness models and Codex input capabilities from upstream catalogs.

use std::time::Duration;

use agentic_core::error::Error;
use reqwest::Client;
use serde::Deserialize;
use tokio::time::{Instant, sleep};

use crate::agentic_cli::{Harness, SourceOptions};
use crate::agentic_output::redact_url;
use crate::model_capabilities::{CodexCatalogCapabilities, InputModalities};

use super::{PLACEHOLDER_MODEL, harness_binary};

/// Operator-provided Codex version, used instead of probing the Codex binary.
const CODEX_CLIENT_VERSION_ENV: &str = "AGENTIC_CODEX_CLIENT_VERSION";
/// Bound on the `codex --version` probe.
const CODEX_VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound on the catalog payload the launcher reads from a gateway.
pub(super) const MAX_CATALOG_BYTES: usize = 1024 * 1024;
/// How long a served, non-empty catalog may keep omitting the selected model.
///
/// A catalog that already lists other models proves the upstream is warm, so a missing model
/// is a configuration error rather than a cold start and must not consume the whole budget.
pub(super) const CATALOG_MODEL_GRACE: Duration = Duration::from_secs(10);
#[derive(Debug, Deserialize)]
struct ModelList {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}

/// Resolve the harness model: the explicit `--model`, or the first model the upstream serves.
///
/// # Errors
///
/// Returns a configuration error when no model is given and the upstream lists none.
pub async fn resolve_model(client: &Client, source: &SourceOptions, api_key: Option<&str>) -> Result<String, Error> {
    if let Some(model) = &source.model {
        return Ok(model.clone());
    }
    let Some(upstream) = &source.upstream else {
        return Ok(PLACEHOLDER_MODEL.to_owned());
    };
    let models_url = format!("{}/v1/models", agentic_core::config::normalize_base_url(upstream));
    let display_models_url = redact_url(&models_url);
    let display_upstream = redact_url(upstream);
    let mut request = client.get(&models_url);
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }
    let response = request
        .send()
        .await
        .map_err(|error| {
            Error::Config(format!(
                "failed to list upstream models at {display_models_url}: {}",
                error.without_url()
            ))
        })?
        .error_for_status()
        .map_err(|error| {
            Error::Config(format!(
                "upstream model listing at {display_models_url} failed: {}",
                error.without_url()
            ))
        })?;
    let body = response.text().await.map_err(|error| {
        Error::Config(format!(
            "failed to read model listing from {display_models_url}: {}",
            error.without_url()
        ))
    })?;
    let list: ModelList = agentic_core::utils::common::deserialize_from_str(&body)
        .map_err(|error| Error::Config(format!("invalid model listing from {display_models_url}: {error}")))?;
    let mut ids = list.data.into_iter().map(|entry| entry.id);
    let Some(model) = ids.next() else {
        return Err(Error::Config(format!(
            "upstream {display_upstream} serves no models; pass --model explicitly"
        )));
    };
    let remaining = ids.count();
    if remaining > 0 {
        eprintln!(
            "upstream serves {} models; using {model}. Pass --model to choose another.",
            remaining + 1
        );
    }
    Ok(model)
}

/// The Codex model the launcher runs and the input modalities the gateway resolved for it.
#[derive(Debug)]
pub(super) struct CodexModelSelection {
    pub(super) model: String,
    pub(super) input_modalities: InputModalities,
}

/// The polling budget for one catalog resolution.
#[derive(Clone, Copy, Debug)]
pub(super) struct CatalogBudget {
    /// Overall wall-clock budget for resolving the catalog.
    pub(super) timeout: Duration,
    /// Delay between attempts, unless the gateway asks for a longer one.
    pub(super) interval: Duration,
    /// How long a served, non-empty catalog may keep omitting the selected model.
    pub(super) missing_grace: Duration,
}

/// Why one catalog attempt failed, and whether another attempt could succeed.
enum CatalogAttempt {
    Resolved(CodexModelSelection),
    /// The gateway or its upstream may still be warming up.
    Transient(Error, Option<Duration>),
    /// Another attempt cannot change the result.
    Permanent(Error),
    /// The catalog is served and lists models, but not the selected one.
    ModelMissing(Error),
}

enum BodyError {
    TooLarge,
    Transport(reqwest::Error),
}

/// Statuses a warming gateway can return before it can serve its catalog.
fn is_transient_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || matches!(
            status,
            reqwest::StatusCode::REQUEST_TIMEOUT
                | reqwest::StatusCode::TOO_EARLY
                | reqwest::StatusCode::TOO_MANY_REQUESTS
        )
}

/// `Retry-After` expressed in whole seconds; the HTTP-date form is not honored.
fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Read a catalog response without trusting the gateway to bound it.
async fn read_bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, BodyError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
    {
        return Err(BodyError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(BodyError::Transport)? {
        if body.len() + chunk.len() > MAX_CATALOG_BYTES {
            return Err(BodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The models a catalog advertises, for an error message that names the alternatives.
fn advertised_models(catalog: &CodexCatalogCapabilities) -> String {
    const LISTED: usize = 5;
    let listed = catalog
        .models
        .iter()
        .take(LISTED)
        .map(|entry| entry.slug.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if catalog.models.len() > LISTED {
        format!("{listed}, ...")
    } else {
        listed
    }
}

/// Resolve the Codex CLI version the gateway catalog is requested for.
///
/// The gateway only transforms its model list when a client version is present, and Codex
/// reports its own version, so the launcher asks the same binary it is about to run instead of
/// inventing a value. [`CODEX_CLIENT_VERSION_ENV`] skips the probe where it cannot run.
///
/// # Errors
///
/// Returns a configuration error when the Codex binary cannot be run or reports no version.
async fn codex_client_version() -> Result<String, Error> {
    if let Some(version) = std::env::var(CODEX_CLIENT_VERSION_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        return Ok(version);
    }
    let binary = harness_binary(Harness::Codex);
    let display_binary = binary.to_string_lossy().into_owned();
    let mut command = tokio::process::Command::new(&binary);
    command
        .arg("--version")
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let output = tokio::time::timeout(CODEX_VERSION_PROBE_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            Error::Config(format!(
                "{display_binary} --version timed out after {}s; set {CODEX_CLIENT_VERSION_ENV} to skip the probe",
                CODEX_VERSION_PROBE_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|error| {
            Error::Config(format!(
                "failed to run {display_binary} --version: {error}; install Codex or set AGENTIC_CODEX_BIN"
            ))
        })?;
    if !output.status.success() {
        return Err(Error::Config(format!(
            "{display_binary} --version failed with {}; set {CODEX_CLIENT_VERSION_ENV} to skip the probe",
            output.status
        )));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| line.split_whitespace().next_back())
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::Config(format!(
                "could not read a version from {display_binary} --version; set {CODEX_CLIENT_VERSION_ENV} to provide it"
            ))
        })
}

/// Ask the gateway once for the model catalog and select the requested model.
async fn catalog_attempt(
    client: &Client,
    catalog_url: &str,
    display_url: &str,
    client_version: &str,
    requested_model: Option<&str>,
    api_key: Option<&str>,
) -> CatalogAttempt {
    let mut request = client.get(catalog_url).query(&[("client_version", client_version)]);
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            return CatalogAttempt::Transient(
                Error::Config(format!(
                    "failed to reach the gateway model catalog at {display_url}: {}",
                    error.without_url()
                )),
                None,
            );
        }
    };

    let status = response.status();
    if !status.is_success() {
        let retry_after = retry_after(&response);
        let message = format!("the gateway model catalog at {display_url} returned HTTP {status}");
        return if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            CatalogAttempt::Permanent(Error::Config(format!(
                "{message}; pass --api-key if the gateway requires authentication"
            )))
        } else if is_transient_status(status) {
            CatalogAttempt::Transient(Error::Config(message), retry_after)
        } else {
            CatalogAttempt::Permanent(Error::Config(message))
        };
    }

    let body = match read_bounded_body(response).await {
        Ok(body) => body,
        Err(BodyError::TooLarge) => {
            return CatalogAttempt::Permanent(Error::Config(format!(
                "the gateway model catalog at {display_url} is larger than {MAX_CATALOG_BYTES} bytes"
            )));
        }
        Err(BodyError::Transport(error)) => {
            return CatalogAttempt::Transient(
                Error::Config(format!(
                    "failed to read the gateway model catalog at {display_url}: {}",
                    error.without_url()
                )),
                None,
            );
        }
    };

    let catalog: CodexCatalogCapabilities = match serde_json::from_slice(&body) {
        Ok(catalog) => catalog,
        Err(error) => {
            return CatalogAttempt::Permanent(Error::Config(format!(
                "the gateway model catalog at {display_url} is not a Codex model catalog: {error}"
            )));
        }
    };
    if catalog.models.is_empty() {
        return CatalogAttempt::Transient(
            Error::Config(format!("the gateway model catalog at {display_url} lists no models")),
            None,
        );
    }
    let Some(entry) = catalog.select(requested_model) else {
        return CatalogAttempt::ModelMissing(Error::Config(format!(
            "the gateway model catalog at {display_url} does not list model {:?}; it serves: {}",
            requested_model.unwrap_or_default(),
            advertised_models(&catalog)
        )));
    };
    if requested_model.is_none() && catalog.models.len() > 1 {
        eprintln!(
            "gateway serves {} models; using {}. Pass --model to choose another.",
            catalog.models.len(),
            entry.slug
        );
    }
    CatalogAttempt::Resolved(CodexModelSelection {
        model: entry.slug.clone(),
        input_modalities: entry.input_modalities,
    })
}

/// Resolve the Codex model and its input modalities from one gateway catalog snapshot.
///
/// Selecting the model and reading its capabilities from the same response keeps the isolated
/// Codex catalog consistent with what the gateway serves over HTTP. Transient failures are
/// retried until `timeout` expires, because a gateway can answer `/health` before its upstream
/// can list models; authentication failures and undecodable catalogs are reported immediately.
///
/// # Errors
///
/// Returns a configuration error when the catalog cannot be fetched within `timeout`, the
/// gateway rejects the request, or the catalog does not list the selected model.
pub(super) async fn resolve_codex_selection(
    client: &Client,
    gateway_url: &str,
    requested_model: Option<&str>,
    api_key: Option<&str>,
    timeout: Duration,
    interval: Duration,
) -> Result<CodexModelSelection, Error> {
    let client_version = codex_client_version().await?;
    catalog_selection(
        client,
        gateway_url,
        &client_version,
        requested_model,
        api_key,
        CatalogBudget {
            timeout,
            interval,
            missing_grace: CATALOG_MODEL_GRACE,
        },
    )
    .await
}

/// Poll the gateway catalog for `requested_model` until it resolves or the budget expires.
pub(super) async fn catalog_selection(
    client: &Client,
    gateway_url: &str,
    client_version: &str,
    requested_model: Option<&str>,
    api_key: Option<&str>,
    budget: CatalogBudget,
) -> Result<CodexModelSelection, Error> {
    let base = gateway_url.trim_end_matches('/');
    let catalog_url = format!("{base}/v1/models");
    let display_url = redact_url(base);
    let deadline = Instant::now() + budget.timeout;
    // Set on the first miss rather than up front: a slow warm-up must not consume the grace a
    // served catalog is owed once it starts answering.
    let mut missing_deadline = None;
    let mut last_error = None;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(attempt) = tokio::time::timeout(
            remaining,
            catalog_attempt(
                client,
                &catalog_url,
                &display_url,
                client_version,
                requested_model,
                api_key,
            ),
        )
        .await
        else {
            break;
        };

        let delay = match attempt {
            CatalogAttempt::Resolved(selection) => return Ok(selection),
            CatalogAttempt::Permanent(error) => return Err(error),
            CatalogAttempt::ModelMissing(error) => {
                let now = Instant::now();
                if now >= *missing_deadline.get_or_insert((now + budget.missing_grace).min(deadline)) {
                    return Err(error);
                }
                last_error = Some(error);
                budget.interval
            }
            CatalogAttempt::Transient(error, retry_after) => {
                last_error = Some(error);
                retry_after.unwrap_or(budget.interval)
            }
        };

        let now = Instant::now();
        if now >= deadline {
            break;
        }
        sleep(delay.min(deadline - now)).await;
    }

    Err(last_error.unwrap_or_else(|| {
        Error::Config(format!(
            "the gateway model catalog at {display_url} did not become available"
        ))
    }))
}
