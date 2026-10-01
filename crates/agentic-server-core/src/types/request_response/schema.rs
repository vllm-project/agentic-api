use super::{MultiAgentConfig, RequestPayload, ResponseTextFormat};
use utoipa::openapi::schema::{AllOfBuilder, ArrayBuilder, ObjectBuilder, OneOfBuilder, SchemaType, Type};
use utoipa::openapi::{Ref, RefOr};

impl utoipa::PartialSchema for ResponseTextFormat {
    fn schema() -> RefOr<utoipa::openapi::schema::Schema> {
        let str_type = || ObjectBuilder::new().schema_type(SchemaType::new(Type::String));

        OneOfBuilder::new()
            .discriminator(Some(utoipa::openapi::schema::Discriminator::new("type")))
            .item(
                AllOfBuilder::new().item(
                    ObjectBuilder::new()
                        .property("type", str_type().enum_values(Some(["text"])))
                        .required("type"),
                ),
            )
            .item(
                AllOfBuilder::new().item(
                    ObjectBuilder::new()
                        .property("type", str_type().enum_values(Some(["json_object"])))
                        .required("type"),
                ),
            )
            .item(
                AllOfBuilder::new().item(
                    ObjectBuilder::new()
                        .property("type", str_type().enum_values(Some(["json_schema"])))
                        .required("type")
                        .property("name", str_type())
                        .required("name")
                        .property("schema", ObjectBuilder::new())
                        .required("schema")
                        .property("description", str_type())
                        .property(
                            "strict",
                            ObjectBuilder::new().schema_type(SchemaType::new(Type::Boolean)),
                        ),
                ),
            )
            .into()
    }
}

impl utoipa::ToSchema for ResponseTextFormat {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ResponseTextFormat")
    }
}

impl utoipa::PartialSchema for RequestPayload {
    fn schema() -> RefOr<utoipa::openapi::schema::Schema> {
        let str_type = || ObjectBuilder::new().schema_type(SchemaType::new(Type::String));
        let bool_type = || ObjectBuilder::new().schema_type(SchemaType::new(Type::Boolean));
        let null_type = || ObjectBuilder::new().schema_type(SchemaType::new(Type::Null));
        let nullable_str = || ObjectBuilder::new().schema_type(SchemaType::from_iter([Type::String, Type::Null]));
        let nullable_num = || ObjectBuilder::new().schema_type(SchemaType::from_iter([Type::Number, Type::Null]));
        let nullable_int = || ObjectBuilder::new().schema_type(SchemaType::from_iter([Type::Integer, Type::Null]));
        let nullable_bool = || ObjectBuilder::new().schema_type(SchemaType::from_iter([Type::Boolean, Type::Null]));
        let nullable_ref = |name: &str| OneOfBuilder::new().item(Ref::from_schema_name(name)).item(null_type());
        let nullable_array = |item: RefOr<utoipa::openapi::schema::Schema>| {
            OneOfBuilder::new()
                .item(ArrayBuilder::new().items(item))
                .item(null_type())
        };
        ObjectBuilder::new()
            .property("model", str_type())
            .required("model")
            .property("input", Ref::from_schema_name("ResponsesInput"))
            .required("input")
            .property("instructions", nullable_str())
            .property("previous_response_id", nullable_str())
            .property("conversation", nullable_str())
            .property("tools", nullable_array(Ref::from_schema_name("ResponsesTool").into()))
            .property("tool_choice", nullable_ref("ToolChoice"))
            .property("stream", bool_type())
            .property("store", bool_type())
            .property("include", nullable_array(str_type().into()))
            .property("reasoning", nullable_ref("ReasoningConfig"))
            .property("text", nullable_ref("ResponseTextConfig"))
            .property("temperature", nullable_num())
            .property("top_p", nullable_num())
            .property("max_output_tokens", nullable_int())
            .property("max_tool_calls", nullable_int())
            .property("ignore_eos", nullable_bool())
            .property("truncation", nullable_str())
            .property(
                "metadata",
                ObjectBuilder::new().schema_type(SchemaType::from_iter([Type::Object, Type::Null])),
            )
            .property("parallel_tool_calls", nullable_bool())
            .property("prompt_cache_key", nullable_str())
            .property("service_tier", nullable_str())
            .property(
                "multi_agent",
                OneOfBuilder::new()
                    .item(<MultiAgentConfig as utoipa::PartialSchema>::schema())
                    .item(null_type()),
            )
            .property("cache_salt", nullable_str())
            .property(
                "context_management",
                nullable_array(Ref::from_schema_name("ContextManagement").into()),
            )
            .into()
    }
}

impl utoipa::ToSchema for RequestPayload {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("RequestPayload")
    }
}
