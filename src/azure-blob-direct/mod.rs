//! Azure Blob direct multipart upload/range download protocols using SharedKey.

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
