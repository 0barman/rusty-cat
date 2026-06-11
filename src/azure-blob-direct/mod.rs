//! Azure Blob direct multipart upload/range download protocols using SharedKey.
//!
//! # Uncommitted blocks and storage cost
//!
//! A block-blob upload stages data with `Put Block` and only materializes the
//! blob once `Put Block List` commits. If the upload is interrupted before
//! commit (process crash, power loss, an unrecoverable error, or a cancel whose
//! cleanup `DELETE` also fails), the staged **uncommitted blocks remain on the
//! service and are billed**. Azure garbage-collects uncommitted blocks only
//! after roughly seven days of blob inactivity, so cost can accrue until then.
//!
//! Unlike OSS multipart, Azure block blobs have no separate session id: the
//! resume state is the uncommitted block list keyed by the blob URL, so
//! [`crate::api::UploadResumeInfo::provider_upload_id`] is always `None` here.
//! To bound the cost:
//!
//! 1. **Configure a lifecycle management rule (strongly recommended)** on the
//!    storage account/container to delete stale or never-committed blobs, so the
//!    service reclaims abandoned uploads on a schedule you control rather than
//!    relying solely on the ~7-day default.
//! 2. **Clean up proactively when you can.** On cancel the executor deletes the
//!    target blob; if that `DELETE` fails it is now reported at `WARN` level
//!    (tag `cancel_group`) instead of being swallowed. After a crash, deleting
//!    the blob URL (or letting the lifecycle rule fire) discards the staged
//!    uncommitted blocks.

mod constants;
mod download;
mod put_block_session;
mod signing;
mod time_util;
mod upload;
mod xml;

pub use download::AzureBlobDirectDownload;
pub use upload::AzureBlobDirectUpload;
pub use xml::block_list_xml;
