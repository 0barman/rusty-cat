use reqwest::header::HeaderMap;
use crate::{InnerErrorCode, MeowError, TransferTask};

/// 自定义断点下载协议。
///
/// 实现方负责：HEAD 与分片 GET 的 URL/请求头语义，以及从 HEAD 响应中解析远端资源总长度。
/// 执行器负责：发起 HTTP、校验 `206 Partial Content` 与 `Content-Range`、写本地文件、并发/重试/进度/暂停恢复。
///
/// 典型调用顺序：
///
/// 1. 准备阶段：克隆 [`TransferTask`] 上的请求头，用 [`BreakpointDownload::head_url`] 得到 HEAD URL，
///    再经 [`BreakpointDownload::merge_head_headers`] 合并 HEAD 专用头后发送 `HEAD`；
///    成功后用 [`BreakpointDownload::total_size_from_head`] 从响应头得到远端总字节数，并与本地已落盘长度对齐续传起点。
/// 2. 分片阶段：对每个分片克隆任务头，调用 [`BreakpointDownload::merge_range_get_headers`] 写入 `Range`（及实现约定的其它头），
///    对 [`TransferTask::url`] 发起 `GET`；执行器要求响应状态为 `206`，且带合法 `Content-Range`。
///
/// # 与执行器协作的约定
///
/// - [`TransferTask`] 上的 [`crate::http_breakpoint::BreakpointDownloadHttpConfig`]（由任务或 [`crate::meow_config::MeowConfig`] 提供）
///   中的 `range_accept` 会被默认的 [`BreakpointDownload::merge_range_get_headers`] 用作 Range GET 的 `Accept`；
///   自定义实现若覆盖该方法，应自行决定如何体现该配置或协议等价物。
/// - `Range` 请求头值由执行器按当前分片生成，格式为 HTTP Range 单元字符串，例如 `bytes=0-1048575` 或 `bytes=0-`（空总长度时的退化形式）；
///   实现**不应**随意改写起止含义，除非协议明确要求（此时须在文档中说明并与服务端一致）。
/// - [`BreakpointDownload::total_size_from_head`] 失败时，准备阶段失败，任务进入错误路径（可能重试，取决于上层配置）。
pub trait BreakpointDownload: Send + Sync {
    /// 返回本次 **HEAD** 请求应使用的完整 URL。
    ///
    /// 用于在准备阶段探测远端资源长度。默认实现返回 [`TransferTask::url`]，适用于「HEAD 与 GET 同址」的常见对象存储或静态资源；
    /// 若协议要求 HEAD 落在单独端点（例如带不同 path 或 query），应覆盖本方法。
    ///
    /// # 参数
    ///
    /// - `self`：协议实现（如 [`crate::http_breakpoint::StandardRangeDownload`] 或自定义类型）。
    /// - `task`：当前下载任务快照，含业务 URL、方法、头、本地路径等；实现通常至少读取 `task.url()`，也可读取其它 getter 拼出 HEAD 地址。
    ///
    /// # 返回值
    ///
    /// - 合法 URL 字符串，将被执行器直接传给 HTTP 客户端的 `HEAD` 请求。
    fn head_url(&self, task: &TransferTask) -> String {
        task.url().to_string()
    }

    /// 在发送 **HEAD** 之前，将本协议需要的专用请求头合并进 `base`。
    ///
    /// `base` 在执行器侧已由 [`TransferTask`] 的头部克隆而来；本方法应**向 `base` 插入或更新**键值，而不是替换整张表，
    /// 以便保留调用方在任务上配置的通用头（鉴权、`User-Agent` 等）。默认实现不修改任何头。
    ///
    /// # 参数
    ///
    /// - `self`：协议实现。
    /// - `task`：当前任务；若 HEAD 需要与 GET 不同的业务参数（如版本号、桶名），可从此处读取并写入 `base`。
    /// - `base`：即将用于 `HEAD` 请求的 [`HeaderMap`]，可变引用；实现应只追加/覆盖与本协议相关的项。
    fn merge_head_headers(&self, _task: &TransferTask, _base: &mut HeaderMap) {}

    /// 在发送 **带 Range 的分片 GET** 之前，将本协议需要的专用请求头合并进 `base`。
    ///
    /// 默认实现会写入：
    ///
    /// - `Range`：值为参数 `range_value`（执行器生成的单元范围字符串）；
    /// - `Accept`：优先取任务所带断点下载 HTTP 配置中的 `range_accept`（入队时由
    ///   [`crate::meow_config::MeowConfig`] 或 builder 写入的 [`crate::http_breakpoint::BreakpointDownloadHttpConfig`]），
    ///   若未设置则与标准实现一致，为 `application/octet-stream`。
    ///
    /// 覆盖本方法时，若仍希望支持调用方配置的 Accept，请读取同一套
    /// [`crate::http_breakpoint::BreakpointDownloadHttpConfig::range_accept`] 语义（与默认实现保持一致）。
    ///
    /// # 参数
    ///
    /// - `self`：协议实现；默认实现中未使用 `self`，仅为保持 trait 对象统一签名。
    /// - `task`：当前任务；用于解析断点下载 HTTP 配置及实现自定义头逻辑时读取 URL/头等元数据。
    /// - `range_value`：执行器构造的 [`Range`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Range) 字段**完整取值**，
    ///   形如 `bytes=<first>-<last>` 或 `bytes=<first>-`（例如 `bytes=0-1048575`）；实现应将其原样写入（或通过网关翻译为协议等价头），
    ///   除非文档明确说明与标准 Range 语义不同。
    /// - `base`：即将用于本次 `GET` 的 [`HeaderMap`]，由任务头克隆而来；在此追加 `Range`、`Accept` 等分片下载专用头。
    fn merge_range_get_headers(
        &self,
        task: &TransferTask,
        range_value: &str,
        base: &mut HeaderMap,
    ) {
        let _ = self;
        crate::http_breakpoint::insert_header(base, "Range", range_value);
        let accept = task
            .breakpoint_download_http()
            .map(|c| c.range_accept.as_str())
            .unwrap_or(crate::http_breakpoint::DEFAULT_RANGE_ACCEPT);
        crate::http_breakpoint::insert_header(base, "Accept", accept);
    }

    /// 从 **HEAD** 成功响应的头字段中解析远端资源的总字节数。
    ///
    /// 默认实现要求存在合法且大于 0 的 `Content-Length`，否则返回
    /// [`InnerErrorCode::MissingOrInvalidContentLengthFromHead`] 对应的 [`MeowError`]。
    /// 对象存储或 CDN 若对 HEAD 返回其它长度表示方式（例如自定义头、`x-*-size`），应覆盖本方法并返回与 GET 分片一致的总大小。
    ///
    /// # 参数
    ///
    /// - `self`：协议实现。
    /// - `headers`：**HEAD** 响应头（而非 GET）；实现应只从中读取长度相关字段，避免依赖响应体（HEAD 通常无 body）。
    ///
    /// # 返回值
    ///
    /// - `Ok(n)`：`n` 为资源总字节数，且应满足 `n > 0`（默认实现会拒绝 0）；执行器用它与本地文件长度比较以决定续传起点或已完成。
    /// - `Err`：无法确定有效总长度；执行器将中止准备阶段并按错误处理。
    fn total_size_from_head(&self, headers: &HeaderMap) -> Result<u64, MeowError> {
        headers
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| n > 0)
            .ok_or_else(|| {
                MeowError::from_code_str(
                    InnerErrorCode::MissingOrInvalidContentLengthFromHead,
                    "missing or invalid content-length from HEAD",
                )
            })
    }
}
