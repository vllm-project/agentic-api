//! Resolve message `file_id` references from the Files API before inference.

use std::collections::HashMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::tool::file_search::FileSearchService;
use crate::types::file_search::FileContent;
use crate::types::io::{InOutItem, InputContent, InputItem, InputMessageContent, ResponsesInput};

/// Maximum resolved media, as base64 data URLs, added to one request.
const MAX_HYDRATED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
struct ResolvedFile {
    filename: String,
    media_type: String,
    data_url: String,
}

impl ResolvedFile {
    /// Builds the data URL for a stored file, or `None` when its content type cannot form one.
    fn new(file: FileContent) -> Option<Self> {
        let media_type = file.content_type.split(';').next().unwrap_or_default().trim();
        if media_type.is_empty()
            || media_type
                .chars()
                .any(|character| character == ',' || character.is_whitespace() || character.is_control())
        {
            return None;
        }
        let mut data_url = String::with_capacity(
            "data:".len() + media_type.len() + ";base64,".len() + file.bytes.len().div_ceil(3) * 4,
        );
        data_url.push_str("data:");
        data_url.push_str(media_type);
        data_url.push_str(";base64,");
        STANDARD.encode_string(&file.bytes, &mut data_url);
        Some(Self {
            filename: file.filename,
            media_type: media_type.to_owned(),
            data_url,
        })
    }

    fn is_image(&self) -> bool {
        self.media_type
            .split_once('/')
            .is_some_and(|(kind, subtype)| kind.eq_ignore_ascii_case("image") && !subtype.is_empty())
    }
}

/// Replace message `input_image` and `input_file` file references with base64 data URLs.
///
/// Canonical input keeps the client's reference; only the model-facing copy is hydrated, so a
/// continuation resolves the reference again. A request without a file store, a missing file, or
/// a reference that is not an image or text file fails explicitly instead of reaching the
/// upstream unresolved.
pub(super) async fn hydrate_file_references(
    input: &mut ResponsesInput,
    file_search: Option<&FileSearchService>,
) -> ExecutorResult<()> {
    hydrate_with_limit(input, file_search, MAX_HYDRATED_BYTES).await
}

async fn hydrate_with_limit(
    input: &mut ResponsesInput,
    file_search: Option<&FileSearchService>,
    max_bytes: usize,
) -> ExecutorResult<()> {
    let ResponsesInput::Items(items) = input else {
        return Ok(());
    };
    let Some(file_search) = file_search else {
        return match unresolved_file_reference(items) {
            Some((item_index, part_index, part)) => Err(part_error(
                item_index,
                part_index,
                &format!(
                    "{part} with file_id requires the gateway file store; configure file storage or send the content inline"
                ),
            )),
            None => Ok(()),
        };
    };

    let mut cache: HashMap<String, ResolvedFile> = HashMap::new();
    let mut hydrated_bytes = 0usize;
    for (item_index, item) in items.iter_mut().enumerate() {
        let InOutItem::Input(InputItem::Message(message)) = item else {
            continue;
        };
        let InputMessageContent::Parts(parts) = &mut message.content else {
            continue;
        };
        for (part_index, part) in parts.iter_mut().enumerate() {
            match part {
                InputContent::InputImage(image) => {
                    let Some(file_id) = image.file_id.as_deref() else {
                        continue;
                    };
                    if image.image_url.is_some() {
                        return Err(part_error(
                            item_index,
                            part_index,
                            "input_image must not combine file_id with image_url",
                        ));
                    }
                    let resolved = resolve_at(file_search, &mut cache, file_id, item_index, part_index).await?;
                    if !resolved.is_image() {
                        return Err(part_error(
                            item_index,
                            part_index,
                            "input_image.file_id must reference a file with an image media type",
                        ));
                    }
                    charge(&mut hydrated_bytes, max_bytes, resolved.data_url.len())?;
                    image.image_url = Some(resolved.data_url);
                    image.file_id = None;
                }
                InputContent::InputFile(file) => {
                    let Some(file_id) = file.file_id.as_deref() else {
                        continue;
                    };
                    if file.file_data.is_some() || file.file_url.is_some() {
                        return Err(part_error(
                            item_index,
                            part_index,
                            "input_file must not combine file_id with file_data or file_url",
                        ));
                    }
                    let resolved = resolve_at(file_search, &mut cache, file_id, item_index, part_index).await?;
                    charge(&mut hydrated_bytes, max_bytes, resolved.data_url.len())?;
                    file.file_data = Some(resolved.data_url);
                    if file.filename.is_none() {
                        file.filename = Some(resolved.filename);
                    }
                    file.file_id = None;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn part_error(item_index: usize, part_index: usize, message: &str) -> ExecutorError {
    ExecutorError::InvalidRequest(format!("input[{item_index}].content[{part_index}]: {message}"))
}

fn unresolved_file_reference(items: &[InOutItem]) -> Option<(usize, usize, &'static str)> {
    items.iter().enumerate().find_map(|(item_index, item)| {
        let InOutItem::Input(InputItem::Message(message)) = item else {
            return None;
        };
        let InputMessageContent::Parts(parts) = &message.content else {
            return None;
        };
        parts.iter().enumerate().find_map(|(part_index, part)| match part {
            InputContent::InputImage(image) if image.file_id.is_some() => Some((item_index, part_index, "input_image")),
            InputContent::InputFile(file) if file.file_id.is_some() => Some((item_index, part_index, "input_file")),
            _ => None,
        })
    })
}

async fn resolve_at(
    file_search: &FileSearchService,
    cache: &mut HashMap<String, ResolvedFile>,
    file_id: &str,
    item_index: usize,
    part_index: usize,
) -> ExecutorResult<ResolvedFile> {
    if let Some(file) = cache.get(file_id) {
        return Ok(file.clone());
    }
    let file = ResolvedFile::new(file_search.read_file(file_id).await?).ok_or_else(|| {
        part_error(
            item_index,
            part_index,
            "the referenced file has an invalid content type; upload it again with a valid media type",
        )
    })?;
    cache.insert(file_id.to_owned(), file.clone());
    Ok(file)
}

fn charge(total: &mut usize, max_bytes: usize, bytes: usize) -> ExecutorResult<()> {
    *total = total.saturating_add(bytes);
    if *total > max_bytes {
        return Err(ExecutorError::PayloadTooLarge(
            "resolved file_id references exceed the per-request hydration limit".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::create_pool_with_schema;
    use crate::types::file_search::FileSearchConfig;
    use serde_json::{Value, json};
    use std::sync::Arc;

    async fn file_search() -> (FileSearchService, tempfile::TempDir) {
        let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
        let files = tempfile::tempdir().unwrap();
        let config = FileSearchConfig {
            files_storage_dir: Some(files.path().to_owned()),
            ..FileSearchConfig::default()
        };
        let service =
            FileSearchService::new(pool, Arc::new(reqwest::Client::new()), config).expect("file search service");
        (service, files)
    }

    fn message(parts: &Value) -> ResponsesInput {
        serde_json::from_value(json!([{"role": "user", "content": parts}])).expect("message input")
    }

    fn invalid_message(error: ExecutorError) -> String {
        match error {
            ExecutorError::InvalidRequest(message) => message,
            other => panic!("expected an invalid request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn inlines_image_and_file_references_and_keeps_part_fields() {
        let (service, _files) = file_search().await;
        let text = service
            .upload_file(
                "notes.txt",
                "text/plain; charset=utf-8",
                "assistants",
                b"hello".to_vec(),
            )
            .await
            .unwrap();
        let image = service
            .upload_file("plot.png", "image/png", "vision", b"png bytes".to_vec())
            .await
            .unwrap();
        let mut input = message(&json!([
            {"type": "input_image", "file_id": &image.id, "detail": "high", "extra": "keep"},
            {"type": "input_file", "file_id": &text.id},
            {"type": "input_file", "file_id": &text.id, "filename": "client.txt"}
        ]));

        hydrate_file_references(&mut input, Some(&service)).await.unwrap();

        let value = serde_json::to_value(&input).unwrap();
        let parts = value[0]["content"].as_array().unwrap();
        assert_eq!(
            parts[0]["image_url"],
            format!("data:image/png;base64,{}", STANDARD.encode(b"png bytes"))
        );
        assert!(parts[0].get("file_id").is_none());
        assert_eq!(parts[0]["detail"], "high");
        assert_eq!(parts[0]["extra"], "keep");
        assert_eq!(
            parts[1]["file_data"],
            format!("data:text/plain;base64,{}", STANDARD.encode(b"hello"))
        );
        assert_eq!(parts[1]["filename"], "notes.txt");
        assert!(parts[1].get("file_id").is_none());
        assert_eq!(parts[2]["filename"], "client.txt");
        input.validate().unwrap();
    }

    #[tokio::test]
    async fn rejects_references_that_also_carry_inline_content() {
        let (service, _files) = file_search().await;
        for parts in [
            json!([{"type": "input_image", "file_id": "file_1", "image_url": "data:image/png;base64,AA=="}]),
            json!([{"type": "input_file", "file_id": "file_1", "file_data": "aGk="}]),
            json!([{"type": "input_file", "file_id": "file_1", "file_url": "https://example.invalid/a.txt"}]),
        ] {
            let mut input = message(&parts);
            let error = hydrate_file_references(&mut input, Some(&service)).await.unwrap_err();
            assert!(invalid_message(error).contains("must not combine file_id"), "{parts}");
        }
    }

    #[tokio::test]
    async fn input_image_rejects_a_non_image_file() {
        let (service, _files) = file_search().await;
        let text = service
            .upload_file("notes.txt", "text/plain", "assistants", b"hello".to_vec())
            .await
            .unwrap();
        let mut input = message(&json!([{"type": "input_image", "file_id": &text.id}]));
        let error = hydrate_file_references(&mut input, Some(&service)).await.unwrap_err();
        assert!(invalid_message(error).contains("image media type"));
    }

    #[tokio::test]
    async fn rejects_an_invalid_stored_content_type() {
        let (service, _files) = file_search().await;
        let file = service
            .upload_file("notes.txt", "text/plain,evil", "assistants", b"hello".to_vec())
            .await
            .unwrap();
        let mut input = message(&json!([{"type": "input_file", "file_id": &file.id}]));
        let error = hydrate_file_references(&mut input, Some(&service)).await.unwrap_err();
        assert!(invalid_message(error).contains("invalid content type"));
    }

    #[tokio::test]
    async fn missing_files_are_reported_as_not_found() {
        let (service, _files) = file_search().await;
        let id = format!("file-{}", uuid::Uuid::now_v7());
        let mut input = message(&json!([{"type": "input_file", "file_id": &id}]));
        let error = hydrate_file_references(&mut input, Some(&service)).await.unwrap_err();
        assert!(matches!(
            error,
            ExecutorError::FileSearch(crate::types::file_search::FileSearchError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn references_require_a_configured_file_store() {
        let id = "file-01a15d85-a1db-76f3-918e-b31d6819a316";
        for (kind, expected) in [("input_image", "input_image"), ("input_file", "input_file")] {
            let mut input = message(&json!([{"type": kind, "file_id": id}]));
            let error = hydrate_file_references(&mut input, None).await.unwrap_err();
            assert!(invalid_message(error).contains(expected));
        }
        let mut input = message(&json!([{"type": "input_text", "text": "no reference"}]));
        hydrate_file_references(&mut input, None).await.unwrap();
    }

    #[tokio::test]
    async fn hydration_budget_counts_every_reference() {
        let (service, _files) = file_search().await;
        let file = service
            .upload_file("notes.txt", "text/plain", "assistants", b"hello".to_vec())
            .await
            .unwrap();
        let data_url_len = format!("data:text/plain;base64,{}", STANDARD.encode(b"hello")).len();
        let limit = data_url_len + 1;

        let mut one = message(&json!([{"type": "input_file", "file_id": &file.id}]));
        hydrate_with_limit(&mut one, Some(&service), limit).await.unwrap();

        let mut two = message(&json!([
            {"type": "input_file", "file_id": &file.id},
            {"type": "input_file", "file_id": &file.id}
        ]));
        let error = hydrate_with_limit(&mut two, Some(&service), limit).await.unwrap_err();
        assert!(matches!(error, ExecutorError::PayloadTooLarge(_)));
    }
}
