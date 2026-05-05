//! Aliyun OSS presigned multipart upload/range download helpers.
//!
//! This module intentionally does not generate Aliyun V4 signatures. The
//! expected security model is: an application server keeps AccessKey credentials,
//! creates short-lived presigned `UploadPart`/range URLs, and the client uses
//! `rusty-cat` only to execute multipart upload/download against those URLs.

mod constants;
mod download_plan;
mod multipart_upload;
mod multipart_upload_plan;
mod range_download;
mod range_download_plan;
mod upload_part;

pub use download_plan::{range_download_with_expiry, range_download_with_total_size};
pub use multipart_upload::AliOssPresignedMultipartUpload;
pub use multipart_upload_plan::AliOssPresignedMultipartUploadPlan;
pub use range_download::AliOssPresignedRangeDownload;
pub use range_download_plan::AliOssPresignedRangeDownloadPlan;
pub use upload_part::{upload_part, upload_part_with_expiry};

#[cfg(test)]
mod tests {
    #[test]
    fn test_upload_part_records_part_number_as_provider_id() {
        let part = super::upload_part(7, 30, 10, "https://example.com/p7");
        assert_eq!(part.provider_part_id.as_deref(), Some("7"));
    }

    #[test]
    fn test_range_download_with_expiry_sets_metadata() {
        let plan = super::range_download_with_expiry("https://example.com/file", 100, 1234);
        assert_eq!(plan.total_size, Some(100));
        assert_eq!(plan.range_expires_at_unix_secs, Some(1234));
    }
}
