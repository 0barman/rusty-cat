use std::collections::BTreeSet;

use crate::error::{InnerErrorCode, MeowError};

use super::{CompletionRequest, PresignedUploadPart};

/// Full upload plan returned by an application server or provider helper.
#[derive(Debug, Clone)]
pub struct PresignedMultipartUploadPlan {
    /// Provider/session identifier, for example OSS uploadId.
    pub upload_id: Option<String>,
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
    /// Refresh threshold in seconds. A part URL is refreshed before upload when
    /// `now + refresh_before_secs >= expires_at_unix_secs`.
    pub refresh_before_secs: u64,
}

impl PresignedMultipartUploadPlan {
    /// Creates a plan.
    pub fn new(total_size: u64, chunk_size: u64, parts: Vec<PresignedUploadPart>) -> Self {
        Self {
            upload_id: None,
            total_size,
            chunk_size,
            parts,
            complete_request: None,
            abort_request: None,
            refresh_before_secs: 60,
        }
    }

    /// Sets provider/session identifier.
    pub fn with_upload_id(mut self, upload_id: impl Into<String>) -> Self {
        self.upload_id = Some(upload_id.into());
        self
    }

    /// Sets completion callback request.
    pub fn with_complete_request(mut self, req: CompletionRequest) -> Self {
        self.complete_request = Some(req);
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
