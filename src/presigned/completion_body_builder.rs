use crate::error::MeowError;

use super::{PresignedMultipartUploadPlan, PresignedUploadedPart};

/// Builds the request body used by a presigned multipart completion callback.
///
/// Applications can use this to adapt provider-neutral uploaded part metadata
/// into their own backend contract, such as `{ key, upload_id, parts }`.
pub trait PresignedCompletionBodyBuilder: Send + Sync {
    /// Builds completion request bytes from the immutable upload plan and the
    /// parts uploaded successfully in this process.
    fn build_body(
        &self,
        plan: &PresignedMultipartUploadPlan,
        uploaded_parts: &[PresignedUploadedPart],
    ) -> Result<Vec<u8>, MeowError>;
}

impl<F> PresignedCompletionBodyBuilder for F
where
    F: Fn(&PresignedMultipartUploadPlan, &[PresignedUploadedPart]) -> Result<Vec<u8>, MeowError>
        + Send
        + Sync,
{
    fn build_body(
        &self,
        plan: &PresignedMultipartUploadPlan,
        uploaded_parts: &[PresignedUploadedPart],
    ) -> Result<Vec<u8>, MeowError> {
        self(plan, uploaded_parts)
    }
}
