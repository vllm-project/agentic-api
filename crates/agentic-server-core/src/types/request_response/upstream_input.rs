//! The model's view of a request's tools and input.
//!
//! `types::io::input` projects search items; this resolves namespaced calls against the request's declarations and
//! lowers async calls, so a live round, a continuation, a replay and a compaction summary show the model its history
//! the same way.

use std::borrow::Cow;

use super::{RequestPayload, UpstreamRequestError, UpstreamTool};
use crate::tool::async_execution::lower_async_calls;
use crate::tool::{CodexNamespaceHandler, NamespaceMap, ToolDeclaration};
use crate::types::io::ResponsesInput;
use crate::types::io::input::{HistoryNaming, InputValidationError};

impl RequestPayload {
    /// The function tools offered upstream for `declarations`, with namespace members under their flat names.
    ///
    /// # Errors
    ///
    /// Returns a tool error for a namespace member name collision or a declaration that cannot be lowered.
    pub(super) fn upstream_tools(
        declarations: Option<&[ToolDeclaration]>,
    ) -> Result<Option<Vec<UpstreamTool>>, UpstreamRequestError> {
        let Some(renamed) = declarations
            .map(|tools| CodexNamespaceHandler.resolve_namespace_members(tools))
            .transpose()?
        else {
            return Ok(None);
        };
        for tool in &renamed {
            tool.validate()?;
        }
        let tools: Vec<UpstreamTool> = renamed
            .iter()
            .flat_map(ToolDeclaration::to_function_tools)
            .map(UpstreamTool::Function)
            .collect();
        Ok(Some(tools).filter(|tools| !tools.is_empty()))
    }

    /// The top-level functions this request knows: those it declares and those a search in its input loaded. A name the
    /// namespace map resolves as a member of its own namespace is left out: a live request reads such a call as
    /// that member, and so must its summary. The names are naming context only: declarations whose namespace
    /// members collide, which a generating request rejects, give no map to consult.
    #[must_use]
    pub(crate) fn top_level_function_names(&self) -> Vec<String> {
        let declarations = self.tool_declarations();
        let map = CodexNamespaceHandler
            .build_namespace_map(declarations.as_deref())
            .ok()
            .flatten();
        let declared = declarations.iter().flatten().filter_map(ToolDeclaration::function_name);
        let mut names: Vec<String> = self
            .input
            .searched_function_names()
            .into_iter()
            .chain(declared)
            .map(str::to_owned)
            .collect();
        names.sort_unstable();
        names.dedup();
        names.retain(|name| {
            map.as_ref()
                .is_none_or(|map| map.mapping_for_member(name, name).is_none())
        });
        names
    }

    /// The input as the model reads it, using the namespace map and naming context built from this request.
    ///
    /// # Errors
    ///
    /// Returns a file that cannot be converted to text.
    pub(super) fn upstream_input(
        &self,
        map: Option<&NamespaceMap>,
        history: &HistoryNaming,
    ) -> Result<Cow<'_, ResponsesInput>, InputValidationError> {
        let functions = history.functions.iter().map(String::as_str).collect();
        Ok(lower_async_calls(
            CodexNamespaceHandler.resolve_input(
                map,
                &functions,
                self.input
                    .normalized_model_input(self.tools.as_deref().unwrap_or_default())?,
            ),
        ))
    }
}
