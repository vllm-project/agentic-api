//! Test-only capture of `tracing` event fields, without a subscriber dependency.

use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// Records every event's fields as `name=value` text while its guard is installed.
#[derive(Clone, Default)]
pub(crate) struct LogCapture(Arc<Mutex<String>>);

impl LogCapture {
    /// Capture this thread's events, including tasks on a current-thread runtime.
    pub(crate) fn install(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(self.clone())
    }

    pub(crate) fn text(&self) -> String {
        self.0.lock().unwrap().clone()
    }
}

struct Fields<'a>(&'a mut String);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        write!(self.0, " {}={value:?}", field.name()).unwrap();
    }
}

impl Subscriber for LogCapture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut text = self.0.lock().unwrap();
        event.record(&mut Fields(&mut text));
        text.push('\n');
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}
