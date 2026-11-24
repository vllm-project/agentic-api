//! Translator associations for client-executed tools, including namespace members.

use super::custom::CustomTranslator;
use super::function::FunctionTranslator;
use super::namespace::CodexNamespaceTranslator;
use super::shell::ShellTranslator;
use super::tool_search::ToolSearchTranslator;
use super::{CallExecution, HasTranslator};
use crate::tool::{CodexNamespaceHandler, CustomHandler, FunctionHandler, ShellHandler, ToolSearchHandler};

impl HasTranslator for ShellHandler {
    type Translator = ShellTranslator;

    fn new_translator(_execution: CallExecution) -> Self::Translator {
        ShellTranslator::default()
    }
}

impl HasTranslator for FunctionHandler {
    type Translator = FunctionTranslator;

    fn new_translator(execution: CallExecution) -> Self::Translator {
        FunctionTranslator::new(execution)
    }
}

impl HasTranslator for CustomHandler {
    type Translator = CustomTranslator;

    fn new_translator(execution: CallExecution) -> Self::Translator {
        CustomTranslator::new(execution)
    }
}

impl HasTranslator for ToolSearchHandler {
    type Translator = ToolSearchTranslator;

    fn new_translator(_execution: CallExecution) -> Self::Translator {
        ToolSearchTranslator::default()
    }
}

impl HasTranslator for CodexNamespaceHandler {
    type Translator = CodexNamespaceTranslator;

    fn new_translator(execution: CallExecution) -> Self::Translator {
        CodexNamespaceTranslator::new(execution)
    }
}
