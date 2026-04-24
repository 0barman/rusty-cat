use std::path::PathBuf;

use bytes::Bytes;

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
    /// `Bytes` is cheap to clone (refcount bump) and slicing is zero-copy, so
    /// tasks cloned across scheduler/runtime layers and per-chunk slices never
    /// duplicate the underlying payload.
    Bytes(Bytes),
}

impl std::fmt::Debug for UploadSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
            Self::Bytes(bytes) => f.debug_struct("Bytes").field("len", &bytes.len()).finish(),
        }
    }
}
