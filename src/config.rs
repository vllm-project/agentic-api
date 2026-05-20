use clap::Args;

#[derive(Debug, Clone, Args)]
pub struct RuntimeConfig {
    #[arg(skip)]
    pub llm_api_base: String,

    #[arg(long, env = "OPENAI_API_KEY", hide_env_values = true)]
    pub openai_api_key: Option<String>,

    #[arg(long, default_value = "0.0.0.0")]
    pub gateway_host: String,

    #[arg(long, default_value_t = 9000)]
    pub gateway_port: u16,

    #[arg(long, default_value = "http://localhost:8080")]
    pub ogx_base_url: String,

    #[arg(long, default_value_t = 10)]
    pub max_iterations: u32,
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
