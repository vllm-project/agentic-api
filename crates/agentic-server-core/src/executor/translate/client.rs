//! Translator associations for client-executed tools, including namespace members.

use super::HasTranslator;
use super::custom::CustomTranslator;
use super::function::FunctionTranslator;
use super::namespace::CodexNamespaceTranslator;
use super::tool_search::ToolSearchTranslator;
use crate::tool::{CodexNamespaceHandler, CustomHandler, FunctionHandler, ToolSearchHandler};

impl HasTranslator for FunctionHandler {
    type Translator = FunctionTranslator;

    fn new_translator() -> Self::Translator {
        FunctionTranslator
    }
}

impl HasTranslator for CustomHandler {
    type Translator = CustomTranslator;

    fn new_translator() -> Self::Translator {
        CustomTranslator::default()
    }
}

impl HasTranslator for ToolSearchHandler {
    type Translator = ToolSearchTranslator;

    fn new_translator() -> Self::Translator {
        ToolSearchTranslator::default()
    }
}

impl HasTranslator for CodexNamespaceHandler {
    type Translator = CodexNamespaceTranslator;

    fn new_translator() -> Self::Translator {
        CodexNamespaceTranslator
    }
}

impl HasTranslator for crate::tool::ShellHandler {
    type Translator = super::shell::ShellTranslator;

    fn new_translator() -> Self::Translator {
        super::shell::ShellTranslator::default()
    }
}
