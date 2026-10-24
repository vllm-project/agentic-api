//! Inline UTF-8 text files carried by message `input_file` parts.
//!
//! A message file is accepted only when its `file_data` is base64 (a data URL
//! or raw) and the file is identified as text: either its media type is text
//! (`text/*`, a reviewed list of text `application/*` types, or any `+json` or
//! `+xml` type), or its `filename` has a known text extension or name such as
//! `Dockerfile` or `.gitignore`. The filename decides for raw base64 and for
//! any media type except PDF, image, audio, and video; `video/mp2t` with a
//! `.ts`, `.mts`, or `.cts` name is TypeScript. Strict UTF-8 decoding rejects
//! binary content that passes these checks. The client's `input_file` is what
//! the gateway stores; only the model-facing copy replaces it with the
//! `input_text` part produced by `InputTextContent::try_from(&InputFileContent)`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use thiserror::Error;

mod kinds;

use super::{InputFileContent, InputTextContent};

/// Why a message `input_file` cannot be converted to model-facing text.
///
/// Messages never echo the file data, URL, name, or media type.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum InputFileError {
    #[error(
        "input_file with file_id is not supported: this gateway has no Files API; send the file inline as \
         file_data (a base64 UTF-8 text file) or as input_text"
    )]
    FileId,
    #[error(
        "input_file with file_url is not supported: the gateway does not fetch files; send the file inline as \
         file_data (a base64 UTF-8 text file) or as input_text"
    )]
    FileUrl,
    #[error("input_file without file_data is not supported; send the file inline as file_data")]
    MissingData,
    #[error(
        "input_file content is not supported: only UTF-8 text files can be inlined, identified by a text media \
         type (text/*, application/json, application/xml, application/yaml, any +json or +xml type, or another \
         text type such as application/javascript) or, for raw base64 or a generic media type, by a filename with \
         a text extension; extract the text client-side and send it as input_text, or send pages as input_image"
    )]
    NotText,
    #[error("input_file file_data is not supported: data URLs must be base64-encoded (data:<media type>;base64,...)")]
    NotBase64DataUrl,
    #[error("input_file file_data is not supported: it is not valid base64")]
    InvalidBase64,
    #[error("input_file file_data is not supported: the decoded file is not valid UTF-8 text")]
    NotUtf8,
}

/// Split a base64 data URL into its media type and payload.
fn parse_data_url(url: &str) -> Result<(&str, &str), InputFileError> {
    let (header, payload) = url.split_once(',').ok_or(InputFileError::NotBase64DataUrl)?;
    // `data:<media type>[;<parameter>]*;base64`: parameters such as `charset` are ignored.
    let (media_type, encoding) = header.rsplit_once(';').ok_or(InputFileError::NotBase64DataUrl)?;
    if !encoding.eq_ignore_ascii_case("base64") {
        return Err(InputFileError::NotBase64DataUrl);
    }
    let media_type = media_type
        .split_once(';')
        .map_or(media_type, |(media_type, _)| media_type);
    Ok((media_type.trim(), payload))
}

impl InputFileContent {
    /// Check the reference fields and media type without decoding, returning the base64 payload.
    fn text_payload(&self) -> Result<&str, InputFileError> {
        if self.file_id.is_some() {
            return Err(InputFileError::FileId);
        }
        if self.file_url.is_some() {
            return Err(InputFileError::FileUrl);
        }
        let data = self.file_data.as_deref().ok_or(InputFileError::MissingData)?;
        let (media_type, payload) = match data.get(..5) {
            Some(scheme) if scheme.eq_ignore_ascii_case("data:") => parse_data_url(&data[5..])?,
            _ => ("", data),
        };
        if kinds::is_text_file(media_type, self.filename.as_deref()) {
            Ok(payload)
        } else {
            Err(InputFileError::NotText)
        }
    }

    fn decoded_bytes(&self) -> Result<Vec<u8>, InputFileError> {
        STANDARD
            .decode(self.text_payload()?)
            .map_err(|_| InputFileError::InvalidBase64)
    }

    /// Check that the file can be sent to the model as text, without building the model-facing text.
    ///
    /// # Errors
    /// Returns why the file cannot be inlined: a file reference, missing data,
    /// a non-text media type, or data that is not base64-encoded UTF-8.
    pub(super) fn validate(&self) -> Result<(), InputFileError> {
        std::str::from_utf8(&self.decoded_bytes()?)
            .map(drop)
            .map_err(|_| InputFileError::NotUtf8)
    }
}

/// The model-facing text of an inline text file, prefixed with
/// `File: <filename>` when a filename is set.
impl TryFrom<&InputFileContent> for InputTextContent {
    type Error = InputFileError;

    fn try_from(file: &InputFileContent) -> Result<Self, Self::Error> {
        let text = String::from_utf8(file.decoded_bytes()?).map_err(|_| InputFileError::NotUtf8)?;
        let mut content = Self::new(match file.filename.as_deref() {
            Some(filename) => format!("File: {filename}\n{text}"),
            None => text,
        });
        // A cache boundary belongs to this content position after text inlining, too.
        // Other file extensions may not apply to text (or may collide with `text`).
        if let Some(breakpoint) = file.extra.get("prompt_cache_breakpoint") {
            content
                .extra
                .insert("prompt_cache_breakpoint".to_owned(), breakpoint.clone());
        }
        Ok(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn file(fields: serde_json::Value) -> InputFileContent {
        serde_json::from_value(fields).unwrap()
    }

    fn data(data: &str, filename: Option<&str>) -> InputFileContent {
        InputFileContent {
            file_data: Some(data.to_owned()),
            filename: filename.map(str::to_owned),
            ..InputFileContent::default()
        }
    }

    /// Convert like normalization does, checking that validation agrees.
    fn text(file: &InputFileContent) -> Result<String, InputFileError> {
        let converted = InputTextContent::try_from(file).map(|part| part.text);
        assert_eq!(file.validate(), converted.as_ref().map(drop).map_err(Clone::clone));
        converted
    }

    fn encode(text: &[u8]) -> String {
        STANDARD.encode(text)
    }

    #[test]
    fn accepts_text_media_types_with_and_without_filename() {
        for media_type in [
            "text/plain",
            "text/markdown",
            "TEXT/CSV",
            "application/json",
            "application/xml",
        ] {
            let url = format!("data:{media_type};base64,{}", encode(b"a: 1"));
            assert_eq!(text(&data(&url, None)).unwrap(), "a: 1", "{media_type}");
        }
        let url = format!("data:application/yaml;base64,{}", encode(b"a: 1"));
        assert_eq!(
            text(&data(&url, Some("config.yaml"))).unwrap(),
            "File: config.yaml\na: 1"
        );
    }

    #[test]
    fn ignores_media_type_parameters() {
        let url = format!("data:text/plain;charset=utf-8;base64,{}", encode("héllo".as_bytes()));
        assert_eq!(text(&data(&url, Some("notes.txt"))).unwrap(), "File: notes.txt\nhéllo");
    }

    #[test]
    fn generic_or_missing_media_type_falls_back_to_the_filename() {
        let payload = encode(b"# Notes");
        let octet = format!("data:application/octet-stream;base64,{payload}");
        assert_eq!(
            text(&data(&octet, Some("notes.md"))).unwrap(),
            "File: notes.md\n# Notes"
        );
        assert_eq!(
            text(&data(&payload, Some("config.YML"))).unwrap(),
            "File: config.YML\n# Notes"
        );
        assert_eq!(
            text(&data(&payload, Some("config.yaml"))).unwrap(),
            "File: config.yaml\n# Notes"
        );
        assert_eq!(text(&data(&payload, None)), Err(InputFileError::NotText));
        assert_eq!(text(&data(&octet, Some("archive.zip"))), Err(InputFileError::NotText));
        let empty_type = format!("data:;base64,{payload}");
        assert_eq!(text(&data(&empty_type, Some("a.txt"))).unwrap(), "File: a.txt\n# Notes");
    }

    #[test]
    fn accepts_common_source_and_config_files_by_media_type() {
        for (media_type, filename) in [
            ("application/javascript", "app.js"),
            ("application/x-sh", "run.sh"),
            ("application/toml", "Cargo.toml"),
            ("application/sql", "schema.sql"),
            ("application/ld+json", "context.jsonld"),
            ("application/typescript", "index.ts"),
            ("video/mp2t", "index.ts"),
        ] {
            let url = format!("data:{media_type};base64,{}", encode(b"x = 1"));
            assert_eq!(
                text(&data(&url, Some(filename))).unwrap(),
                format!("File: {filename}\nx = 1"),
                "{media_type}"
            );
        }
    }

    #[test]
    fn raw_base64_is_accepted_for_text_filenames_only() {
        let payload = encode(b"FROM scratch");
        for filename in ["Dockerfile", "main.rb", "A.cs"] {
            assert_eq!(
                text(&data(&payload, Some(filename))).unwrap(),
                format!("File: {filename}\nFROM scratch")
            );
        }
        assert_eq!(text(&data(&payload, Some("blob"))), Err(InputFileError::NotText));
    }

    #[test]
    fn binary_files_stay_rejected_despite_text_filenames() {
        for media_type in ["application/pdf", "image/png"] {
            let url = format!("data:{media_type};base64,{}", encode(b"text"));
            assert_eq!(
                text(&data(&url, Some("a.txt"))),
                Err(InputFileError::NotText),
                "{media_type}"
            );
        }
        let zip = format!(
            "data:application/zip;base64,{}",
            encode(&[0x50, 0x4b, 0x03, 0x04, 0xff, 0x00])
        );
        assert_eq!(text(&data(&zip, Some("a.txt"))), Err(InputFileError::NotUtf8));
    }

    #[test]
    fn not_text_error_explains_the_filename_rule() {
        let message = InputFileError::NotText.to_string();
        assert!(message.contains("only UTF-8 text files"), "{message}");
        assert!(
            message.contains("for raw base64 or a generic media type, by a filename with a text extension"),
            "{message}"
        );
        assert!(message.contains("+json or +xml"), "{message}");
    }

    #[test]
    fn rejects_pdf_by_media_type_or_filename() {
        let pdf = format!("data:application/pdf;base64,{}", encode(b"%PDF-1.7"));
        // An explicit media type wins over a text filename.
        assert_eq!(text(&data(&pdf, Some("a.txt"))), Err(InputFileError::NotText));
        assert_eq!(
            text(&data(&encode(b"%PDF-1.7"), Some("report.pdf"))),
            Err(InputFileError::NotText)
        );
    }

    #[test]
    fn rejects_references_and_missing_data() {
        let reference = file(json!({"file_id":"file_1", "file_data":"aGk=", "filename":"a.txt"}));
        assert_eq!(text(&reference), Err(InputFileError::FileId));
        let url = file(json!({"file_url":"https://example.invalid/a.txt"}));
        assert_eq!(text(&url), Err(InputFileError::FileUrl));
        assert_eq!(
            text(&file(json!({"filename":"a.txt"}))),
            Err(InputFileError::MissingData)
        );
    }

    #[test]
    fn rejects_undecodable_data() {
        assert_eq!(
            text(&data("data:text/plain,hello", None)),
            Err(InputFileError::NotBase64DataUrl)
        );
        assert_eq!(
            text(&data("data:text/plain", None)),
            Err(InputFileError::NotBase64DataUrl)
        );
        assert_eq!(
            text(&data("data:text/plain;base64,not base64!", None)),
            Err(InputFileError::InvalidBase64)
        );
        let latin1 = format!("data:text/plain;base64,{}", encode(&[0x68, 0xe9, 0x6c]));
        assert_eq!(text(&data(&latin1, None)), Err(InputFileError::NotUtf8));
    }

    #[test]
    fn inlining_preserves_cache_boundary_without_copying_file_only_extensions() {
        for boundary in [None, Some(Value::Null), Some(json!({"mode":"explicit"}))] {
            let mut fields = json!({"filename":"notes.txt", "file_data":"data:text/plain;base64,aGk=",
                "text":"must not override decoded content", "file_extension":{"provider":"opaque"}});
            if let Some(boundary) = &boundary {
                fields["prompt_cache_breakpoint"] = boundary.clone();
            }
            let file = file(fields);
            let original = serde_json::to_value(&file).unwrap();
            let converted = InputTextContent::try_from(&file).unwrap();
            assert_eq!(converted.text, "File: notes.txt\nhi");
            assert_eq!(converted.extra.get("prompt_cache_breakpoint"), boundary.as_ref());
            assert_eq!(converted.extra.len(), usize::from(boundary.is_some()));
            assert_eq!(serde_json::to_value(&file).unwrap(), original);
        }
    }

    #[test]
    fn accepts_empty_and_multibyte_text() {
        assert_eq!(
            text(&data("data:text/plain;base64,", Some("empty.txt"))).unwrap(),
            "File: empty.txt\n"
        );
        let url = format!("data:text/plain;base64,{}", encode("çğış 日本 🙂".as_bytes()));
        assert_eq!(text(&data(&url, None)).unwrap(), "çğış 日本 🙂");
    }
}
