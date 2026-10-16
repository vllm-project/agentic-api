//! Supported native tool versions and their declared capabilities.
//! Classification and request validation share these exact version lists.

pub const NATIVE_WEB_SEARCH_TYPE: &str = "web_search_20250305";
pub const NATIVE_WEB_FETCH_TYPE: &str = "web_fetch_20250910";

/// One version of a native server tool that the Messages gateway executes, and
/// what it defines beyond its tool's basic version, as Anthropic documents
/// each version. The Messages adapter (`executor::messages_request`) accepts a
/// setting only on a version that defines it and rewrites every listed version
/// into the basic version's function tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct NativeToolVersion {
    /// The declared `type`, such as `web_search_20260209`.
    pub type_name: &'static str,
    /// Dynamic filtering: the version's default caller is code execution
    /// rather than the model, so a declaration runs here only when its
    /// `allowed_callers` includes `"direct"`.
    pub dynamic_filtering: bool,
    /// Whether the version defines `use_cache`.
    pub use_cache: bool,
    /// Whether the version defines `response_inclusion`.
    pub response_inclusion: bool,
}

impl NativeToolVersion {
    /// The version in `versions` that a declared tool `type` names, if any.
    #[must_use]
    pub fn find(versions: &[Self], tool_type: Option<&str>) -> Option<Self> {
        versions
            .iter()
            .copied()
            .find(|version| Some(version.type_name) == tool_type)
    }
}

/// Every native web-search version the Messages gateway executes, oldest first.
pub const NATIVE_WEB_SEARCH_VERSIONS: &[NativeToolVersion] = &[
    NativeToolVersion {
        type_name: NATIVE_WEB_SEARCH_TYPE,
        dynamic_filtering: false,
        use_cache: false,
        response_inclusion: false,
    },
    NativeToolVersion {
        type_name: "web_search_20260209",
        dynamic_filtering: true,
        use_cache: false,
        response_inclusion: false,
    },
    NativeToolVersion {
        type_name: "web_search_20260318",
        dynamic_filtering: true,
        use_cache: false,
        response_inclusion: true,
    },
];

/// Every native web-fetch version the Messages gateway executes, oldest first.
pub const NATIVE_WEB_FETCH_VERSIONS: &[NativeToolVersion] = &[
    NativeToolVersion {
        type_name: NATIVE_WEB_FETCH_TYPE,
        dynamic_filtering: false,
        use_cache: false,
        response_inclusion: false,
    },
    NativeToolVersion {
        type_name: "web_fetch_20260209",
        dynamic_filtering: true,
        use_cache: false,
        response_inclusion: false,
    },
    NativeToolVersion {
        type_name: "web_fetch_20260309",
        dynamic_filtering: true,
        use_cache: true,
        response_inclusion: false,
    },
    NativeToolVersion {
        type_name: "web_fetch_20260318",
        dynamic_filtering: true,
        use_cache: true,
        response_inclusion: true,
    },
];
