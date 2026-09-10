use crate::events::WireEvent;
use crate::executor::error::ExecutorResult;
use crate::tool::custom::CustomToolMap;
use crate::tool::{NamespaceMap, ToolType};
use crate::types::event::ResponseStatus;
use crate::types::io::OutputItem;
use crate::types::io::ToolChoice;
use crate::types::request_response::ResponsePayload;
use crate::types::tools::ResponsesTool;
use std::collections::{HashMap, HashSet};

/// An owned snapshot of the effective tool classification and availability for one round.
#[derive(Default)]
pub(in crate::executor) struct TranslationContext {
    tool_types: HashMap<String, ToolType>,
    gateway_tool_names: HashSet<String>,
    withheld_function_names: HashSet<String>,
    tool_search_active: bool,
    namespace_map: Option<NamespaceMap>,
    custom_tool_map: Option<CustomToolMap>,
    response_tools: Option<Vec<ResponsesTool>>,
    response_tool_choice: Option<ToolChoice>,
}

impl std::fmt::Debug for TranslationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranslationContext")
            .field("tool_count", &self.tool_types.len())
            .field("withheld_count", &self.withheld_function_names.len())
            .field("tool_search_active", &self.tool_search_active)
            .finish_non_exhaustive()
    }
}

impl TranslationContext {
    pub(in crate::executor) fn new(
        tool_types: HashMap<String, ToolType>,
        withheld_function_names: HashSet<String>,
        tool_search_active: bool,
    ) -> Self {
        Self {
            tool_types,
            withheld_function_names,
            tool_search_active,
            ..Self::default()
        }
    }

    pub(in crate::executor) fn with_gateway_tools(mut self, names: HashSet<String>) -> Self {
        self.gateway_tool_names = names;
        self
    }

    pub(super) fn is_gateway_owned_name(&self, name: &str) -> bool {
        self.gateway_tool_names.contains(name) || self.tool_type(name).is_gateway_owned()
    }

    /// Owned public mappings; construction performs no registry lookups.
    pub(in crate::executor) fn with_response_metadata(
        mut self,
        namespace_map: Option<NamespaceMap>,
        custom_tool_map: Option<CustomToolMap>,
        response_tools: Option<Vec<ResponsesTool>>,
        response_tool_choice: Option<ToolChoice>,
    ) -> Self {
        self.namespace_map = namespace_map;
        self.custom_tool_map = custom_tool_map;
        self.response_tools = super::tool_search::public_response_tools(response_tools);
        self.response_tool_choice = response_tool_choice;
        self
    }

    pub(super) fn restore_stream_event_wire(&self, wire: &mut WireEvent) -> ExecutorResult<()> {
        super::tool_search::restore_response_tools(
            wire,
            self.response_tools.as_deref(),
            self.response_tool_choice.as_ref(),
        )?;
        super::custom::CustomTranslator::restore_response_wire(wire, self.custom_tool_map.as_ref());
        let _ = super::namespace::CodexNamespaceTranslator::restore_response_wire(wire, self.namespace_map.as_ref());
        Ok(())
    }

    pub(super) fn restore_response_metadata(&self, payload: &mut ResponsePayload) {
        if let Some(tools) = &self.response_tools {
            payload.tools = Some(tools.clone());
            payload.tool_choice = Some(self.response_tool_choice.clone().unwrap_or_default());
        }
    }

    pub(in crate::executor) fn tool_type(&self, name: &str) -> ToolType {
        if self.tool_search_active && name == crate::tool::tool_search::TOOL_SEARCH_NAME {
            ToolType::ToolSearch
        } else {
            self.tool_types.get(name).copied().unwrap_or(ToolType::Function)
        }
    }

    pub(super) fn is_withheld_function(&self, name: &str) -> bool {
        self.withheld_function_names.contains(name)
    }

    pub(super) fn tool_search_is_active(&self) -> bool {
        self.tool_search_active
    }

    pub(super) fn validate_json_body(&self, body: &str) -> ExecutorResult<()> {
        crate::tool::tool_search::validate_blocking_response(
            body,
            self.tool_search_active,
            &self.withheld_function_names,
        )?;
        Ok(())
    }

    pub(in crate::executor) fn normalize_response_output(
        &self,
        output: &mut Vec<OutputItem>,
        status: ResponseStatus,
        unfinished_stream_item_ids: &HashSet<String>,
    ) -> ExecutorResult<()> {
        super::tool_search::normalize_response_output(self, output, status, unfinished_stream_item_ids)?;
        super::namespace::CodexNamespaceTranslator::restore_output_items(output, self.namespace_map.as_ref());
        Ok(())
    }
}
