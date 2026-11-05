//! Typed declaration and `OpenAPI` schema for the built-in file search tool.

use serde::{Deserialize, Serialize};

/// Parameters for a file search tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct FileSearchToolParam {
    pub vector_store_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_num_results: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<crate::types::file_search::SearchFilter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ranking_options: Option<crate::types::file_search::RankingOptions>,
    /// `async` applies only to function and custom tools; captured so declaration validation can reject it.
    #[serde(default, rename = "async", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(ignore))]
    pub unsupported_async: Option<bool>,
}

#[cfg(feature = "openapi")]
pub(super) fn tagged_schema(type_value: &str) -> Option<utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>> {
    use utoipa::openapi::RefOr;
    use utoipa::openapi::schema::{ObjectBuilder, Schema, SchemaType, Type};

    // Strict parameter objects must include the discriminator in the same
    // object; an allOf sibling cannot extend additionalProperties:false.
    if type_value != "file_search" {
        return None;
    }
    let RefOr::T(Schema::Object(mut object)) = <FileSearchToolParam as utoipa::PartialSchema>::schema() else {
        return None;
    };
    object.properties.insert(
        "type".into(),
        ObjectBuilder::new()
            .schema_type(SchemaType::new(Type::String))
            .enum_values(Some([type_value]))
            .into(),
    );
    object.required.push("type".into());
    Some(object.into())
}
