use super::file_search;
use super::{CodexNamespaceMember, NonEmptyToolName, ResponsesTool, ShellEnvironment};

impl utoipa::PartialSchema for NonEmptyToolName {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::SchemaType::new(
                utoipa::openapi::schema::Type::String,
            ))
            .min_length(Some(1))
            .into()
    }
}

impl utoipa::ToSchema for NonEmptyToolName {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("NonEmptyToolName")
    }
}

impl utoipa::PartialSchema for ShellEnvironment {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::Ref;
        use utoipa::openapi::schema::{AllOfBuilder, ObjectBuilder, SchemaType, Type};

        AllOfBuilder::new()
            .item(
                ObjectBuilder::new()
                    .property(
                        "type",
                        ObjectBuilder::new()
                            .schema_type(SchemaType::new(Type::String))
                            .enum_values(Some(["local"])),
                    )
                    .required("type"),
            )
            .item(Ref::from_schema_name("LocalShellEnvironment"))
            .into()
    }
}

impl utoipa::ToSchema for ShellEnvironment {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ShellEnvironment")
    }
}

impl utoipa::PartialSchema for ResponsesTool {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::Ref;
        use utoipa::openapi::schema::{AdditionalProperties, AllOfBuilder, ObjectBuilder, SchemaType, Type};

        fn tagged(type_value: &str, schema: &str) -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
            if let Some(schema) = file_search::tagged_schema(type_value) {
                return schema;
            }
            AllOfBuilder::new()
                .item(
                    ObjectBuilder::new()
                        .property(
                            "type",
                            ObjectBuilder::new()
                                .schema_type(SchemaType::new(Type::String))
                                .enum_values(Some([type_value])),
                        )
                        .required("type"),
                )
                .item(Ref::from_schema_name(schema))
                .into()
        }

        // Keep the tagged declaration in one closed object so its schema
        // matches the serde tagged enum variant.
        fn code_interpreter_tagged() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
            ObjectBuilder::new()
                .property(
                    "type",
                    ObjectBuilder::new()
                        .schema_type(SchemaType::new(Type::String))
                        .enum_values(Some(["code_interpreter"])),
                )
                .property("container", Ref::from_schema_name("CodeInterpreterAutoContainer"))
                .required("type")
                .required("container")
                .additional_properties(Some(AdditionalProperties::FreeForm(false)))
                .into()
        }

        utoipa::openapi::schema::OneOfBuilder::new()
            .discriminator(Some(utoipa::openapi::schema::Discriminator::new("type")))
            .item(tagged("function", "FunctionToolParam"))
            .item(tagged("tool_search", "ToolSearchToolParam"))
            .item(tagged("mcp", "McpToolParam"))
            .item(
                AllOfBuilder::new()
                    .item(
                        ObjectBuilder::new()
                            .property(
                                "type",
                                ObjectBuilder::new()
                                    .schema_type(SchemaType::new(Type::String))
                                    .enum_values(Some([
                                        "web_search_preview",
                                        "web_search",
                                        "web_search_preview_2025_03_11",
                                        "web_search_2025_08_26",
                                    ])),
                            )
                            .required("type"),
                    )
                    .item(Ref::from_schema_name("WebSearchToolParam")),
            )
            .item(tagged("file_search", "FileSearchToolParam"))
            .item(code_interpreter_tagged())
            .item(tagged("shell", "ShellToolParam"))
            .item(tagged("namespace", "CodexNamespaceToolParam"))
            .item(tagged("custom", "CustomToolParam"))
            .into()
    }
}
#[cfg(feature = "openapi")]
impl utoipa::ToSchema for ResponsesTool {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ResponsesTool")
    }
}

impl utoipa::PartialSchema for CodexNamespaceMember {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::Ref;
        use utoipa::openapi::schema::{AllOfBuilder, ObjectBuilder, SchemaType, Type};

        utoipa::openapi::schema::OneOfBuilder::new()
            .discriminator(Some(utoipa::openapi::schema::Discriminator::new("type")))
            .item(
                AllOfBuilder::new()
                    .item(
                        ObjectBuilder::new()
                            .property(
                                "type",
                                ObjectBuilder::new()
                                    .schema_type(SchemaType::new(Type::String))
                                    .enum_values(Some(["function"])),
                            )
                            .required("type"),
                    )
                    .item(Ref::from_schema_name("FunctionToolParam")),
            )
            .into()
    }
}

impl utoipa::ToSchema for CodexNamespaceMember {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("CodexNamespaceMember")
    }
}
