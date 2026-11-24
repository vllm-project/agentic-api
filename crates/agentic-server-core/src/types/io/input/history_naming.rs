//! Tool names resolved from a request's declarations and completed search history during upstream conversion.

/// Naming context built inside the request's upstream conversion.
#[derive(Clone, Default)]
pub(crate) struct HistoryNaming {
    /// Top-level functions declared or loaded by a completed search.
    pub(crate) functions: Vec<String>,
}
