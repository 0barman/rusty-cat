use std::path::PathBuf;
use std::sync::Arc;

/// Immutable upload source data used by upload tasks.
///
/// Keep this type crate-internal first. Public APIs expose source selection
/// via builders to preserve compatibility.
#[derive(Clone)]
pub(crate) enum UploadSource {
    /// Upload bytes are read from a local file path.
    File(PathBuf),
    /// Upload bytes are read directly from process memory.
    ///
    /// `Arc<Vec<u8>>` avoids cloning large payload buffers when tasks are
    /// cloned across scheduler/runtime layers.
    Bytes(Arc<Vec<u8>>),
}

impl std::fmt::Debug for UploadSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
            Self::Bytes(bytes) => f.debug_struct("Bytes").field("len", &bytes.len()).finish(),
        }
    }
}
