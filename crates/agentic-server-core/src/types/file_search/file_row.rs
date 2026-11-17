//! Files API row metadata: upload purposes and the parameter a row error names.

/// Upload purposes the Files API accepts, in the order `OpenAI` reports them.
pub const FILE_PURPOSES: [&str; 6] = ["fine-tune", "assistants", "batch", "user_data", "vision", "evals"];

/// The request parameter a Files API row error names, as the recorded rows report it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileRowParam {
    Id,
    Purpose,
    After,
    Before,
}

impl FileRowParam {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Purpose => "purpose",
            Self::After => "after",
            Self::Before => "before",
        }
    }
}
