use crate::presigned::PresignedRangeDownloadPlan;

/// Creates an Aliyun presigned range-download plan that skips HEAD by using a
/// known object size returned by the application server.
pub fn range_download_with_total_size(
    url: impl Into<String>,
    total_size: u64,
) -> PresignedRangeDownloadPlan {
    PresignedRangeDownloadPlan::new(url).with_total_size(total_size)
}

/// Creates an Aliyun presigned range-download plan with URL expiration metadata.
pub fn range_download_with_expiry(
    url: impl Into<String>,
    total_size: u64,
    expires_at_unix_secs: u64,
) -> PresignedRangeDownloadPlan {
    range_download_with_total_size(url, total_size)
        .with_range_expires_at_unix_secs(expires_at_unix_secs)
}
