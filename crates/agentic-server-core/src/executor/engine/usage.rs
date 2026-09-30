//! Usage accumulation across inference rounds.

use crate::types::io::ResponseUsage;

pub(super) fn add_usage(total: ResponseUsage, usage: ResponseUsage) -> ResponseUsage {
    total.saturating_add(usage)
}

pub(super) fn accumulate_usage(total: &mut Option<ResponseUsage>, usage: Option<ResponseUsage>) {
    if let Some(usage) = usage {
        *total = Some(total.map_or(usage, |current| add_usage(current, usage)));
    }
}
