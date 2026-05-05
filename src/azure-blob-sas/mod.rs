//! Azure Blob SAS multipart upload/range download helpers.
//!
//! This module intentionally does not generate SharedKey signatures or Entra ID
//! tokens. The expected security model is: an application server keeps Azure
//! credentials, creates short-lived SAS URLs, and the client uses `rusty-cat`
//! only to execute `Put Block`, `Put Block List`, and range download requests.

mod block_id;
mod block_list;
mod constants;
mod download_plan;
mod multipart_upload;
mod multipart_upload_plan;
mod put_block;
mod range_download;
mod range_download_plan;

pub use block_id::block_id_by_index;
pub use block_list::{block_list_xml, put_block_list_request, put_block_list_url};
pub use download_plan::range_download_with_total_size;
pub use multipart_upload::{AzureBlobPresignedMultipartUpload, AzureBlobSasMultipartUpload};
pub use multipart_upload_plan::{
    AzureBlobPresignedMultipartUploadPlan, AzureBlobSasMultipartUploadPlan,
};
pub use put_block::{put_block, put_block_from_blob_url};
pub use range_download::{AzureBlobPresignedRangeDownload, AzureBlobSasRangeDownload};
pub use range_download_plan::{AzureBlobPresignedRangeDownloadPlan, AzureBlobSasRangeDownloadPlan};

#[cfg(test)]
mod tests {
    use super::block_id_by_index;

    #[test]
    fn test_block_id_by_index_is_stable() {
        assert_eq!(block_id_by_index(1), "MDAwMDAwMDE=");
    }

    #[test]
    fn test_put_block_from_blob_url_preserves_sas_query() {
        let part = super::put_block_from_blob_url(
            "https://acct.blob.core.windows.net/c/b.bin?sv=2024-01-01&sig=x",
            1,
            0,
            5,
        )
        .expect("put block url");
        assert!(part.url.contains("sv=2024-01-01"));
        assert!(part.url.contains("comp=block"));
        assert!(part.url.contains("blockid=MDAwMDAwMDE"));
        assert_eq!(part.provider_part_id.as_deref(), Some("MDAwMDAwMDE="));
    }

    #[test]
    fn test_put_block_list_request_builds_xml_and_url() {
        let req = super::put_block_list_request(
            "https://acct.blob.core.windows.net/c/b.bin?sv=2024-01-01&sig=x",
            ["MDAwMDAwMDA=", "MDAwMDAwMDE="],
        )
        .expect("block list request");
        assert!(req.url.contains("comp=blocklist"));
        let body = String::from_utf8(req.body.expect("xml body")).expect("utf8 xml");
        assert!(body.contains("<Latest>MDAwMDAwMDA=</Latest>"));
        assert!(body.contains("<Latest>MDAwMDAwMDE=</Latest>"));
    }
}
