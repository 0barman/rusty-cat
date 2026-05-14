//! Generic presigned URL upload/download protocols.
//!
//! These types are provider-neutral building blocks for a server-signed transfer
//! model: the application server keeps cloud credentials, returns short-lived
//! presigned/SAS URLs to the client, and `rusty-cat` only performs chunk I/O,
//! retry, progress, lifecycle handling, and optional URL refresh callbacks.
//!
//! This module intentionally does not generate cloud-provider signatures.

mod completion_body_builder;
mod completion_request;
mod headers;
mod multipart_upload;
mod multipart_upload_plan;
mod range_download;
mod range_download_plan;
mod refresher;
mod time;
mod upload_part;
mod uploaded_part;

pub use completion_body_builder::PresignedCompletionBodyBuilder;
pub use completion_request::CompletionRequest;
pub use headers::{headers_from_iter, headers_from_pairs};
pub use multipart_upload::PresignedMultipartUpload;
pub use multipart_upload_plan::PresignedMultipartUploadPlan;
pub use range_download::PresignedRangeDownload;
pub use range_download_plan::PresignedRangeDownloadPlan;
pub use refresher::{PresignedDownloadUrlRefresher, PresignedUploadUrlRefresher};
pub use upload_part::PresignedUploadPart;
pub use uploaded_part::PresignedUploadedPart;

#[cfg(test)]
mod tests;
