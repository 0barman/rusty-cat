use crate::presigned::PresignedMultipartUploadPlan;

/// Azure Blob SAS multipart upload plan.
pub type AzureBlobSasMultipartUploadPlan = PresignedMultipartUploadPlan;

/// Backward-compatible alias for older `Presigned` naming.
pub type AzureBlobPresignedMultipartUploadPlan = AzureBlobSasMultipartUploadPlan;
