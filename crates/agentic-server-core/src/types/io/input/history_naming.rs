//! How the model knew the tools named in a request's history, beyond the tools the request declares.
//!
//! A compaction summary declares no tools, so it carries its source request's naming as context only: nothing in it
//! is callable, and the upstream projection reads it the way it reads the request's own declarations.

/// Naming context a request's history needs from elsewhere.
#[derive(Clone, Default)]
pub(crate) struct HistoryNaming {
    /// Top-level functions the source request knew: declared, restored after compaction, or loaded by a search.
    pub(crate) functions: Vec<String>,
}
