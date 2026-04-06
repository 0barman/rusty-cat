/// [`crate::transfer_executor_trait::TransferTrait::prepare`] 的返回值：下一字节偏移与（若已知）资源总大小。
#[derive(Debug, Clone, Copy)]
pub struct PrepareOutcome {
    pub next_offset: u64,
    pub total_size: u64,
}
