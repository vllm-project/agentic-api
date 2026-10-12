//! Scoped HTTPS certificate trust for transport integration tests.
//! The normal DNS pinning, redirect policy, hostname verification and bounded transport still apply.
use super::{McpError, http_client};

tokio::task_local! {
    static ROOT_CERTIFICATE: http_client::Certificate;
}

/// Run a fixture request while trusting its local certificate, without modifying process or OS trust.
///
/// # Errors
/// Returns a client construction error if the fixture certificate is not valid DER.
pub async fn with_root_certificate<T>(
    certificate_der: Vec<u8>,
    future: impl std::future::Future<Output = T>,
) -> Result<T, McpError> {
    let certificate = http_client::Certificate::from_der(&certificate_der).map_err(McpError::BuildHttpClient)?;
    Ok(ROOT_CERTIFICATE.scope(certificate, future).await)
}

pub(super) fn apply_root_certificate(builder: http_client::ClientBuilder) -> http_client::ClientBuilder {
    match ROOT_CERTIFICATE.try_with(Clone::clone) {
        Ok(certificate) => builder.tls_certs_only([certificate]),
        Err(_) => builder,
    }
}
