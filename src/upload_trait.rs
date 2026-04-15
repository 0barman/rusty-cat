use async_trait::async_trait;
use crate::http_breakpoint::UploadResumeInfo;
use crate::{MeowError, TransferTask};

/// 自定义断点上传协议。
///
/// 实现方负责：按业务约定构造 HTTP 请求（multipart 或二进制体）、解析响应体为 [`UploadResumeInfo`]。
/// 执行器负责：打开本地文件、按 [`TransferTask`] 的 `chunk_size` 读分片、并发/重试/进度/暂停恢复；
/// 每次上传前会调用 [`BreakpointUpload::prepare`]，随后对每个分片调用 [`BreakpointUpload::upload_chunk`]。
///
/// # 与执行器协作的约定
///
/// - 若返回的 [`UploadResumeInfo::completed_file_id`] 为 `Some`，执行器认为该次 HTTP 已表示**整文件上传结束**，
///   会直接将进度推到文件末尾并结束任务（不再继续分片，也不再调用 [`BreakpointUpload::complete_upload`]）。
/// - [`UploadResumeInfo::next_byte`] 表示服务端建议的下一字节偏移；执行器会将其与本地已传偏移合并后再继续。
/// - 当某次分片后计算得到的下一偏移已到达或超过 `task.total_size()` 时，执行器会调用
///   [`BreakpointUpload::complete_upload`]（若该分片响应未通过 `completed_file_id` 提前结束）。
/// - 用户取消上传任务时，执行器对上传方向会调用 [`BreakpointUpload::abort_upload`]。
#[async_trait]
pub trait BreakpointUpload: Send + Sync {
    /// 传输开始前的准备阶段：通常对应「初始化会话 / 查询已传进度」等一次或多次 HTTP 调用。
    ///
    /// 在首个分片上传之前由执行器调用一次，用于与服务端对齐续传点或创建分片上传会话。
    ///
    /// # 参数
    ///
    /// - `client`：执行器持有的 [`reqwest::Client`]，应用与分片阶段使用同一客户端配置（代理、TLS、超时等）。
    /// - `task`：当前上传任务快照，含 URL、方法、请求头、本地路径、文件签名、总分片大小等；请求 URL/方法/头
    ///   一般应与此保持一致，除非协议明确要求单独的 prepare 端点（此时由实现自行决定 URL）。
    /// - `local_offset`：本地已确认写入的字节偏移（断点续传时非零）；实现可据此与服务端 `nextByte` 等字段对齐。
    ///
    /// # 返回值
    ///
    /// - `Ok(info)`：见 [`UploadResumeInfo`]。若 `completed_file_id` 为 `Some`，执行器**立即**结束上传成功路径。
    ///   否则执行器取 `info.next_byte.unwrap_or(0)`，与 `local_offset` 取较大值后再与 `task.total_size()` 取较小值，
    ///   作为下一分片的起始偏移。
    /// - `Err`：准备失败，任务进入失败状态（可能触发重试策略，取决于上层配置）。
    async fn prepare(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
        local_offset: u64,
    ) -> Result<UploadResumeInfo, MeowError>;

    /// 上传单个分片：从本地文件读出的一段字节，由实现封装为 HTTP 请求并发送。
    ///
    /// 执行器保证：`chunk` 长度与当前分片一致（最后一片可能更短），`offset` 为该分片在文件中的起始字节偏移，
    /// 且 `offset + chunk.len() <= task.total_size()`（除边界情况外由执行器保证顺序与范围）。
    ///
    /// # 参数
    ///
    /// - `client`：同上，用于发送本分片的 HTTP 请求。
    /// - `task`：当前任务；实现从中读取 URL、方法、头、文件名、总大小、签名等元数据以拼请求体或查询参数。
    /// - `chunk`：本分片原始字节，只读；不应假设跨调用缓存，每次调用对应独立一次 HTTP。
    /// - `offset`：本分片在整文件中的起始偏移（从 0 开始），与 `chunk` 内容一一对应。
    ///
    /// # 返回值
    ///
    /// - `Ok(info)`：若 `completed_file_id` 为 `Some`，执行器认为上传已完成。否则根据 `next_byte` 更新进度：
    ///   下一偏移为 `info.next_byte.unwrap_or(offset + chunk.len() as u64)`，再与 `task.total_size()` 取最小值。
    ///   若该下一偏移已达到或超过文件总大小，执行器随后调用 [`BreakpointUpload::complete_upload`]。
    /// - `Err`：本分片失败，由执行器按重试/失败策略处理。
    async fn upload_chunk(
        &self,
        client: &reqwest::Client,
        task: &TransferTask,
        chunk: &[u8],
        offset: u64,
    ) -> Result<UploadResumeInfo, MeowError>;

    /// 所有分片字节已按协议上传完毕后，由执行器调用的收尾步骤。
    ///
    /// 典型场景：对象存储的 `CompleteMultipartUpload`、合并块列表、通知业务侧落库等。默认实现为空操作（`Ok(())`），
    /// 适用于服务端在最后一个分片响应中即视为完成、无需额外 HTTP 的协议。
    ///
    /// # 参数
    ///
    /// - `client`：用于发送完成/提交类请求（若需要）。
    /// - `task`：当前任务，用于 URL、头及协议所需的业务标识。
    ///
    /// # 返回值
    ///
    /// - `Ok(())`：收尾成功，执行器将任务标为完成。
    /// - `Err`：收尾失败，任务失败（可能经重试，取决于上层）。
    async fn complete_upload(
        &self,
        _client: &reqwest::Client,
        _task: &TransferTask,
    ) -> Result<(), MeowError> {
        Ok(())
    }

    /// 用户取消上传任务时，由执行器调用，用于协议层的清理或中止语义。
    ///
    /// 例如：中止分片上传会话、删除未合并的临时对象。默认实现为空操作。仅在上传方向的任务取消路径中调用。
    ///
    /// # 参数
    ///
    /// - `client`：用于发送中止/删除类请求（若需要）。
    /// - `task`：被取消的任务，用于定位远端资源或会话 ID。
    ///
    /// # 返回值
    ///
    /// - `Ok(())`：取消侧协议处理成功（或无可执行操作）。
    /// - `Err`：中止请求失败；是否影响任务已处于取消状态由上层决定，但实现应尽量返回明确错误信息。
    async fn abort_upload(
        &self,
        _client: &reqwest::Client,
        _task: &TransferTask,
    ) -> Result<(), MeowError> {
        Ok(())
    }
}
