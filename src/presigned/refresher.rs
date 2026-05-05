use async_trait::async_trait;

use crate::error::MeowError;

use super::{PresignedRangeDownloadPlan, PresignedUploadPart};

/// Refreshes presigned upload URLs when they are expired or close to expiring.
///
/// Implement this trait in application code when a backend service can issue a
/// fresh URL for the same multipart part. The refreshed part must keep the same
/// `part_number`, `offset`, and `size` so the executor never uploads bytes to a
/// mismatched remote part.
#[async_trait]
pub trait PresignedUploadUrlRefresher: Send + Sync {
    /// Returns a fresh upload part description for the same file range.
    async fn refresh_upload_part(
        &self,
        part: &PresignedUploadPart,
    ) -> Result<PresignedUploadPart, MeowError>;
}

/// Refreshes presigned range-download URLs when they are expired or close to
/// expiring.
///
/// This trait is synchronous because [`crate::BreakpointDownload`] URL/header
/// hooks are synchronous. Implementors should keep this operation lightweight or
/// use a local cache populated by application code.
pub trait PresignedDownloadUrlRefresher: Send + Sync {
    /// Returns a fresh range-download plan for the same object.
    fn refresh_range_download(
        &self,
        plan: &PresignedRangeDownloadPlan,
    ) -> Result<PresignedRangeDownloadPlan, MeowError>;
}
