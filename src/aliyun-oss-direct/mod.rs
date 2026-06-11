//! Aliyun OSS direct multipart upload/range download protocols using OSS V4 signing.
//!
//! # Orphaned multipart uploads and storage cost
//!
//! A multipart upload that is interrupted (process crash, power loss, an
//! unrecoverable error, or a cancel whose `AbortMultipartUpload` call also
//! fails) leaves uploaded parts on OSS. **These parts keep occupying storage and
//! are billed until they are explicitly aborted or removed.** The bundled
//! executor calls [`AliOssDirectUpload`]'s abort hook on cancel, but it cannot
//! help after a crash, because the in-memory `UploadId` is lost with the process.
//!
//! To keep this from silently accruing cost, do both of the following:
//!
//! 1. **Configure a bucket lifecycle rule (strongly recommended).** Add an
//!    `AbortMultipartUpload` lifecycle rule (for example "abort incomplete
//!    multipart uploads after N days") so OSS reclaims orphaned parts
//!    automatically. This is the only defense against crashes that never run any
//!    cleanup code.
//! 2. **Persist the `UploadId` for targeted cleanup.** Read it from
//!    [`crate::api::UploadResumeInfo::provider_upload_id`] or
//!    [`AliOssDirectUpload::current_upload_id`] and store it durably. After a
//!    crash you can reconstruct the protocol and call
//!    [`AliOssDirectUpload::abort_multipart`] with the persisted id to abort the
//!    session immediately instead of waiting for the lifecycle rule.
//!
//! When a cancel-time abort fails, the executor now emits a `WARN`-level log
//! (tag `cancel_group`) containing the failing `UploadId` instead of swallowing
//! it, so the condition is observable.

mod constants;
mod download;
mod multipart_session;
mod signing;
mod upload;
mod xml;

pub use download::AliOssDirectDownload;
pub use upload::AliOssDirectUpload;
