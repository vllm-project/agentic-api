//! Shared SSE field classification for Responses and Messages ingestion.

/// An owned SSE data payload, with the field prefix removed.
#[derive(PartialEq, Eq)]
pub struct SseLine(String);

impl std::fmt::Debug for SseLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseLine")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}

/// Field-level classification; JSON and semantic validation happen downstream.
#[derive(Debug, PartialEq, Eq)]
pub enum ClassifiedSseLine {
    Data(SseLine),
    Done,
    Ignore,
}

impl SseLine {
    /// Classifies one framed line, accepting the optional space after `data:`.
    /// Malformed JSON remains data so validation policy can decide its disposition.
    #[must_use]
    pub fn parse(raw: &str) -> ClassifiedSseLine {
        let Some(data) = raw.strip_prefix("data:") else {
            return ClassifiedSseLine::Ignore;
        };
        let data = data.strip_prefix(' ').unwrap_or(data);
        match data.trim() {
            "" => ClassifiedSseLine::Ignore,
            "[DONE]" => ClassifiedSseLine::Done,
            _ => ClassifiedSseLine::Data(Self(data.to_owned())),
        }
    }

    /// Returns the data payload without trimming its contents.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{ClassifiedSseLine, SseLine};

    #[test]
    fn sse_line_accepts_optional_space_without_parsing_json() {
        for raw in ["data:{", "data: {"] {
            let ClassifiedSseLine::Data(line) = SseLine::parse(raw) else {
                panic!("malformed JSON must remain a data payload");
            };
            assert_eq!(line.as_str(), "{");
        }
    }

    #[test]
    fn sse_line_distinguishes_done_from_ignored_fields() {
        for raw in ["data:[DONE]", "data: [DONE]", "data: [DONE]\r\n"] {
            assert_eq!(SseLine::parse(raw), ClassifiedSseLine::Done);
        }
        for raw in ["", ": heartbeat", "event: response.completed", "data:", "data: \t"] {
            assert_eq!(SseLine::parse(raw), ClassifiedSseLine::Ignore);
        }
    }

    #[test]
    fn sse_line_preserves_data_whitespace() {
        let ClassifiedSseLine::Data(line) = SseLine::parse("data:  {\"text\":\"  hello  \"} ") else {
            panic!("expected a data payload");
        };
        assert_eq!(line.as_str(), " {\"text\":\"  hello  \"} ");
    }
}
