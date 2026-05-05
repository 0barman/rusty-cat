/// Result captured after a presigned part upload succeeds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PresignedUploadedPart {
    /// Provider-defined part number.
    pub part_number: u64,
    /// Optional provider-specific part identifier, for example Azure block id.
    pub provider_part_id: Option<String>,
    /// Start offset in the full file.
    pub offset: u64,
    /// Uploaded byte size.
    pub size: u64,
    /// Optional ETag returned by providers such as Aliyun OSS/S3.
    pub etag: Option<String>,
}
