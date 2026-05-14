use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use crate::error::{InnerErrorCode, MeowError};

use super::{CompletionRequest, PresignedCompletionBodyBuilder, PresignedUploadPart};

/// Full upload plan returned by an application server or provider helper.
#[derive(Clone)]
pub struct PresignedMultipartUploadPlan {
    /// Provider/session identifier, for example OSS uploadId.
    pub upload_id: Option<String>,
    /// Application-defined metadata carried into completion body builders.
    pub metadata: BTreeMap<String, String>,
    /// Total object size.
    pub total_size: u64,
    /// Chunk size used to create the plan.
    pub chunk_size: u64,
    /// Presigned parts. Each part must map to a unique offset.
    pub parts: Vec<PresignedUploadPart>,
    /// Optional completion request. Common presigned OSS flows notify the
    /// application server here so it can verify and merge parts.
    pub complete_request: Option<CompletionRequest>,
    /// Optional abort request called when the task is cancelled.
    pub abort_request: Option<CompletionRequest>,
    /// Optional application-specific completion body builder.
    pub complete_body_builder: Option<Arc<dyn PresignedCompletionBodyBuilder>>,
    /// Refresh threshold in seconds. A part URL is refreshed before upload when
    /// `now + refresh_before_secs >= expires_at_unix_secs`.
    pub refresh_before_secs: u64,
}

impl fmt::Debug for PresignedMultipartUploadPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PresignedMultipartUploadPlan")
            .field("upload_id", &self.upload_id)
            .field("metadata", &self.metadata)
            .field("total_size", &self.total_size)
            .field("chunk_size", &self.chunk_size)
            .field("parts", &self.parts)
            .field("complete_request", &self.complete_request)
            .field("abort_request", &self.abort_request)
            .field(
                "complete_body_builder",
                &self.complete_body_builder.as_ref().map(|_| "<builder>"),
            )
            .field("refresh_before_secs", &self.refresh_before_secs)
            .finish()
    }
}

impl PresignedMultipartUploadPlan {
    /// Creates a plan.
    pub fn new(total_size: u64, chunk_size: u64, parts: Vec<PresignedUploadPart>) -> Self {
        Self {
            upload_id: None,
            metadata: BTreeMap::new(),
            total_size,
            chunk_size,
            parts,
            complete_request: None,
            abort_request: None,
            complete_body_builder: None,
            refresh_before_secs: 60,
        }
    }

    /// Sets provider/session identifier.
    pub fn with_upload_id(mut self, upload_id: impl Into<String>) -> Self {
        self.upload_id = Some(upload_id.into());
        self
    }

    /// Adds application-defined metadata used by custom completion body builders.
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Sets completion callback request.
    pub fn with_complete_request(mut self, req: CompletionRequest) -> Self {
        self.complete_request = Some(req);
        self
    }

    /// Sets an application-specific completion request body builder.
    pub fn with_complete_body_builder(
        mut self,
        builder: Arc<dyn PresignedCompletionBodyBuilder>,
    ) -> Self {
        self.complete_body_builder = Some(builder);
        self
    }

    /// Sets abort callback request.
    pub fn with_abort_request(mut self, req: CompletionRequest) -> Self {
        self.abort_request = Some(req);
        self
    }

    /// Sets the URL refresh threshold in seconds.
    pub fn with_refresh_before_secs(mut self, secs: u64) -> Self {
        self.refresh_before_secs = secs;
        self
    }

    /// Validates basic plan invariants before execution.
    ///
    /// This catches common server-side planning mistakes early, such as an
    /// empty non-zero upload, zero-length parts, duplicate offsets, or parts
    /// outside the declared object size.
    pub fn validate(&self) -> Result<(), MeowError> {
        if self.chunk_size == 0 {
            return Err(MeowError::from_code_str(
                InnerErrorCode::ParameterEmpty,
                "presigned plan chunk_size must be greater than zero",
            ));
        }
        if self.total_size > 0 && self.parts.is_empty() {
            return Err(MeowError::from_code_str(
                InnerErrorCode::ParameterEmpty,
                "presigned plan parts must not be empty for non-empty upload",
            ));
        }
        let mut offsets = BTreeSet::new();
        for part in &self.parts {
            if part.size == 0 {
                return Err(MeowError::from_code(
                    InnerErrorCode::InvalidRange,
                    format!("presigned part {} has zero size", part.part_number),
                ));
            }
            let end = part.offset.checked_add(part.size).ok_or_else(|| {
                MeowError::from_code(
                    InnerErrorCode::InvalidRange,
                    format!("presigned part {} range overflow", part.part_number),
                )
            })?;
            if end > self.total_size {
                return Err(MeowError::from_code(
                    InnerErrorCode::InvalidRange,
                    format!(
                        "presigned part {} out of range: end={} total={}",
                        part.part_number, end, self.total_size
                    ),
                ));
            }
            if !offsets.insert(part.offset) {
                return Err(MeowError::from_code(
                    InnerErrorCode::InvalidRange,
                    format!("duplicate presigned part offset: {}", part.offset),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn part_for_offset(&self, offset: u64) -> Option<&PresignedUploadPart> {
        self.parts.iter().find(|p| p.offset == offset)
    }
}
