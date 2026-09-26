use serde::Serialize;

use crate::types::io::ToolChoice;

pub(super) const fn default_true() -> bool {
    true
}

// serde requires a `&Option<T>` receiver, so the idiomatic `Option<&T>` does not apply here.
#[allow(clippy::ref_option)]
pub(super) fn is_absent_or_default_tool_choice(choice: &Option<ToolChoice>) -> bool {
    choice.as_ref().is_none_or(|choice| matches!(choice, ToolChoice::Auto))
}

// serde's `serialize_with` passes a reference to the field's concrete type.
#[allow(clippy::ref_option)]
pub(super) fn serialize_upstream_tool_choice<S>(choice: &Option<ToolChoice>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    choice
        .as_ref()
        .map(ToolChoice::normalized_for_upstream)
        .serialize(serializer)
}
