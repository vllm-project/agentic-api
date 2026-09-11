//! Vector Store lifecycle operations, separate from model preparation and transport.
use super::{
    Arc, AtomicBool, CancelIngestionOnDrop, FileObject, FileSearchError, FileSearchService, VectorStoreFileObject,
    VectorStoreObject, ingest, invalid, validate_attributes,
};

pub(super) fn validate_store_fields(
    name: Option<&str>,
    metadata: Option<&std::collections::BTreeMap<String, String>>,
    expiration: Option<&crate::types::file_search::VectorStoreExpiresAfter>,
) -> Result<(), FileSearchError> {
    if name.is_some_and(|name| name.len() > 256) {
        return invalid("vector store name must not exceed 256 bytes");
    }
    if metadata.is_some_and(|metadata| {
        metadata.len() > 16
            || metadata
                .iter()
                .any(|(key, value)| key.is_empty() || key.chars().count() > 64 || value.chars().count() > 512)
    }) {
        return invalid(
            "metadata accepts at most 16 entries, with 1 to 64 character keys and values up to 512 characters",
        );
    }
    if let Some(expiration) = expiration {
        expiration.validate()?;
    }
    Ok(())
}

impl FileSearchService {
    /// Updates only provided fields. Expired stores cannot be revived.
    /// # Errors
    /// Returns validation, not-found, or storage errors.
    pub async fn update_vector_store(
        &self,
        id: &str,
        request: crate::types::file_search::UpdateVectorStoreRequest,
    ) -> Result<VectorStoreObject, FileSearchError> {
        validate_store_fields(
            request.name.0.as_ref().and_then(Option::as_deref),
            request.metadata.0.as_ref().and_then(Option::as_ref),
            request.expires_after.0.as_ref().and_then(Option::as_ref),
        )?;
        self.storage.update_store(id, request).await?;
        self.storage.store_object(id).await
    }

    /// Replaces attachment attributes, including the attributes used for retrieval.
    /// # Errors
    /// Returns validation, not-found, or storage errors.
    pub async fn update_vector_store_file(
        &self,
        store_id: &str,
        file_id: &str,
        request: crate::types::file_search::UpdateVectorStoreFileRequest,
    ) -> Result<VectorStoreFileObject, FileSearchError> {
        let _permit = self.permit()?;
        validate_attributes(&request.attributes)?;
        self.storage
            .update_attachment(store_id, file_id, request.attributes)
            .await
    }

    /// Returns extracted original text as a single bounded page, without chunk overlap or context hints.
    /// Legacy attachments reparse the original upload without calling model providers.
    /// # Errors
    /// Returns not-found, parser, resource-limit, or storage errors.
    pub async fn vector_store_file_content(
        &self,
        store_id: &str,
        file_id: &str,
    ) -> Result<crate::types::file_search::VectorStoreFileContentPage, FileSearchError> {
        let permit = self.permit()?;
        self.get_vector_store_file(store_id, file_id).await?;
        let text = if let Some(text) = self.storage.parsed_content(store_id, file_id).await? {
            text
        } else {
            let uploaded = self.storage.file(file_id).await?;
            let file: FileObject = serde_json::from_str(&uploaded.data)?;
            let bytes = self
                .read_content(file_id, file.bytes, uploaded.content_base64, permit.clone())
                .await?;
            let cancelled = Arc::new(AtomicBool::new(false));
            let _cancel_on_drop = CancelIngestionOnDrop(cancelled.clone());
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                ingest::extract_text(bytes, &file.filename, &uploaded.content_type, &cancelled)
            })
            .await??
        };
        self.get_vector_store_file(store_id, file_id).await?;
        Ok(crate::types::file_search::VectorStoreFileContentPage {
            object: "vector_store.file_content.page".into(),
            data: vec![crate::types::file_search::ParsedFileContent::Text { text }],
            has_more: false,
            next_page: None,
        })
    }

    /// Removes attachments and search data from at most `limit` expired stores, preserving uploads and store metadata.
    /// Explicit cleanup is restart-safe and does not start background tasks.
    /// # Errors
    /// Returns invalid-request unless limit is 1..1000, or storage errors.
    pub async fn cleanup_expired_vector_stores(&self, limit: usize) -> Result<usize, FileSearchError> {
        if !(1..=1000).contains(&limit) {
            return invalid("cleanup limit must be between 1 and 1000");
        }
        self.storage.expire_stores(limit).await
    }
}
