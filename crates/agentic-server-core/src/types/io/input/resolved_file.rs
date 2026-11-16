//! Content resolved from the Files API for one model-facing request.
//!
//! Rehydration reads the referenced files and builds a [`ResolvedFiles`] map; only
//! [`super::ResponsesInput::normalized_model_input`] uses it, to replace a client's `file_id`
//! with inline content in the model copy. Stored history and `enriched_request.input` keep the
//! client's reference.
//!
//! Stored files do not carry a client-visible content type, so both the text rules and the
//! image data URL use the uploaded filename, as `OpenAI` does.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::content::{InputFileContent, InputImageContent, InputTextContent};
use super::file_text::{InputFileError, kinds, text_content_from_bytes};

/// The stored content behind one message `file_id` reference.
#[derive(Debug, Clone)]
pub struct ResolvedFile {
    pub(crate) filename: String,
    pub(crate) bytes: Vec<u8>,
}

impl ResolvedFile {
    /// The stored bytes and filename behind a `file_id`.
    #[must_use]
    pub fn new(filename: String, bytes: Vec<u8>) -> Self {
        Self { filename, bytes }
    }

    /// The filename extension without its dot, preserving its case.
    fn extension(&self) -> Option<&str> {
        file_extension(&self.filename)
    }

    pub(crate) fn is_image(&self) -> bool {
        self.image_media_type().is_some()
    }

    /// The media type for an accepted image filename.
    pub(crate) fn image_media_type(&self) -> Option<&'static str> {
        match self.extension()?.to_ascii_lowercase().as_str() {
            "jpeg" | "jpg" => Some("image/jpeg"),
            "png" => Some("image/png"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            _ => None,
        }
    }

    /// The base64 data URL used for an `input_image` reference.
    pub(crate) fn data_url(&self) -> Option<String> {
        let media_type = self.image_media_type()?;
        let mut data_url = String::with_capacity(self.data_url_len()?);
        data_url.push_str("data:");
        data_url.push_str(media_type);
        data_url.push_str(";base64,");
        STANDARD.encode_string(&self.bytes, &mut data_url);
        Some(data_url)
    }

    /// The encoded length an equivalent inline `image_url` would add to a request.
    pub(crate) fn data_url_len(&self) -> Option<usize> {
        let media_type = self.image_media_type()?;
        Some("data:".len() + media_type.len() + ";base64,".len() + self.bytes.len().div_ceil(3) * 4)
    }

    /// The bytes an equivalent inline `input_text` part adds to the serialized request body.
    ///
    /// This counts the `text` value's JSON-escaped length, including the `File: <filename>`
    /// prefix, not the raw stored byte count: quotes, backslashes, and control characters
    /// grow when serialized. `None` means the stored bytes are not UTF-8, which cannot be
    /// lowered to text and is rejected before inference.
    pub(crate) fn text_json_len(&self) -> Option<usize> {
        let text = std::str::from_utf8(&self.bytes).ok()?;
        let prefix = format!("File: {}\n", self.filename);
        let prefix_len = serde_json::to_string(&prefix).ok()?.len().checked_sub(2)?;
        let text_len = serde_json::to_string(text).ok()?.len().checked_sub(2)?;
        prefix_len.checked_add(text_len)
    }

    /// Builds the model-facing text part for an `input_file` reference.
    ///
    /// The client's filename wins over the stored name; a reference cannot carry one, but
    /// conversion stays the same code that inline `file_data` uses. The caller keeps the
    /// client-facing part unchanged.
    pub(crate) fn to_input_text(&self, file: &InputFileContent) -> Result<InputTextContent, InputFileError> {
        let filename = file.filename.as_deref().unwrap_or(&self.filename);
        if !is_text_filename(filename) {
            return Err(InputFileError::NotText);
        }
        text_content_from_bytes(Some(filename), &self.bytes, &file.extra)
    }

    /// Builds the model-facing image part for an `input_image` reference.
    pub(crate) fn to_input_image(&self, image: &mut InputImageContent) -> Result<(), InputFileError> {
        image.image_url = Some(self.data_url().ok_or(InputFileError::NotImage)?);
        image.file_id = None;
        Ok(())
    }
}

/// Files resolved for one request, keyed by the client's `file_id`.
pub type ResolvedFiles = HashMap<String, Arc<ResolvedFile>>;

/// The filename extension without its dot, unless the basename is a dotfile or has none.
pub(crate) fn file_extension(filename: &str) -> Option<&str> {
    let basename = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    basename
        .rsplit_once('.')
        .filter(|(stem, extension)| !stem.is_empty() && !extension.is_empty())
        .map(|(_, extension)| extension)
}

/// Whether a filename names a text file by its extension or a known text basename.
pub(crate) fn is_text_filename(filename: &str) -> bool {
    kinds::is_text_file("", Some(filename))
}

/// The extension shown in a wrong-file-type error, including a leading dot.
pub(crate) fn extension_label(filename: &str) -> String {
    format!(".{}", file_extension(filename).unwrap_or_default())
}

/// The formats `OpenAI` lists for context stuffing, used verbatim in a wrong-file-type error.
const CONTEXT_STUFFING_EXTENSIONS: &str = ".art, .bat, .brf, .c, .cls, .css, .csv, .diff, .doc, .docx, .dot, .eml, \
     .epub, .es, .h, .hs, .htm, .html, .hwp, .hwpx, .ics, .ifb, .java, .js, .json, .keynote, .ksh, .ltx, .mail, \
     .markdown, .md, .mht, .mhtml, .mjs, .msg, .nws, .odp, .ods, .odt, .otp, .ots, .pages, .patch, .pdf, .pl, .pm, \
     .pot, .potm, .potx, .ppa, .pps, .ppsm, .ppsx, .ppt, .pptm, .pptx, .pwz, .py, .rst, .rtf, .scala, .sh, .shtml, \
     .srt, .sty, .svg, .svgz, .tex, .text, .txt, .tsv, .vcf, .vtt, .wiz, .xla, .xlb, .xlc, .xlm, .xls, .xlsx, .xlt, \
     .xlw, .xml, .yaml, .yml";

/// The supported text formats, as `OpenAI` lists its context stuffing formats.
pub(crate) fn context_stuffing_extensions() -> &'static str {
    CONTEXT_STUFFING_EXTENSIONS
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn image_media_types_follow_the_accepted_extensions() {
        for (filename, media_type) in [
            ("plot.jpeg", "image/jpeg"),
            ("plot.JPG", "image/jpeg"),
            ("plot.png", "image/png"),
            ("plot.gif", "image/gif"),
            ("plot.webp", "image/webp"),
        ] {
            let file = ResolvedFile::new(filename.to_owned(), b"image".to_vec());
            assert_eq!(file.image_media_type(), Some(media_type), "{filename}");
            assert!(file.is_image());
            assert_eq!(
                file.data_url().unwrap(),
                format!("data:{media_type};base64,{}", STANDARD.encode(b"image"))
            );
            assert_eq!(file.data_url_len(), Some(file.data_url().unwrap().len()));
        }
        for filename in ["plot.svg", "plot.bin", "plot", ".png"] {
            let file = ResolvedFile::new(filename.to_owned(), b"image".to_vec());
            assert_eq!(file.image_media_type(), None, "{filename}");
            assert!(!file.is_image());
        }
    }

    #[test]
    fn resolved_text_uses_the_client_filename_and_keeps_the_cache_boundary() {
        let resolved = ResolvedFile::new("notes.txt".into(), b"hello".to_vec());
        let mut file = InputFileContent {
            file_id: Some("file-1".into()),
            ..InputFileContent::default()
        };
        file.extra
            .insert("prompt_cache_breakpoint".into(), json!({"mode": "explicit"}));

        let text = resolved.to_input_text(&file).unwrap();
        assert_eq!(text.text, "File: notes.txt\nhello");
        assert_eq!(
            text.extra.get("prompt_cache_breakpoint"),
            Some(&json!({"mode": "explicit"}))
        );
    }

    #[test]
    fn text_json_len_counts_the_prefix_and_json_escaping() {
        let bytes = b"quote \" backslash \\ newline \n tab \t".to_vec();
        let resolved = ResolvedFile::new("notes.txt".into(), bytes.clone());
        let text = format!("File: notes.txt\n{}", String::from_utf8(bytes).unwrap());
        let expected = serde_json::to_string(&text).unwrap().len() - 2;
        assert_eq!(resolved.text_json_len(), Some(expected));
        assert!(expected > text.len(), "escaped control characters grow the size");

        let binary = ResolvedFile::new("notes.txt".into(), vec![0xff]);
        assert_eq!(binary.text_json_len(), None);
    }

    #[test]
    fn resolved_text_rejects_non_text_and_non_utf8_content() {
        let file = InputFileContent {
            file_id: Some("file-1".into()),
            ..InputFileContent::default()
        };
        let pdf = ResolvedFile::new("report.pdf".into(), b"%PDF".to_vec());
        assert_eq!(pdf.to_input_text(&file), Err(InputFileError::NotText));
        let binary = ResolvedFile::new("notes.txt".into(), vec![0xff]);
        assert_eq!(binary.to_input_text(&file), Err(InputFileError::NotUtf8));
    }

    #[test]
    fn resolved_image_replaces_the_reference_and_keeps_other_fields() {
        let resolved = ResolvedFile::new("plot.png".into(), b"png".to_vec());
        let mut image = InputImageContent {
            file_id: Some("file-1".into()),
            detail: Some("high".into()),
            extra: json!({"extra": "keep"}).as_object().cloned().unwrap(),
            ..InputImageContent::default()
        };
        resolved.to_input_image(&mut image).unwrap();
        let expected = format!("data:image/png;base64,{}", STANDARD.encode(b"png"));
        assert_eq!(image.image_url.as_deref(), Some(expected.as_str()));
        assert!(image.file_id.is_none());
        assert_eq!(image.detail.as_deref(), Some("high"));
        assert_eq!(image.extra.get("extra"), Some(&json!("keep")));
    }
}
