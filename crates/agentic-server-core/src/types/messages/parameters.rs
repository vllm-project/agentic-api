//! Typed Messages parameters shared by admission, fingerprints, and forwarding.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::HashMap;
#[cfg(feature = "openapi")]
use utoipa::{
    PartialSchema, ToSchema,
    openapi::{
        ObjectBuilder, RefOr,
        schema::{AnyOfBuilder, Schema, Type},
    },
};

/// Create-only fields distinguish omission from an explicit null on continuation.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum RequestField<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<T> RequestField<T> {
    #[must_use]
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
    #[must_use]
    pub fn as_ref(&self) -> Option<&T> {
        if let Self::Value(value) = self {
            Some(value)
        } else {
            None
        }
    }
    #[must_use]
    pub fn into_option(self) -> Option<T> {
        if let Self::Value(value) = self {
            Some(value)
        } else {
            None
        }
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for RequestField<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Option::<T>::deserialize(deserializer)?.map_or(Self::Null, Self::Value))
    }
}
impl<T: Serialize> Serialize for RequestField<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Value(value) => value.serialize(serializer),
            Self::Missing | Self::Null => serializer.serialize_none(),
        }
    }
}

/// Known thinking parameters validate their payload; future provider types retain all fields.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ThinkingConfig {
    Known(KnownThinkingConfig),
    Other(ProviderThinkingConfig),
}

impl<'de> Deserialize<'de> for ThinkingConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        // A malformed known type must not fall through to the opaque provider form.
        let known = KnownThinkingConfig::deserialize(&value).map_err(serde::de::Error::custom)?;
        match known {
            KnownThinkingConfig::Unknown => ProviderThinkingConfig::deserialize(value)
                .map(Self::Other)
                .map_err(serde::de::Error::custom),
            known => Ok(Self::Known(known)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KnownThinkingConfig {
    Enabled {
        budget_tokens: u32,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    BetweenTools {
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    Adaptive {
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    Disabled {
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    #[serde(other)]
    Unknown,
}

/// A provider thinking configuration the gateway forwards without interpreting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ProviderThinkingConfig {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: CacheControlKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}
/// Known cache-control types stay typed; future spellings are preserved verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheControlKind {
    Ephemeral,
    #[serde(untagged)]
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct McpServer {
    #[serde(flatten)]
    pub configuration: McpServerFingerprint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_token: Option<String>,
}

/// Includes all configuration fields, but cannot contain an authorization token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct McpServerFingerprint {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_configuration: Option<McpToolConfiguration>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct McpToolConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "openapi", schema(as = MessagesContextManagement))]
pub struct ContextManagement {
    #[serde(default, skip_serializing_if = "RequestField::is_missing")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Vec<ContextEdit>>))]
    pub edits: RequestField<Vec<ContextEdit>>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type")]
pub enum ContextEdit {
    #[serde(rename = "clear_thinking_20251015")]
    ClearThinking {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        keep: Option<ThinkingKeep>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    #[serde(other)]
    Unsupported,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(untagged)]
pub enum ThinkingKeep {
    All(KeepAll),
    Recent(ThinkingTurns),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum KeepAll {
    All,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(untagged)]
pub enum Container {
    Id(String),
    Configuration(ContainerConfig),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ContainerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<ContainerSkill>>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ContainerSkill {
    #[serde(rename = "type")]
    pub kind: String,
    pub skill_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Fallback {
    pub model: String,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Compaction is passed through for ordinary Messages and refused for threads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CompactionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Anthropic accepts default routing or an explicitly ordered fallback chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(untagged)]
pub enum Fallbacks {
    Default(DefaultFallbackRouting),
    Models(Vec<Fallback>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum DefaultFallbackRouting {
    Default,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ThinkingTurns {
    #[serde(rename = "type")]
    pub kind: ThinkingTurnsKind,
    pub value: u32,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ThinkingTurnsKind {
    ThinkingTurns,
}

#[cfg(feature = "openapi")]
impl PartialSchema for ThinkingConfig {
    fn schema() -> RefOr<Schema> {
        AnyOfBuilder::new()
            .item(KnownThinkingConfig::schema())
            .item(ProviderThinkingConfig::schema())
            .into()
    }
}
#[cfg(feature = "openapi")]
impl ToSchema for ThinkingConfig {}

#[cfg(feature = "openapi")]
impl PartialSchema for CacheControlKind {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new().schema_type(Type::String).into()
    }
}
#[cfg(feature = "openapi")]
impl ToSchema for CacheControlKind {}

#[cfg(test)]
mod tests;
