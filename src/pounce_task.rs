use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use reqwest::header::HeaderMap;
use reqwest::Method;

use crate::direction::Direction;
use crate::http_breakpoint::{BreakpointDownload, BreakpointDownloadHttpConfig, BreakpointUpload};
use crate::upload_source::UploadSource;

/// User-facing task input built by upload/download builders.
///
/// This type only carries request parameters. Internal runtime state is created
/// later when the task is enqueued.
#[derive(Clone)]
pub struct PounceTask {
    /// Transfer direction.
    pub(crate) direction: Direction,
    /// Display file name.
    pub(crate) file_name: String,
    /// Local source/target path.
    pub(crate) file_path: PathBuf,
    /// Upload-only source descriptor.
    pub(crate) upload_source: Option<UploadSource>,
    /// Total file size in bytes (upload only at build time).
    pub(crate) total_size: u64,
    /// Chunk size in bytes.
    pub(crate) chunk_size: u64,
    /// Request URL.
    pub(crate) url: String,
    /// Request HTTP method.
    pub(crate) method: Method,
    /// Base request headers.
    pub(crate) headers: HeaderMap,
    /// Download-only signature shown in callbacks.
    ///
    /// Upload tasks ignore this value and use internal signature generation.
    pub(crate) client_file_sign: Option<String>,
    /// Optional custom upload breakpoint protocol.
    pub(crate) breakpoint_upload: Option<Arc<dyn BreakpointUpload + Send + Sync>>,
    /// Optional custom download breakpoint protocol.
    pub(crate) breakpoint_download: Option<Arc<dyn BreakpointDownload + Send + Sync>>,
    /// Optional HTTP configuration for breakpoint download.
    pub(crate) breakpoint_download_http: Option<BreakpointDownloadHttpConfig>,
    /// Maximum retry count per chunk transfer.
    ///
    /// Applies only to chunk transfer stage, not prepare stage.
    pub(crate) max_chunk_retries: u32,
    /// Maximum retry count after the first failed upload `prepare` (`BreakpointUpload::prepare`).
    ///
    /// Used only for upload direction; download tasks carry the default but do not consult it.
    pub(crate) max_upload_prepare_retries: u32,
}

impl fmt::Debug for PounceTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PounceTask")
            .field("direction", &self.direction)
            .field("file_name", &self.file_name)
            .field("file_path", &self.file_path)
            .field("upload_source", &self.upload_source)
            .field("total_size", &self.total_size)
            .field("chunk_size", &self.chunk_size)
            .field("url", &self.url)
            .field("method", &self.method)
            .field("headers", &self.headers)
            .field("client_file_sign", &self.client_file_sign)
            .field(
                "breakpoint_upload",
                &self
                    .breakpoint_upload
                    .as_ref()
                    .map(|_| "Arc<dyn BreakpointUpload + Send + Sync>"),
            )
            .field(
                "breakpoint_download",
                &self
                    .breakpoint_download
                    .as_ref()
                    .map(|_| "Arc<dyn BreakpointDownload + Send + Sync>"),
            )
            .field("breakpoint_download_http", &self.breakpoint_download_http)
            .field("max_chunk_retries", &self.max_chunk_retries)
            .field(
                "max_upload_prepare_retries",
                &self.max_upload_prepare_retries,
            )
            .finish()
    }
}

impl PounceTask {
    /// Default maximum retry count per chunk transfer.
    pub const DEFAULT_MAX_CHUNK_RETRIES: u32 = 3;

    /// Default maximum retry count after the first failed upload prepare.
    pub const DEFAULT_MAX_UPLOAD_PREPARE_RETRIES: u32 = 3;

    /// Normalizes chunk size input.
    ///
    /// `0` is converted to `1 MiB`; other values are kept unchanged.
    pub(crate) fn normalized_chunk_size(chunk_size: u64) -> u64 {
        if chunk_size == 0 {
            1024 * 1024
        } else {
            chunk_size
        }
    }

    /// Normalizes retry count input.
    ///
    /// - `0` means "disable retry".
    /// - Other values are used as-is.
    pub(crate) fn normalized_max_chunk_retries(max_chunk_retries: u32) -> u32 {
        max_chunk_retries
    }

    /// Normalizes upload prepare retry count input (same rules as chunk retries).
    pub(crate) fn normalized_max_upload_prepare_retries(max_upload_prepare_retries: u32) -> u32 {
        max_upload_prepare_retries
    }

    /// Checks whether required task fields are missing/invalid.
    ///
    /// For upload, `total_size` must be greater than `0`.
    pub(crate) fn is_empty(&self) -> bool {
        self.file_name.is_empty()
            || self.url.is_empty()
            || match self.direction {
                Direction::Upload => self.total_size == 0 || self.upload_source.is_none(),
                Direction::Download => self.file_path.as_os_str().is_empty(),
            }
    }
}
