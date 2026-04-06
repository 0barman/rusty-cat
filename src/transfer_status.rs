use crate::error::MeowError;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TransferStatus {
    #[default]
    None,

    /// 待上传/下载
    Pending,

    /// 上传中/下载中
    Transmission,

    /// 上传/下载暂停
    Paused,

    /// 上传/下载完成
    Complete,

    /// 上传/下载出错
    Failed(MeowError),

    /// 上传/下载取消
    Canceled,
}

impl TransferStatus {
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
