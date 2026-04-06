use crate::direction::Direction;
use crate::ids::TaskId;
use crate::transfer_status::TransferStatus;

#[derive(Debug, Clone)]
pub struct FileTransferRecord {
    task_id: TaskId,
    file_sign: String,
    file_name: String,
    total_size: u64,
    progress: f32,
    status: TransferStatus,
    direction: Direction,
}

impl FileTransferRecord {
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

    pub fn file_sign(&self) -> &str {
        &self.file_sign
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    pub fn progress(&self) -> f32 {
        self.progress
    }

    pub fn status(&self) -> &TransferStatus {
        &self.status
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub fn task_id(&self) -> TaskId {
        self.task_id
    }
}
