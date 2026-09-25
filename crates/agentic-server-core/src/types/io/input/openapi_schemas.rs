use super::{InputContent, InputItem, InputMessageContent, ResponsesInput, ToolCallOutput};
use utoipa::openapi::schema::{ArrayBuilder, OneOfBuilder, Schema, SchemaType, Type};
use utoipa::openapi::{ObjectBuilder, Ref, RefOr};

fn string_schema() -> RefOr<Schema> {
    ObjectBuilder::new().schema_type(SchemaType::new(Type::String)).into()
}

impl utoipa::PartialSchema for InputMessageContent {
    fn schema() -> RefOr<Schema> {
        OneOfBuilder::new()
            .item(string_schema())
            .item(ArrayBuilder::new().items(Ref::from_schema_name("InputContent")))
            .into()
    }
}
impl utoipa::ToSchema for InputMessageContent {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("InputMessageContent")
    }
}

impl utoipa::PartialSchema for ToolCallOutput {
    fn schema() -> RefOr<Schema> {
        OneOfBuilder::new()
            .item(string_schema())
            .item(ArrayBuilder::new().items(Ref::from_schema_name("ToolOutputContent")))
            .into()
    }
}
impl utoipa::ToSchema for ToolCallOutput {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ToolCallOutput")
    }
}

impl utoipa::PartialSchema for ResponsesInput {
    fn schema() -> RefOr<Schema> {
        OneOfBuilder::new()
            .item(string_schema())
            .item(ArrayBuilder::new().items(Ref::from_schema_name("InputItem")))
            .into()
    }
}
impl utoipa::ToSchema for ResponsesInput {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ResponsesInput")
    }
}

fn tagged_text_variant(type_value: &str) -> RefOr<Schema> {
    ObjectBuilder::new()
        .property(
            "type",
            ObjectBuilder::new()
                .schema_type(SchemaType::new(Type::String))
                .enum_values(Some([type_value])),
        )
        .required("type")
        .property("text", ObjectBuilder::new().schema_type(SchemaType::new(Type::String)))
        .required("text")
        .into()
}

impl utoipa::PartialSchema for InputContent {
    fn schema() -> RefOr<Schema> {
        OneOfBuilder::new()
            .discriminator(Some(utoipa::openapi::schema::Discriminator::new("type")))
            .item(tagged_text_variant("input_text"))
            .item(
                ObjectBuilder::new()
                    .property(
                        "type",
                        ObjectBuilder::new()
                            .schema_type(SchemaType::new(Type::String))
                            .enum_values(Some(["input_image"])),
                    )
                    .required("type")
                    .property(
                        "file_id",
                        ObjectBuilder::new().schema_type(SchemaType::new(Type::String)),
                    )
                    .property(
                        "image_url",
                        ObjectBuilder::new().schema_type(SchemaType::new(Type::String)),
                    )
                    .property(
                        "detail",
                        ObjectBuilder::new().schema_type(SchemaType::new(Type::String)),
                    ),
            )
            .item(tagged_ref("input_file", "InputFileContent"))
            .item(tagged_text_variant("output_text"))
            .item(tagged_ref("refusal", "RefusalContent"))
            .item(tagged_text_variant("reasoning_text"))
            .into()
    }
}
impl utoipa::ToSchema for InputContent {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("InputContent")
    }
}

fn tagged_ref(type_value: &str, schema_name: &str) -> RefOr<Schema> {
    use utoipa::openapi::schema::AllOfBuilder;
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
        .item(Ref::from_schema_name(schema_name))
        .into()
}

impl utoipa::PartialSchema for InputItem {
    fn schema() -> RefOr<Schema> {
        use utoipa::openapi::schema::AllOfBuilder;
        let message_branch: RefOr<Schema> = AllOfBuilder::new()
            .item(
                ObjectBuilder::new().property(
                    "type",
                    ObjectBuilder::new()
                        .schema_type(SchemaType::new(Type::String))
                        .enum_values(Some(["message"])),
                ),
            )
            .item(Ref::from_schema_name("InputMessage"))
            .into();
        OneOfBuilder::new()
            .discriminator(Some(utoipa::openapi::schema::Discriminator::new("type")))
            .item(message_branch)
            .item(tagged_ref("function_call", "InputFunctionToolCall"))
            .item(tagged_ref("function_call_output", "FunctionToolResultMessage"))
            .item(tagged_ref("tool_search_call", "InputToolSearchCall"))
            .item(tagged_ref("tool_search_output", "ToolSearchOutputMessage"))
            .item(tagged_ref("custom_tool_call", "CustomToolCall"))
            .item(tagged_ref("custom_tool_call_output", "CustomToolCallOutputMessage"))
            .item(tagged_ref("shell_call", "ShellCall"))
            .item(tagged_ref("shell_call_output", "ShellCallOutputMessage"))
            .item(tagged_ref("reasoning", "ReasoningOutput"))
            .item(tagged_ref("mcp_list_tools", "McpListTools"))
            .item(tagged_ref("compaction", "CompactionItem"))
            .item(tagged_ref("multi_agent_call", "MultiAgentCall"))
            .item(tagged_ref("multi_agent_call_output", "MultiAgentCallOutput"))
            .item(tagged_ref("agent_message", "AgentMessage"))
            .item(
                ObjectBuilder::new()
                    .property(
                        "type",
                        ObjectBuilder::new()
                            .schema_type(SchemaType::new(Type::String))
                            .enum_values(Some(["compaction_trigger"])),
                    )
                    .required("type"),
            )
            .into()
    }
}
impl utoipa::ToSchema for InputItem {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("InputItem")
    }
}
