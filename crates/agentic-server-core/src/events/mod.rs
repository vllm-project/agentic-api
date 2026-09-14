pub mod normalize;
mod sse;
pub mod types;
mod validate;

pub(crate) use normalize::normalize_sse_data_checked;
pub use normalize::normalize_sse_line;
pub use sse::{ClassifiedSseLine, SseLine};
pub use types::{EventFrame, EventPayload, SSEEventType, SSEItemType, WireEvent};
pub(crate) use validate::{
    ValidatedFrame, ensure_supported_output_item_type, expected_item_type, output_item_identity, validate_frame,
};
