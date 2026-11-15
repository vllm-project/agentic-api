//! The `require_approval` policy of an MCP tool declaration.

use serde::{Deserialize, Serialize};

/// Which tools of an MCP server need MCP approval before the gateway calls them: `"always"`, `"never"`, or a filter
/// object, as in the Responses API `mcp` tool. The gateway has no approval step yet, so a request is accepted only
/// when no tool it can call needs approval. A request-declared server must set a policy; a configured server's
/// `require_approval = "never"` applies when the request omits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum McpApprovalPolicy {
    /// Every tool needs approval, or none does.
    Mode(McpApprovalMode),
    /// Tools matched by `never` run without approval. Tools matched by `always`, and tools that neither filter
    /// matches, need approval: the Responses API default.
    Filter(McpApprovalFilter),
}

/// The string form of [`McpApprovalPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum McpApprovalMode {
    Always,
    Never,
}

/// The object form of [`McpApprovalPolicy`]. It serializes as the Responses API echoes it: both sides, `null` when
/// absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct McpApprovalFilter {
    #[serde(default)]
    pub always: Option<McpApprovalToolFilter>,
    #[serde(default)]
    pub never: Option<McpApprovalToolFilter>,
}

/// The tools one side of an [`McpApprovalFilter`] matches. It serializes as the Responses API echoes it: `read_only`
/// is `null` and `tool_names` is `[]` when the declaration leaves them out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct McpApprovalToolFilter {
    #[serde(default, serialize_with = "serialize_tool_names")]
    pub tool_names: Option<Vec<String>>,
    /// Matching by the MCP `readOnlyHint` annotation; parsed so validation can reject it.
    #[serde(default)]
    pub read_only: Option<bool>,
}

#[allow(clippy::ref_option)] // serde's serialize_with passes a reference to the field.
fn serialize_tool_names<S>(names: &Option<Vec<String>>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    names.as_deref().unwrap_or_default().serialize(serializer)
}

/// A policy shape the gateway does not support.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpApprovalPolicyError {
    #[error("MCP require_approval filters match tools by tool_names only; read_only is not supported")]
    ReadOnlyFilter,
    #[error("MCP require_approval lists tool '{0}' under both always and never")]
    ConflictingTool(String),
}

impl McpApprovalPolicy {
    /// Whether calling `tool_name` needs approval under this policy.
    #[must_use]
    pub fn requires_approval(&self, tool_name: &str) -> bool {
        match self {
            Self::Mode(mode) => *mode == McpApprovalMode::Always,
            Self::Filter(filter) => {
                McpApprovalToolFilter::names(filter.always.as_ref(), tool_name)
                    || !McpApprovalToolFilter::names(filter.never.as_ref(), tool_name)
            }
        }
    }

    /// Checks the parts of the policy that do not depend on the server's tools.
    ///
    /// # Errors
    ///
    /// Returns an error for a `read_only` filter, or for a tool listed under both `always` and `never`.
    pub fn validate(&self) -> Result<(), McpApprovalPolicyError> {
        let Self::Filter(filter) = self else {
            return Ok(());
        };
        let sides = [filter.always.as_ref(), filter.never.as_ref()];
        if sides.iter().flatten().any(|side| side.read_only.is_some()) {
            return Err(McpApprovalPolicyError::ReadOnlyFilter);
        }
        let never = filter
            .never
            .as_ref()
            .and_then(|side| side.tool_names.as_deref())
            .unwrap_or_default();
        if let Some(name) = filter
            .always
            .as_ref()
            .and_then(|side| side.tool_names.as_deref())
            .unwrap_or_default()
            .iter()
            .find(|name| never.contains(name))
        {
            return Err(McpApprovalPolicyError::ConflictingTool(name.clone()));
        }
        Ok(())
    }
}

impl McpApprovalToolFilter {
    fn names(filter: Option<&Self>, tool_name: &str) -> bool {
        filter
            .and_then(|filter| filter.tool_names.as_deref())
            .is_some_and(|names| names.iter().any(|name| name == tool_name))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{McpApprovalFilter, McpApprovalMode, McpApprovalPolicy, McpApprovalPolicyError, McpApprovalToolFilter};

    fn policy(value: serde_json::Value) -> McpApprovalPolicy {
        serde_json::from_value(value).unwrap()
    }

    fn rejected(value: serde_json::Value) -> bool {
        serde_json::from_value::<McpApprovalPolicy>(value).is_err()
    }

    #[test]
    fn string_forms_round_trip_and_filters_serialize_as_the_responses_api_echoes_them() {
        for value in [json!("never"), json!("always")] {
            assert_eq!(serde_json::to_value(policy(value.clone())).unwrap(), value);
        }
        // As in tests/cassettes/mcp/mcp-approval-openai-gpt-5.6-sol-nonstreaming.yaml.
        for (value, echoed) in [
            (
                json!({"never": {"tool_names": ["ask_question", "read_wiki_structure"]}}),
                json!({"always": null,
                       "never": {"read_only": null, "tool_names": ["ask_question", "read_wiki_structure"]}}),
            ),
            (
                json!({"always": {"tool_names": ["delete"]}, "never": {"tool_names": ["read"]}}),
                json!({"always": {"read_only": null, "tool_names": ["delete"]},
                       "never": {"read_only": null, "tool_names": ["read"]}}),
            ),
            (
                json!({"never": {"read_only": true}}),
                json!({"always": null, "never": {"read_only": true, "tool_names": []}}),
            ),
            (json!({}), json!({"always": null, "never": null})),
        ] {
            let echo = serde_json::to_value(policy(value.clone())).unwrap();
            assert_eq!(echo, echoed, "{value}");
            assert_eq!(
                serde_json::to_value(policy(echo)).unwrap(),
                echoed,
                "{value}: the echo parses back"
            );
        }
        assert_eq!(policy(json!("never")), McpApprovalPolicy::Mode(McpApprovalMode::Never));
        assert_eq!(
            policy(json!({"never": {"tool_names": ["read"]}})),
            McpApprovalPolicy::Filter(McpApprovalFilter {
                always: None,
                never: Some(McpApprovalToolFilter {
                    tool_names: Some(vec!["read".to_owned()]),
                    read_only: None,
                }),
            })
        );
    }

    #[test]
    fn unknown_modes_and_fields_are_rejected() {
        for value in [
            json!("auto"),
            json!({"nevr": {"tool_names": []}}),
            json!({"never": {"tools": ["read"]}}),
            json!({"never": ["read"]}),
            json!(true),
        ] {
            assert!(rejected(value.clone()), "{value} should be rejected");
        }
    }

    #[test]
    fn tools_outside_never_need_approval() {
        let filter = policy(json!({"always": {"tool_names": ["delete"]}, "never": {"tool_names": ["read"]}}));
        assert!(!filter.requires_approval("read"));
        assert!(filter.requires_approval("delete"));
        assert!(filter.requires_approval("write"));
        assert!(policy(json!({})).requires_approval("read"));
        assert!(policy(json!("always")).requires_approval("read"));
        assert!(!policy(json!("never")).requires_approval("read"));
    }

    #[test]
    fn validation_rejects_read_only_and_conflicting_filters() {
        assert_eq!(
            policy(json!({"never": {"read_only": true}})).validate(),
            Err(McpApprovalPolicyError::ReadOnlyFilter)
        );
        assert_eq!(
            policy(json!({"always": {"tool_names": ["read"]}, "never": {"tool_names": ["list", "read"]}})).validate(),
            Err(McpApprovalPolicyError::ConflictingTool("read".to_owned()))
        );
        assert_eq!(policy(json!({"never": {"tool_names": ["read"]}})).validate(), Ok(()));
        assert_eq!(policy(json!("always")).validate(), Ok(()));
    }
}
