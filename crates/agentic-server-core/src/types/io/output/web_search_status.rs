use serde::{Deserialize, Serialize};

use super::GatewayCallStatus;

/// Lifecycle status of a `web_search_call` item.
///
/// `searching` is also terminal for a call that was not executed because the
/// response reached its `max_tool_calls` limit, matching the Responses API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum WebSearchCallStatus {
    InProgress,
    Searching,
    Completed,
    Failed,
}

impl WebSearchCallStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Searching => "searching",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

impl From<GatewayCallStatus> for WebSearchCallStatus {
    fn from(status: GatewayCallStatus) -> Self {
        match status {
            GatewayCallStatus::InProgress => Self::InProgress,
            GatewayCallStatus::Completed => Self::Completed,
            GatewayCallStatus::Failed => Self::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searching_round_trips_with_the_wire_name() {
        let status: WebSearchCallStatus = serde_json::from_str("\"searching\"").unwrap();
        assert_eq!(status, WebSearchCallStatus::Searching);
        assert_eq!(serde_json::to_string(&status).unwrap(), "\"searching\"");
    }
}
