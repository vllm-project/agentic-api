//! Text/PDF extraction and UTF-8-safe overlapping `cl100k_base` token windows.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::types::file_search::{ChunkingStrategy, FileSearchError, StaticChunking, invalid};

const MAX_EXTRACTED_BYTES: usize = 16 * 1024 * 1024;
const MAX_CHUNKS: usize = 2048;
// BPE can take quadratic time on a single long lexical run. Small UTF-8-safe
// blocks bound each call, including when the input contains no whitespace.
const TOKENIZATION_BLOCK_BYTES: usize = 256;

pub(super) fn chunking_config(strategy: &ChunkingStrategy) -> Result<StaticChunking, FileSearchError> {
    let config = match strategy {
        ChunkingStrategy::Contextual { contextual } => {
            contextual.validate()?;
            return Ok(StaticChunking {
                max_chunk_size_tokens: contextual.max_chunk_size_tokens,
                chunk_overlap_tokens: contextual.chunk_overlap_tokens,
            });
        }
        ChunkingStrategy::Auto => StaticChunking::default(),
        ChunkingStrategy::Static { config } => config.clone(),
    };
    if !(100..=4096).contains(&config.max_chunk_size_tokens)
        || config.chunk_overlap_tokens > config.max_chunk_size_tokens / 2
    {
        return invalid(
            "max_chunk_size_tokens must be 100 to 4096 and chunk_overlap_tokens must be 0 to half the chunk size",
        );
    }
    Ok(config)
}

fn is_pdf(filename: &str, content_type: &str) -> bool {
    content_type == "application/pdf"
        || (matches!(content_type, "application/octet-stream" | "") && filename.to_lowercase().ends_with(".pdf"))
}

pub(super) fn validate_content_type(filename: &str, content_type: &str) -> Result<(), FileSearchError> {
    let content_type = content_type.split(';').next().unwrap_or_default().trim();
    if content_type.len() > 256 {
        return invalid("content type is too long");
    }
    let extension = filename.rsplit('.').next().unwrap_or_default().to_lowercase();
    if is_pdf(filename, content_type) {
        #[cfg(feature = "file-search-pdf")]
        return Ok(());
        #[cfg(not(feature = "file-search-pdf"))]
        return invalid(
            "PDF ingestion requires the file-search-pdf build feature (Rust 1.88 or newer); upload UTF-8 text instead",
        );
    }
    if content_type.starts_with("text/")
        || matches!(
            content_type,
            "application/json" | "application/xml" | "application/yaml" | "application/x-yaml" | "application/x-ndjson"
        )
        || (matches!(content_type, "application/octet-stream" | "")
            && matches!(
                extension.as_str(),
                "txt"
                    | "md"
                    | "markdown"
                    | "csv"
                    | "tsv"
                    | "json"
                    | "jsonl"
                    | "xml"
                    | "yaml"
                    | "yml"
                    | "html"
                    | "htm"
                    | "rs"
                    | "py"
                    | "js"
                    | "ts"
                    | "css"
                    | "log"
            ))
    {
        return Ok(());
    }
    invalid("Unsupported file type; upload UTF-8 text or a PDF containing extractable text")
}

pub(super) struct ExtractedDocument {
    pub text: String,
    pub chunks: Vec<String>,
}

pub(super) fn extract_and_chunk(
    bytes: Vec<u8>,
    filename: &str,
    content_type: &str,
    chunking: &StaticChunking,
    cancelled: &AtomicBool,
) -> Result<ExtractedDocument, FileSearchError> {
    validate_content_type(filename, content_type)?;
    let content_type = content_type.split(';').next().unwrap_or_default().trim();
    let text = if is_pdf(filename, content_type) {
        extract_pdf(&bytes)?
    } else {
        String::from_utf8(bytes)
            .map_err(|_| FileSearchError::InvalidRequest("Text files must use UTF-8 encoding".into()))?
    };
    if text.len() > MAX_EXTRACTED_BYTES {
        return invalid("Extracted text exceeds 16 MiB");
    }
    if text.trim().is_empty() {
        return invalid("The file contains no extractable text; scanned PDFs require OCR before upload");
    }
    if text.contains('\0') {
        return invalid("The file contains binary content instead of text");
    }
    let chunks = chunks(&text, chunking, cancelled)?;
    Ok(ExtractedDocument { text, chunks })
}

#[cfg(not(feature = "file-search-pdf"))]
fn extract_pdf(_bytes: &[u8]) -> Result<String, FileSearchError> {
    invalid("PDF ingestion requires the file-search-pdf build feature (Rust 1.88 or newer); upload UTF-8 text instead")
}

#[cfg(feature = "file-search-pdf")]
fn extract_pdf(bytes: &[u8]) -> Result<String, FileSearchError> {
    // A PDF stream starts with the literal `stream` keyword. Counting its raw
    // occurrences (including conservative false positives such as `endstream`)
    // apportions one expansion budget before eager object/xref stream decoding.
    let possible_streams = bytes.windows(6).filter(|window| *window == b"stream").count().max(1);
    let eager_stream_limit = (32 * 1024 * 1024 / possible_streams).min(MAX_EXTRACTED_BYTES);
    let options = lopdf::LoadOptions {
        strict: true,
        max_decompressed_size: Some(eager_stream_limit),
        ..lopdf::LoadOptions::default()
    };
    let document = lopdf::Document::load_mem_with_options(bytes, options).map_err(FileSearchError::PdfParse)?;
    if document.is_encrypted() || document.was_encrypted() {
        return invalid("Encrypted PDFs are not supported; upload an unencrypted PDF");
    }
    let pages = document.get_pages();
    if document.objects.len() > 100_000 || pages.len() > 1024 {
        return invalid("PDF exceeds 1024 pages or 100000 objects");
    }
    // Preflight streams used for text against one shared expansion budget. This
    // includes font/CMap streams; image streams are never decoded for text.
    let mut remaining = 32 * 1024 * 1024;
    for object in document.objects.values() {
        if let lopdf::Object::Stream(stream) = object {
            if stream
                .dict
                .get(b"Subtype")
                .and_then(lopdf::Object::as_name)
                .is_ok_and(|subtype| subtype == b"Image")
            {
                continue;
            }
            let decoded = stream
                .decompressed_content_with_limit(remaining)
                .map_err(FileSearchError::PdfParse)?;
            remaining = remaining
                .checked_sub(decoded.len())
                .ok_or_else(|| FileSearchError::InvalidRequest("PDF stream contents exceed 32 MiB".into()))?;
        }
    }
    let mut text = String::new();
    for page in pages.keys() {
        let extracted = document
            .extract_text_with_limit(&[*page], MAX_EXTRACTED_BYTES)
            .map_err(FileSearchError::PdfParse)?;
        if text.len().saturating_add(extracted.len()).saturating_add(1) > MAX_EXTRACTED_BYTES {
            return invalid("Extracted text exceeds 16 MiB");
        }
        text.push_str(&extracted);
        text.push('\n');
    }
    Ok(text)
}

fn chunks(text: &str, config: &StaticChunking, cancelled: &AtomicBool) -> Result<Vec<String>, FileSearchError> {
    let tokenizer = tiktoken_rs::cl100k_base_singleton();
    let max_tokens =
        (MAX_CHUNKS - 1) * (config.max_chunk_size_tokens - config.chunk_overlap_tokens) + config.max_chunk_size_tokens;
    let mut offsets = vec![0];
    let mut offset = 0;
    while offset < text.len() {
        if cancelled.load(Ordering::Relaxed) {
            return Err(FileSearchError::Unavailable("File ingestion was cancelled".into()));
        }
        let mut end = (offset + TOKENIZATION_BLOCK_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let tokens = tokenizer.encode_ordinary(&text[offset..end]);
        if offsets.len() - 1 + tokens.len() > max_tokens {
            return invalid("The file exceeds the maximum of 2048 chunks; use a smaller file or larger chunks");
        }
        for bytes in tokenizer._decode_native_and_split(tokens) {
            offset += bytes.len();
            offsets.push(offset);
        }
    }
    let token_count = offsets.len() - 1;
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < token_count {
        if cancelled.load(Ordering::Relaxed) {
            return Err(FileSearchError::Unavailable("File ingestion was cancelled".into()));
        }
        let mut end = (start + config.max_chunk_size_tokens).min(token_count);
        // A Unicode scalar can span tokens. End before that scalar and include it
        // intact in the next window rather than storing replacement characters.
        while end > start && !text.is_char_boundary(offsets[end]) {
            end -= 1;
        }
        if end == start {
            return invalid("Unable to find a Unicode boundary within the token window");
        }
        chunks.push(text[offsets[start]..offsets[end]].to_owned());
        if chunks.len() > MAX_CHUNKS {
            return invalid("The file exceeds the maximum of 2048 chunks");
        }
        if end == token_count {
            break;
        }
        let mut next = end.saturating_sub(config.chunk_overlap_tokens).max(start + 1);
        while next < end && !text.is_char_boundary(offsets[next]) {
            next += 1;
        }
        start = next;
    }
    Ok(chunks)
}

/// Keep complete source chunks that fit the remaining model-context budget.
pub(super) fn limit_context(
    results: Vec<crate::types::file_search::SearchResult>,
    mut budget: usize,
) -> Vec<crate::types::file_search::SearchResult> {
    let tokenizer = tiktoken_rs::cl100k_base_singleton();
    results
        .into_iter()
        .filter(|result| {
            let mut tokens = 0usize;
            for content in &result.content {
                let mut text = content.text.as_str();
                while !text.is_empty() {
                    let mut end = text.len().min(TOKENIZATION_BLOCK_BYTES);
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    tokens = tokens.saturating_add(tokenizer.encode_ordinary(&text[..end]).len());
                    if tokens > budget {
                        return false;
                    }
                    text = &text[end..];
                }
            }
            budget -= tokens;
            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_tokens_never_split_scalars_or_drop_text() {
        let text = "A coral 🪸 reef conserves biodiversity. 日本語の文章。 café ".repeat(90);
        let config = StaticChunking {
            max_chunk_size_tokens: 100,
            chunk_overlap_tokens: 0,
        };
        let chunks = extract_and_chunk(
            text.as_bytes().to_vec(),
            "unicode.txt",
            "text/plain",
            &config,
            &AtomicBool::new(false),
        )
        .unwrap()
        .chunks;
        assert!(chunks.len() > 1);
        assert_eq!(chunks.concat(), text);
        assert!(chunks.iter().all(|chunk| !chunk.contains('\u{fffd}')));
    }

    #[test]
    fn token_windows_overlap_and_respect_limits() {
        let text = "a b c d e f ".repeat(200);
        let config = StaticChunking {
            max_chunk_size_tokens: 100,
            chunk_overlap_tokens: 25,
        };
        let windows = chunks(&text, &config, &AtomicBool::new(false)).unwrap();
        assert!(windows.len() > 2);
        let tokenizer = tiktoken_rs::cl100k_base_singleton();
        assert!(
            windows
                .iter()
                .all(|chunk| tokenizer.encode_ordinary(chunk).len() <= 100)
        );
        let first = tokenizer.encode_ordinary(&windows[0]);
        let second = tokenizer.encode_ordinary(&windows[1]);
        assert_eq!(&first[75..], &second[..25]);
    }

    #[test]
    fn unsupported_binary_invalid_utf8_and_empty_text_are_rejected() {
        for (bytes, filename, mime) in [
            (vec![0, 1], "a.png", "image/png"),
            (vec![255], "a.txt", "text/plain"),
            (b"   ".to_vec(), "a.txt", "text/plain"),
            (b"broken".to_vec(), "a.pdf", "application/pdf"),
        ] {
            assert!(
                extract_and_chunk(
                    bytes,
                    filename,
                    mime,
                    &StaticChunking::default(),
                    &AtomicBool::new(false)
                )
                .is_err()
            );
        }
    }

    #[test]
    fn cancelled_tokenization_does_not_continue_processing() {
        assert!(chunks("cancelled text", &StaticChunking::default(), &AtomicBool::new(true)).is_err());
    }

    #[test]
    fn long_unbroken_text_is_preserved_and_can_be_cancelled() {
        let text = "a".repeat(128 * 1024);
        let config = StaticChunking {
            max_chunk_size_tokens: 100,
            chunk_overlap_tokens: 0,
        };
        let windows = chunks(&text, &config, &AtomicBool::new(false)).unwrap();
        assert_eq!(windows.concat(), text);
        assert!(windows.iter().all(|window| window.len() <= 800));

        let cancelled = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(5));
                cancelled.store(true, Ordering::Relaxed);
            });
            assert!(chunks(&"a".repeat(2 * 1024 * 1024), &StaticChunking::default(), &cancelled).is_err());
        });
    }
}
