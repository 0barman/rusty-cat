use crate::presigned::PresignedRangeDownloadPlan;

/// Azure Blob SAS range download plan.
pub type AzureBlobSasRangeDownloadPlan = PresignedRangeDownloadPlan;

/// Backward-compatible alias for older `Presigned` naming.
pub type AzureBlobPresignedRangeDownloadPlan = AzureBlobSasRangeDownloadPlan;
