//! Wire types for the client-executed `computer` tool: `computer_call` items, their UI actions, and the
//! `computer_call_output` screenshot a client returns.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use super::input::InputImageContent;
use super::multi_agent::AgentAttribution;
use crate::types::RequestField;
use crate::types::event::MessageStatus;

/// The function name a computer call takes in model-facing history.
pub(crate) const COMPUTER_FUNCTION_NAME: &str = "computer";

/// Lifecycle status for a computer call or computer call output item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ComputerCallStatus {
    InProgress,
    Completed,
    Incomplete,
}

impl ComputerCallStatus {
    /// The status a lowered function call or output carries; an incomplete item has none.
    #[must_use]
    pub(crate) const fn message_status(self) -> Option<MessageStatus> {
        match self {
            Self::InProgress => Some(MessageStatus::InProgress),
            Self::Completed => Some(MessageStatus::Completed),
            Self::Incomplete => None,
        }
    }
}

/// The mouse button of a `click` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ComputerMouseButton {
    Left,
    Right,
    Wheel,
    Back,
    Forward,
}

/// A screen coordinate on a `drag` path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ComputerPoint {
    pub x: i64,
    pub y: i64,
    #[serde(default, flatten)]
    pub extra: HashMap<String, Value>,
}

/// Modifier keys held during a pointer action: a list or an explicit null.
/// The enclosing `Option` distinguishes either case from an omitted field.
///
/// `OpenAPI` publishes that field as `Option<Vec<String>>`. Deriving it as `Option<ComputerKeys>`
/// builds `oneOf(null, ComputerKeys)`, and this null variant makes an explicit null match both
/// branches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(untagged)]
pub enum ComputerKeys {
    Keys(Vec<String>),
    Null,
}

fn deserialize_optional_keys<'de, D>(deserializer: D) -> Result<Option<ComputerKeys>, D::Error>
where
    D: Deserializer<'de>,
{
    ComputerKeys::deserialize(deserializer).map(Some)
}

/// A documented UI action. Each variant keeps unmodeled fields in `extra`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KnownComputerAction {
    Click {
        button: ComputerMouseButton,
        x: i64,
        y: i64,
        /// `None` omits the field; `Some(ComputerKeys::Null)` preserves an explicit null.
        #[serde(
            default,
            deserialize_with = "deserialize_optional_keys",
            skip_serializing_if = "Option::is_none"
        )]
        #[cfg_attr(feature = "openapi", schema(value_type = Option<Vec<String>>))]
        keys: Option<ComputerKeys>,
        #[serde(default, flatten)]
        extra: HashMap<String, Value>,
    },
    DoubleClick {
        x: i64,
        y: i64,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_keys",
            skip_serializing_if = "Option::is_none"
        )]
        #[cfg_attr(feature = "openapi", schema(value_type = Option<Vec<String>>))]
        keys: Option<ComputerKeys>,
        #[serde(default, flatten)]
        extra: HashMap<String, Value>,
    },
    Drag {
        path: Vec<ComputerPoint>,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_keys",
            skip_serializing_if = "Option::is_none"
        )]
        #[cfg_attr(feature = "openapi", schema(value_type = Option<Vec<String>>))]
        keys: Option<ComputerKeys>,
        #[serde(default, flatten)]
        extra: HashMap<String, Value>,
    },
    Keypress {
        keys: Vec<String>,
        #[serde(default, flatten)]
        extra: HashMap<String, Value>,
    },
    Move {
        x: i64,
        y: i64,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_keys",
            skip_serializing_if = "Option::is_none"
        )]
        #[cfg_attr(feature = "openapi", schema(value_type = Option<Vec<String>>))]
        keys: Option<ComputerKeys>,
        #[serde(default, flatten)]
        extra: HashMap<String, Value>,
    },
    Scroll {
        x: i64,
        y: i64,
        scroll_x: i64,
        scroll_y: i64,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_keys",
            skip_serializing_if = "Option::is_none"
        )]
        #[cfg_attr(feature = "openapi", schema(value_type = Option<Vec<String>>))]
        keys: Option<ComputerKeys>,
        #[serde(default, flatten)]
        extra: HashMap<String, Value>,
    },
    Type {
        text: String,
        #[serde(default, flatten)]
        extra: HashMap<String, Value>,
    },
    Wait {
        #[serde(default, flatten)]
        extra: HashMap<String, Value>,
    },
    Screenshot {
        #[serde(default, flatten)]
        extra: HashMap<String, Value>,
    },
}

/// One UI action of a computer call. An action type the gateway does not model is kept verbatim, including its
/// `type`, so it survives storage and continuation; a malformed documented action is rejected instead.
///
/// Deserialization is hand-written because serde's `other` fallback cannot carry the unmodeled payload.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ComputerAction {
    Known(KnownComputerAction),
    Unknown(Map<String, Value>),
}

#[cfg(feature = "openapi")]
impl utoipa::PartialSchema for ComputerAction {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::Ref;
        use utoipa::openapi::extensions::Extensions;
        use utoipa::openapi::schema::{AnyOfBuilder, ObjectBuilder, SchemaType, Type};

        // utoipa has no `not` builder. A known `type` stays on `KnownComputerAction`, so a broken
        // `click` cannot also match this catch-all. `zoom` and other unmodeled types still can.
        let unknown_type: Extensions = [(
            "not",
            serde_json::json!({
                "enum": [
                    "click",
                    "double_click",
                    "drag",
                    "keypress",
                    "move",
                    "scroll",
                    "type",
                    "wait",
                    "screenshot",
                ]
            }),
        )]
        .into_iter()
        .collect();

        AnyOfBuilder::new()
            .item(Ref::from_schema_name("KnownComputerAction"))
            .item(
                ObjectBuilder::new()
                    .schema_type(SchemaType::new(Type::Object))
                    .property(
                        "type",
                        ObjectBuilder::new()
                            .schema_type(SchemaType::new(Type::String))
                            .extensions(Some(unknown_type)),
                    )
                    .required("type"),
            )
            .into()
    }
}

#[cfg(feature = "openapi")]
impl utoipa::ToSchema for ComputerAction {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ComputerAction")
    }
}

/// The action tags [`KnownComputerAction`] models; anything else is [`ComputerAction::Unknown`].
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ComputerActionKind {
    Click,
    DoubleClick,
    Drag,
    Keypress,
    Move,
    Scroll,
    Type,
    Wait,
    Screenshot,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ComputerActionTag {
    #[serde(rename = "type")]
    kind: ComputerActionKind,
}

impl<'de> Deserialize<'de> for ComputerAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let tag = ComputerActionTag::deserialize(&value).map_err(serde::de::Error::custom)?;
        match (tag.kind, value) {
            (ComputerActionKind::Other, Value::Object(action)) => Ok(Self::Unknown(action)),
            (ComputerActionKind::Other, _) => Err(serde::de::Error::custom("computer action must be an object")),
            (_, value) => KnownComputerAction::deserialize(value)
                .map(Self::Known)
                .map_err(serde::de::Error::custom),
        }
    }
}

/// A safety check the provider raised on a computer call, or one the client acknowledges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ComputerSafetyCheck {
    pub id: String,
    #[serde(default, skip_serializing_if = "RequestField::is_missing")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub code: RequestField<String>,
    #[serde(default, skip_serializing_if = "RequestField::is_missing")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub message: RequestField<String>,
    #[serde(default, flatten)]
    pub extra: HashMap<String, Value>,
}

/// A model-generated request to perform UI actions on a client-controlled computer.
///
/// `actions` is the batch the `computer` tool returns; `action` is the single action of
/// `computer_use_preview` history. Both are kept as sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ComputerCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAttribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ComputerAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<ComputerAction>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_safety_checks: Option<Vec<ComputerSafetyCheck>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ComputerCallStatus>,
    #[serde(default, flatten)]
    pub extra: HashMap<String, Value>,
}

impl ComputerCall {
    /// The actions in execution order: a non-empty `actions` batch when present, else the single `action`.
    #[must_use]
    pub fn ordered_actions(&self) -> &[ComputerAction] {
        self.actions
            .as_deref()
            .filter(|actions| !actions.is_empty())
            .unwrap_or(self.action.as_slice())
    }
}

/// The result a client returns for a computer call: a screenshot of the screen after the actions ran.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ComputerCallOutputContent {
    ComputerScreenshot(InputImageContent),
}

/// Output supplied for a previously emitted computer call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ComputerCallOutputMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub call_id: String,
    pub output: ComputerCallOutputContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_safety_checks: Option<Vec<ComputerSafetyCheck>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ComputerCallStatus>,
    #[serde(default, flatten)]
    pub extra: HashMap<String, Value>,
}

#[cfg(test)]
mod tests;
