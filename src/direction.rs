#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Uploads local file data to remote service.
    Upload,
    /// Downloads remote file data to local storage.
    Download,
}
