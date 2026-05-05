use crate::presigned::PresignedMultipartUpload;

/// Azure Blob SAS Put Block upload protocol.
pub type AzureBlobSasMultipartUpload = PresignedMultipartUpload;

/// Backward-compatible alias for older `Presigned` naming.
pub type AzureBlobPresignedMultipartUpload = AzureBlobSasMultipartUpload;
