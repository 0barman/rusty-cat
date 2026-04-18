use crate::error::MeowError;

/// High-level lifecycle state of a transfer task.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TransferStatus {
    /// Initial placeholder state before scheduling.
    #[default]
    None,

    /// Queued and waiting for execution.
    Pending,

    /// Currently uploading or downloading.
    Transmission,

    /// Temporarily paused by user operation.
    Paused,

    /// Finished successfully.
    Complete,

    /// Failed with an associated error.
    Failed(MeowError),

    /// Canceled by user or caller.
    Canceled,
}

impl TransferStatus {
    /// Maps status to an integer code for external integrations.
    ///
    /// Mapping:
    /// - `Pending=0`
    /// - `Transmission=1`
    /// - `Paused=2`
    /// - `Complete=3`
    /// - `Failed=4`
    /// - `Canceled=5`
    /// - `None=-1`
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::TransferStatus;
    ///
    /// assert_eq!(TransferStatus::Pending.as_i32(), 0);
    /// assert_eq!(TransferStatus::Complete.as_i32(), 3);
    /// ```
    pub fn as_i32(&self) -> i32 {
        match self {
            TransferStatus::Pending => 0,
            TransferStatus::Transmission => 1,
            TransferStatus::Paused => 2,
            TransferStatus::Complete => 3,
            TransferStatus::Failed(_) => 4,
            TransferStatus::Canceled => 5,
            TransferStatus::None => -1,
        }
    }
}
