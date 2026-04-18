use crate::direction::Direction;
use crate::ids::TaskId;
use crate::transfer_status::TransferStatus;

/// Immutable progress/state record emitted by transfer callbacks.
///
/// This snapshot is safe to clone and pass across threads.
#[derive(Debug, Clone)]
pub struct FileTransferRecord {
    /// Unique task identifier.
    task_id: TaskId,
    /// File signature (upload usually MD5; download may use client-defined value).
    file_sign: String,
    /// Display file name.
    file_name: String,
    /// Total file size in bytes.
    total_size: u64,
    /// Progress ratio in range `0.0..=1.0`.
    progress: f32,
    /// Current transfer state.
    status: TransferStatus,
    /// Transfer direction.
    direction: Direction,
}

impl FileTransferRecord {
    /// Creates a new transfer record.
    ///
    /// # Range guidance
    ///
    /// - `total_size >= 0`
    /// - `progress` should stay in `0.0..=1.0`
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{Direction, FileTransferRecord, TaskId, TransferStatus};
    ///
    /// # fn sample(task_id: TaskId) {
    /// let record = FileTransferRecord::new(
    ///     task_id,
    ///     "sign".to_string(),
    ///     "file.bin".to_string(),
    ///     1024,
    ///     0.5,
    ///     TransferStatus::Transmission,
    ///     Direction::Download,
    /// );
    /// assert_eq!(record.total_size(), 1024);
    /// # }
    /// ```
    pub fn new(
        task_id: TaskId,
        file_sign: String,
        file_name: String,
        total_size: u64,
        progress: f32,
        status: TransferStatus,
        direction: Direction,
    ) -> Self {
        Self {
            task_id,
            file_sign,
            file_name,
            total_size,
            progress,
            status,
            direction,
        }
    }

    /// Returns file signature.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::FileTransferRecord;
    ///
    /// fn read_sign(record: &FileTransferRecord) {
    ///     let _ = record.file_sign();
    /// }
    /// ```
    pub fn file_sign(&self) -> &str {
        &self.file_sign
    }

    /// Returns display file name.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::FileTransferRecord;
    ///
    /// fn read_name(record: &FileTransferRecord) {
    ///     let _ = record.file_name();
    /// }
    /// ```
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// Returns total file size in bytes.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::FileTransferRecord;
    ///
    /// fn read_size(record: &FileTransferRecord) {
    ///     let _ = record.total_size();
    /// }
    /// ```
    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    /// Returns progress ratio in range `0.0..=1.0`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::FileTransferRecord;
    ///
    /// fn read_progress(record: &FileTransferRecord) {
    ///     let _ = record.progress();
    /// }
    /// ```
    pub fn progress(&self) -> f32 {
        self.progress
    }

    /// Returns current transfer status.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::FileTransferRecord;
    ///
    /// fn read_status(record: &FileTransferRecord) {
    ///     let _ = record.status();
    /// }
    /// ```
    pub fn status(&self) -> &TransferStatus {
        &self.status
    }

    /// Returns transfer direction.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::FileTransferRecord;
    ///
    /// fn read_direction(record: &FileTransferRecord) {
    ///     let _ = record.direction();
    /// }
    /// ```
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// Returns task ID.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::FileTransferRecord;
    ///
    /// fn read_task_id(record: &FileTransferRecord) {
    ///     let _ = record.task_id();
    /// }
    /// ```
    pub fn task_id(&self) -> TaskId {
        self.task_id
    }
}
