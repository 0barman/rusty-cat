use crate::presigned::PresignedRangeDownloadPlan;

/// Creates an Azure SAS range-download plan using known object size.
pub fn range_download_with_total_size(
    url: impl Into<String>,
    total_size: u64,
) -> PresignedRangeDownloadPlan {
    PresignedRangeDownloadPlan::new(url).with_total_size(total_size)
}
