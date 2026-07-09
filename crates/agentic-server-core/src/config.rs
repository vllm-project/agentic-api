#[derive(Debug, Clone)]
pub struct Config {
    pub llm_api_base: String,
    pub openai_api_key: Option<String>,
    pub llm_ready_timeout_s: f64,
    pub llm_ready_interval_s: f64,
    pub skip_llm_ready_check: bool,
    /// Database URL for conversation and response storage.
    /// `None` means stateful features are disabled; all requests are proxied.
    pub db_url: Option<String>,
}

#[must_use]
pub fn normalize_base_url(url: &str) -> String {
    let mut s = url.trim_end_matches('/').to_owned();
    if s.ends_with("/v1") {
        s.truncate(s.len() - 3);
        s = s.trim_end_matches('/').to_owned();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_trailing_v1() {
        assert_eq!(normalize_base_url("http://host:8000/v1"), "http://host:8000");
        assert_eq!(normalize_base_url("http://host:8000/v1/"), "http://host:8000");
    }

    #[test]
    fn no_v1_unchanged() {
        assert_eq!(normalize_base_url("http://host:8000"), "http://host:8000");
        assert_eq!(normalize_base_url("http://host:8000/"), "http://host:8000");
    }
}
