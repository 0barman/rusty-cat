use crate::presigned::PresignedRangeDownload;

/// Azure Blob SAS range download protocol.
pub type AzureBlobSasRangeDownload = PresignedRangeDownload;

/// Backward-compatible alias for older `Presigned` naming.
pub type AzureBlobPresignedRangeDownload = AzureBlobSasRangeDownload;
