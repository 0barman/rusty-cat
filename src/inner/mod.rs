use crate::direction::Direction;

/// 任务去重键：上传为 `(Upload, file_sign)`，下载为 `(Download, url)`。
pub(crate) type UniqueId = (Direction, String);

pub(crate) mod active_state;
pub(crate) mod exec_impl;
pub(crate) mod executor;
pub(crate) mod group_state;
pub(crate) mod inner_task;
pub(crate) mod scheduler_state;
pub(crate) mod sign;
pub(crate) mod task_callbacks;
pub(crate) mod worker_event;
