# rusty-cat 阿里云 OSS 与 Azure Blob Storage 功能扩展清单与必要性评估

> 初次调研：2026-09-02；筛查复核：2026-09-03；对象：公共 crate `rusty-cat` 0.3.6。事实依据来自当前仓库源码、项目 README、阿里云 OSS 与 Microsoft Azure Blob Storage 官方文档；评估方法采用用户指定 `qa-skills/SKILL.md` 的“主张视为假设、区分事实与推断、寻找替代方案和反转证据”原则。

## 1. 结论

**最终判断：不应把 `rusty-cat` 扩成阿里云 OSS 或 Azure Blob Storage 的全量 SDK。** 它目前明确定位为可恢复文件上传/下载执行器，应用负责业务记录、凭证、权限和 provider setup。现阶段应立即投入已有传输承诺的正确性与安全性；跨进程恢复和很窄的 `UploadMode::Auto` 小文件路径值得继续验证需求。对象 CRUD、控制面和数据处理能力不能因为官方“存在 API”就默认立项。

筛查后的建议分为五档：

- **A，核心必做**：不实现会造成数据混合、误删、凭证泄露、虚假成功，或直接违背“并行/断点/重试”公开承诺；
- **B，建议窄实现**：直接改善常见上传下载，但只公开传输所需的最小类型和操作；
- **C，需求门控**：能力确实可能有用，但必须先有真实生产场景、维护者和真实云契约测试；
- **D，不纳入 rusty-cat**：由 OpenDAL、云厂商 SDK、IaC/控制台或业务服务完成更安全，或其状态机与普通文件传输根本不同；
- **E，独立实验 crate**：密码学、WORM 等高风险能力，只有完成安全/互操作审查后才可能提供。

第 13 章是本次筛查后的**唯一纳入结论**。第 4～6 章保留为官方能力和技术约束目录，其中旧有 P0～P3 只代表最初候选排序，不能再解释成开发承诺。

初版调研发现的主要缺口如下；它们并非同一优先级，哪些属于安全热修、哪些需要需求证据，以第 9 章和 13.1 为准：

- 三种云下载协议当前没有启用同文件并行 Range，配置 `max_parts_in_flight > 1` 也仍是串行下载；
- 阿里云 `ListParts`、`ListMultipartUploads` 未分页，超过 1,000 条会漏数据；
- 阿里云 direct 下载没有把 URL 查询参数纳入 V4 签名，无法可靠扩展 `versionId` 等功能；
- direct 上传没有合并 task 自定义 headers，因此元数据、标签、加密、存储类型、条件写等选项目前实际无法传入；
- 两家 direct 上传都缺少请求级 MD5/CRC64 校验与最终对象校验；
- Azure 取消上传会直接 `DELETE` 目标 Blob，可能误删上传前已经存在的对象；
- direct 模式只支持长期 AccessKey/Shared Key，应允许应用注入可刷新的 STS/Bearer 凭证；核心不负责登录云平台或主动访问 IMDS/OIDC。

对象列举、删除、复制、标签等对某些产品当然有用，但当前没有足够证据证明使用者希望 `rusty-cat` 同时成为通用对象客户端。控制面、网站、媒体处理、查询、事件、HNS/Page Blob 等则明确不属于现有产品边界。

## 2. 名词与范围

- 本文中的“微软云 OSS”统一称为官方名称 **Azure Blob Storage**。
- “新增功能”分为四类：
  - **传输内核**：影响分块调度、校验、重试、续传和取消语义；
  - **对象数据面**：上传、下载、列举、复制、删除、元数据等；
  - **认证与网络**：临时凭证、预签名、endpoint、限速和诊断；
  - **控制面**：桶/容器、生命周期、版本开关、复制、合规策略等。
- 本次评估分别回答两个问题：
  1. **功能本身是否有用**；
  2. **是否应由 `rusty-cat` 实现**。前者为“是”不自动推出后者为“是”。
- 第 4～6 章表中的 P0～P3 是初版调研留下的候选紧迫度，已被第 13 章的 A～E 决策覆盖。
- 三种职责边界：
  - **执行**：`rusty-cat` 负责可靠传输、恢复、重试、校验和取消；
  - **兼容**：`rusty-cat` 能使用应用已配置的 endpoint、凭证、对象 revision 或传输属性；
  - **管理**：bucket/container、策略、证书、账户和数据处理任务由其他工具负责。

### 2.1 证据强度与未知项

**已核实事实**：项目 README 明确把库定义为“resumable file upload and download”，并把凭证、权限和 provider setup 交给应用；源码存在两家云的分片传输实现，但 provider 下载尚未启用同文件并行，Azure direct cancel 会删除目标 Blob。

**需求信号而非结论**：截至 2026-09-03，crates.io 下载量和 GitHub issue 数量仍较小，公开 issue 中也没有云功能请求。这只能说明目前缺少可观察的需求证据，不能证明没有需求；项目较新、用户也可能通过私有渠道反馈。

**替代方案已存在**：Apache OpenDAL 已覆盖 OSS、Azure Blob 以及通用 `read/write/stat/list/delete` 等对象操作；Microsoft 也已提供稳定版 Azure SDK for Rust。重复这些数据面/管理面能力会增加兼容、权限、测试和长期维护成本，而不会强化 `rusty-cat` 的独特价值。

**仍需验证**：真实对象大小分布、direct 与 presigned 使用比例、跨进程恢复需求、是否有人需要同步/备份对象层、真实云失败率与吞吐瓶颈。没有这些数据时，第 13 章 C 类能力只能保持假设状态。

## 3. 当前已有能力基线

以下功能已经存在，不应在路线图中重复当作全新能力：

| 模式 | 已有能力 | 当前边界 |
|---|---|---|
| 阿里云 direct | OSS V4 Header 签名；`InitiateMultipartUpload`、`UploadPart`、`ListParts`、`CompleteMultipartUpload`、`AbortMultipartUpload`；同文件并行上传；暴露当前 `UploadId` | 只接受长期 AK/SK；跨进程不能显式注入旧会话；列表不分页；完成阶段使用 `x-oss-complete-all`；自定义上传 headers 不生效 |
| 阿里云 presigned | 每分片 URL/headers/过期时间；URL 刷新器；已上传分片导出与重新注入；完成/取消回调 | SDK 只消费 URL，不生成 V4 预签名 URL；provider 约束和 ETag 校验仍不完整 |
| Azure direct | Shared Key 签名；`Get Block List`、`Put Block`、`Put Block List`；50,000 blocks/4,000 MiB 单块限制；并行上传 | 固定 REST 版本 `2023-11-03`；只支持 Shared Key；跨进程会话隔离不足；取消会删除整个目标 Blob |
| Azure SAS | 生成 `Put Block`/`Put Block List` 请求描述；每分片 URL/headers/刷新；已上传分片重新注入 | SDK 不生成 SAS；未完整校验 Azure block 约束；取消语义依赖调用方提供的请求 |
| 通用下载器 | HEAD + Range GET；严格校验 206、`Content-Range` 和响应长度；强 ETag + `If-Match` 防止混合不同版本；`.rusty-cat/*.rcdl` 保存 SHA-256 分片摘要并支持跨进程恢复 | Ali direct、Azure direct、presigned 三种云协议均未声明 `supports_parallel_parts()`；presigned 下载也未提供稳定 `resume_identity()` |
| 通用执行器 | 文件和内存 Bytes 上传；暂停、恢复、取消、关闭；任务快照；进度与日志；指数退避；同文件并行上传和内存预算 | 重试不识别 OSS/Azure 结构化错误、服务端 request ID、`Retry-After` 或 provider 限流语义 |

当前源码依据主要位于：

- `rusty-cat/src/aliyun-oss-direct/`
- `rusty-cat/src/aliyun-oss-presigned/`
- `rusty-cat/src/azure-blob-direct/`
- `rusty-cat/src/azure-blob-sas/`
- `rusty-cat/src/presigned/`
- `rusty-cat/src/dflt/`

## 4. 传输可靠性候选能力目录

本章是技术问题目录，不再表示“全部 P0”。其中 endpoint/签名边界、安全取消、会话隔离、provider 限制、零字节、有界解析和不确定结果语义属于必须修复；并行性能、公开 checkpoint、完整 checksum 策略、身份适配器等需要拆分判断。源码已经具备的源文件代际校验和统一内存预算只保留为回归要求，不应重复计为新增功能。最终分级见 13.1。

### 4.1 真正的并行 Range 下载

**可增加的功能**

- 为 `AliOssDirectDownload`、`AzureBlobDirectDownload`、`PresignedRangeDownload` 实现并开启 `supports_parallel_parts()`；
- 为每个 Range 请求独立构造 URL 和最终 headers，允许乱序返回后按绝对偏移写入；
- OSS Range 请求默认加入并签名 `x-oss-range-behavior: standard`：当起始位置越界时返回 `416 InvalidRange`，而不是忽略 Range 后以 `200` 返回整个对象；末端越界时按对象尾部裁剪；
- 保留现有强 ETag/`If-Match` 代际绑定，任一分片 ETag 缺失或变化时整次任务失败，不能拼接不同对象版本；
- presigned 模式增加稳定 `resume_identity`，只把不随换签变化的资源身份、语义 headers、版本/快照 ID 纳入 checkpoint。

**云端依据**

- OSS `GetObject` 支持 Range 与条件请求；Range 边界行为见 [使用 Range 分段获取 OSS 资源](https://help.aliyun.com/zh/oss/how-to-obtain-oss-resources-by-segmenting-http-range-requests)，条件请求见 [OSS 条件下载](https://www.alibabacloud.com/help/en/oss/user-guide/conditional-download)。
- Azure `Get Blob` 支持 `Range`/`x-ms-range`、快照和版本；[Get Blob](https://learn.microsoft.com/en-us/rest/api/storageservices/get-blob)。

**验收重点**

- 乱序完成、重复响应、短响应、200 伪装 Range、416、ETag 中途变化、断网重启均必须有测试；
- signed URL 刷新后，资源身份不能改变；
- 并行数改变不能让已落盘 checkpoint 失效，chunk size 改变则必须创建新分片网格。

### 4.2 direct 上传的真正跨进程续传

初版提出以下 provider-neutral checkpoint 外壳；本次结论是先完成会话隔离，公共 schema 通过 Gate B 后再定稿：

```rust
pub struct UploadCheckpointV1 {
    pub schema_version: u16,
    pub provider: CloudProvider,
    pub normalized_endpoint: String,
    pub namespace: String,
    pub object_key: String,
    pub provider_state: ProviderUploadStateV1,
    pub source_generation: SourceGeneration,
    pub total_size: u64,
    pub chunk_size: u64,
    pub options_fingerprint: String,
    pub completed_parts: Vec<CompletedCloudPart>,
    pub destination_precondition: Option<WriteCondition>,
}

pub enum ProviderUploadStateV1 {
    AliOss {
        upload_id: String,
    },
    AzureBlockBlob {
        block_namespace: String,
        base_revision: Option<AzureObjectRevision>,
    },
}

pub struct SourceGeneration {
    pub whole_file_sha256: Option<[u8; 32]>,
    pub complete_part_sha256: Vec<[u8; 32]>,
}

pub struct CompletedCloudPart {
    pub number: u32,
    pub offset: u64,
    pub size: u64,
    pub source_sha256: [u8; 32],
    pub receipt: ProviderPartReceipt,
}

pub enum ProviderPartReceipt {
    AliOss { etag: String, crc64: Option<u64> },
    AzureBlockBlob { block_id: String, checksum: Option<String> },
}
```

checkpoint 应由调用方持久化，SDK 提供导出、导入和远端 reconcile API，不应把数据库强行内置到核心 crate。`schema_version` 必须参与反序列化迁移；provider 状态和远端 receipt 必须使用枚举，不能用一个含义模糊的字符串同时代表 OSS UploadId 与 Azure block namespace。`normalized_endpoint` 只保存 scheme/authority/base path 等非敏感身份，不得带 userinfo、query、SAS 或预签名参数。

跨进程恢复必须继承现有下载器的强代际保证：checkpoint 要么包含完整文件 SHA-256，要么包含覆盖**全部** chunk grid 的强分块摘要；仅依赖 inode/file ID、长度和 mtime 不足以证明内容相同。某个 part 只有在服务端响应被完整校验后，才能连同该 part 的源 SHA-256 和 provider receipt 一起原子发布到 checkpoint。

**阿里云 OSS**

- 允许显式注入已持久化的 `UploadId`，禁止只按 object key 猜测会话；
- 对 `ListParts` 使用 `part-number-marker` 循环分页，并解析 `PartNumber`、`ETag`、`Size`、`LastModified`；
- 对 `ListMultipartUploads` 使用 `key-marker` + `upload-id-marker` 分页；
- 本地 checkpoint 与远端清单必须同时匹配源文件强代际摘要、总长度、chunk grid、对象 key、upload options；
- 完成时默认提交本地记录的 `(PartNumber, ETag)` 列表；只有显式选择时才使用 `x-oss-complete-all`。选择 `x-oss-complete-all: yes` 时请求体必须为空，而且 OSS 无法发现遗漏或仍在上传的 part，因此不适合作为可靠续传的默认完成方式。

阿里云官方说明 `ListParts` 每次最多返回 1,000 个 part，并明确建议以本地记录的 part number 和 ETag 构造完成清单，不能仅信任远端列表：[ListParts](https://www.alibabacloud.com/help/en/oss/developer-reference/listparts)。完整 multipart 生命周期见 [Multipart upload](https://www.alibabacloud.com/help/en/oss/user-guide/multipart-upload)。

**Azure Blob Storage**

- block ID 加入每次逻辑上传的固定长度 session 前缀，避免两个进程上传同一个 Blob 时相互覆盖；Base64 编码前的原始 ID 不超过 64 bytes，同一 Blob 传给 `blockid` 的 Base64 字符串等长并做 URL 编码；
- `Get Block List?blocklisttype=all` 同时解析 committed/uncommitted block 的 `Name` 和 `Size`，并与 checkpoint 校验；
- 允许从 checkpoint 恢复确定的 block ID，只上传缺失 block；
- 记录开始上传前目标 Blob 是否存在及其 ETag/version ID，用于提交和取消阶段的条件保护。

Azure `Get Block List` 可以返回 committed、uncommitted 或两者，响应中包含 block name 与 size：[Get Block List](https://learn.microsoft.com/en-us/rest/api/storageservices/get-block-list)。

### 4.3 请求级与对象级完整性校验

| 能力 | 阿里云 OSS | Azure Blob Storage |
|---|---|---|
| 上传分片请求校验 | 计算并发送 `Content-MD5`，服务端不一致时返回 `InvalidDigest` | 每个 `Put Block` 发送 `Content-MD5` **或** `x-ms-content-crc64`，两者不能同时发送 |
| 上传分片响应校验 | 校验响应 `ETag`、`Content-MD5`、`x-oss-hash-crc64ecma`，不能只检查 ETag 是否存在 | 对照响应 `Content-MD5` 或 `x-ms-content-crc64`；`x-ms-request-server-encrypted` 只证明服务端加密状态，不是内容 checksum |
| 完成请求校验 | 完成 XML 必须包含准确的 part number + ETag；multipart 对象 ETag 不能当作整个文件 MD5 | 为 `Put Block List` XML 本身提供 MD5/CRC64；可设置 `x-ms-blob-content-md5` 作为对象属性，但该字段不会在 commit 时重新验证整对象 |
| 下载分片校验 | OSS Range 响应中的 `x-oss-hash-crc64ecma` 是**整个对象**的 CRC64，不能直接校验单个 Range；分片继续使用本地 SHA-256 摘要，只有下载完整对象时才在完成后以 CRC64/XZ 校验完整文件，用户只下载局部范围时不能比较整对象 CRC | Range 不超过 4 MiB 时可请求 `x-ms-range-get-content-md5: true` 或 `x-ms-range-get-content-crc64: true`；两者不能同时用 |
| 最终对象校验 | 完成后 `HeadObject/GetObjectMeta`，比较长度、版本 ID、可获得的 CRC64/业务 SHA-256 metadata | 完成后 `Get Blob Properties`，比较长度、ETag/version ID、Content-MD5 或业务 SHA-256 metadata |

OSS 校验值的编码必须区分：`Content-MD5` 是 MD5 原始 16 bytes 的 Base64；UploadPart `ETag` 是带引号的十六进制 MD5，提交时应保留服务端返回值；`x-oss-hash-crc64ecma` 是 CRC64/XZ 的十进制无符号整数文本。multipart 最终 ETag 不是整对象 MD5。

OSS CRC64 还存在“服务端未提供”的合法分支：CRC64 功能上线前的老对象可能没有该字段；`CopyObject`/`UploadPartCopy` 生成的对象或 part 不保证有 CRC64；只要某个 part 缺 CRC64，complete 就可能不返回整对象 CRC64。因此校验结果至少应是三态：

```rust
pub enum VerificationStatus {
    Verified,
    Mismatch,
    NotProvidedByService,
}
```

`IntegrityRequirement::Required` 可以把 `NotProvidedByService` 视为失败；`BestEffort` 则应明确返回未验证，不能把 header 缺失误报为数据损坏或静默声称校验通过。

官方依据：

- [OSS UploadPart](https://www.alibabacloud.com/help/en/oss/developer-reference/uploadpart) 返回 part 的 MD5、CRC64，并支持请求 `Content-MD5` 防传输错误；
- [OSS CRC64 完整性校验](https://help.aliyun.com/zh/oss/user-guide/check-data-transmission-integrity-by-using-crc-64) 说明算法为 CRC64/XZ，且 Range GET 返回的是整个对象而不是该 Range 的 CRC64；
- [OSS CompleteMultipartUpload](https://www.alibabacloud.com/help/en/oss/developer-reference/completemultipartupload) 要求按序提交 part number 与 ETag，并说明 multipart 对象 ETag 不是整个对象 MD5；
- [Azure Put Block](https://learn.microsoft.com/en-us/rest/api/storageservices/put-block) 支持 transactional MD5 或 CRC64；
- [Azure Get Blob](https://learn.microsoft.com/en-us/rest/api/storageservices/get-blob) 支持不超过 4 MiB 的 Range MD5/CRC64；
- [Azure Put Block List](https://learn.microsoft.com/en-us/rest/api/storageservices/put-block-list) 区分“完成请求体校验”和“Blob 内容 MD5 属性”。

初版候选 API 如下；本次不建议一次固化全部枚举，先按已证明的故障模型提供最小策略：

```rust
pub enum TransferChecksum {
    None,
    Md5,
    Crc64,
    Sha256Metadata { key: String },
}

pub enum IntegrityScope {
    TransportOnly,
    VerifyEveryPart,
    VerifyEveryPartAndFinalObject,
}

pub enum IntegrityRequirement {
    Required,
    BestEffort,
    Disabled,
}

pub struct IntegrityPolicy {
    pub scope: IntegrityScope,
    pub requirement: IntegrityRequirement,
}
```

### 4.4 类型安全的上传、下载选项

当前 direct provider 自己新建 header map，没有完整继承 builder headers。应先建立 provider 专属配置，再允许受控的额外 headers；签名必须覆盖最终实际发出的 headers。

**阿里云初版候选类型（字段需分别通过 13.1/13.2 的 gate）**

```rust
pub struct AliOssTransferOptions {
    pub content_type: Option<String>,
    pub cache_control: Option<String>,
    pub content_disposition: Option<String>,
    pub content_encoding: Option<String>,
    pub expires: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub write_condition: Option<WriteCondition>,
}
```

这里只保留 B 类的 HTTP 属性、metadata 和最小写条件。tags、storage class、加密、request payer、traffic limit、callback 等各自保持 C，不能借一个 options 结构默认获批。选项必须在正确 multipart 阶段一致传递；官方字段见 [InitiateMultipartUpload](https://www.alibabacloud.com/help/en/oss/developer-reference/initiatemultipartupload)。

**Azure 初版候选类型（字段需分别通过 13.1/13.5 的 gate）**

```rust
pub struct AzureBlockBlobTransferOptions {
    pub http_headers: BlobHttpHeaders,
    pub metadata: BTreeMap<String, String>,
    pub lease_id: Option<String>,
    pub conditions: BlobRequestConditions,
}
```

这里只保留 B 类的 HTTP 属性、metadata、最小条件和 caller-supplied lease ID。tags、tier、Encryption Scope、CPK、immutability 和 legal hold 各自保持 C/D/E。`Put Block` 与 `Put Block List` 的 headers 不同，获批字段必须在正确阶段一致设置；官方字段见 [Put Block](https://learn.microsoft.com/en-us/rest/api/storageservices/put-block) 与 [Put Block List](https://learn.microsoft.com/en-us/rest/api/storageservices/put-block-list)。

### 4.5 条件写、幂等与并发控制

**阿里云 OSS**

- multipart 防覆盖模式必须在 `InitiateMultipartUpload` 和 `CompleteMultipartUpload` **两个阶段**都发送 `x-oss-forbid-overwrite: true`，分别覆盖初始化时检查与初始化后发生的并发竞态；
- 对下载和复制支持 `If-Match`、`If-None-Match`、修改时间条件；
- 保存并返回 `x-oss-version-id`，让调用方确认最终写入的是哪个版本；
- 注意：bucket 已启用或暂停 versioning 时，`x-oss-forbid-overwrite` 会被忽略；高 QPS 使用该 header 还可能降低性能。

**Azure Blob Storage**

- commit 时支持 `If-Match`、`If-None-Match: *`、`If-Unmodified-Since` 等条件；
- 支持 `x-ms-if-tags` 标签条件；
- 目标有 active lease 时要求 `x-ms-lease-id`；
- 写结果返回 ETag/version ID，所有结果对象都应向调用方暴露这些字段。

官方依据：[OSS CompleteMultipartUpload](https://www.alibabacloud.com/help/en/oss/developer-reference/completemultipartupload)、[Azure 条件 headers](https://learn.microsoft.com/en-us/rest/api/storageservices/specifying-conditional-headers-for-blob-service-operations)、[Lease Blob](https://learn.microsoft.com/en-us/rest/api/storageservices/lease-blob)。

若最小条件 API 通过 Gate B，不使用模糊的 `overwrite: bool`，先只明确表达：

```rust
pub enum WriteCondition {
    Overwrite,
    CreateOnly,
    MatchEtag(String),
}
```

时间、标签条件和 lease lifecycle 分别按 C 类评估；caller-supplied Azure lease ID 可作为 provider extension，而不是通用枚举分支。

### 4.6 临时凭证与自动刷新

核心只定义异步 credential-provider 生命周期、过期时间和刷新并发控制；真正调用 STS、IMDS、Managed Identity 或 OIDC 的获取器是可选适配器，避免把所有身份 SDK 拖入默认依赖。

本次筛查进一步收窄：可刷新 credential source 为 B；具体获取器是 C 且优先外置；user delegation key、OSS 预签名和 SAS 生成均为 D，不纳入当前核心。

**阿里云 OSS**

- 增加 `AliOssCredentialProvider` trait，返回 `AccessKeyId + AccessKeySecret + SecurityToken + expires_at`；
- 独立 adapter 候选可支持 STS `AssumeRole`、ECS RAM Role/IMDSv2、OIDC 和环境变量；核心只接受自定义 provider；
- 使用 STS 时在 Header 签名中加入 `x-oss-security-token`，预签名 URL 中加入同名 query；
- 并发请求共享 single-flight 刷新，临近过期提前刷新；正在执行的请求使用同一个不可变 credential snapshot。

IMDS/metadata credential 适配器必须固定官方元数据 endpoint、禁止重定向、默认绕过系统代理并使用短连接/读取超时；metadata 专用 header、token 和返回的凭证不得转发到其他 origin。

官方依据：[STS 临时凭证访问 OSS](https://www.alibabacloud.com/help/en/oss/developer-reference/use-temporary-access-credentials-provided-by-sts-to-access-oss)、[OSS V4 预签名](https://www.alibabacloud.com/help/en/oss/developer-reference/add-signatures-to-urls)。

**Azure Blob Storage**

- 增加 bearer-token credential provider；可选适配器支持 Microsoft Entra ID、service principal、workload identity、managed identity 和用户自定义 token source；
- direct 请求使用 OAuth 2.0 Bearer，而不是要求 storage account Shared Key；
- user delegation key 与 SAS 生成不属于 credential core；如未来建立独立 presigner crate，再单独评估；
- Shared Key 仅保留为兼容模式，secret 类型必须禁止 `Debug` 明文输出，并尽可能使用可清零内存。

Microsoft 官方推荐使用 Entra ID + managed identity；需要 SAS 时优先 user delegation SAS：[Authorize Blob access with Microsoft Entra ID](https://learn.microsoft.com/en-us/azure/storage/blobs/authorize-access-azure-active-directory)、[Create a user delegation SAS](https://learn.microsoft.com/en-us/rest/api/storageservices/create-user-delegation-sas)。

### 4.7 provider 错误、重试和可观测性

本节只有结构化错误、request ID、provider 重试分类、总预算与取消优先级属于 A；endpoint 熔断和完整 tracing 集成是 C 类候选。

可增加：

- 解析 OSS/Azure XML 错误为结构化类型，而不是只返回状态码和字符串 body；
- 统一暴露 `provider_code`、`request_id`、`host_id`、`service_version`、HTTP status、可重试分类；
- 阿里云记录 `x-oss-request-id`、`x-oss-server-time`；Azure 记录 `x-ms-request-id`、`x-ms-version`、`x-ms-client-request-id`；
- 每次请求自动设置 SDK 自己的 client request ID，贯通 task → part → provider 日志；
- 识别 `Retry-After`、429、服务忙、网络超时、签名时间偏差；若响应存在其他 provider 重试提示则兼容处理，但不把非 Blob 服务字段写成固定协议保证；
- 区分安全重试与非幂等重试，completion 超时后先查询最终对象/会话状态，不能盲目开始新任务；
- A 类补齐 jitter、最大总重试时长和用户取消优先级；endpoint 级熔断仅保留为 C 类宿主策略候选。

建议返回：

```rust
pub struct CloudResponseMeta {
    pub request_id: Option<String>,
    pub client_request_id: Option<String>,
    pub service_version: Option<String>,
    pub etag: Option<String>,
    pub version_id: Option<String>,
    pub checksum: Option<String>,
}
```

### 4.8 安全的取消与孤儿清理

安全 cancel/已知会话 abort 属于 A；全前缀扫描和按年龄批量清理不属于核心，以下相应条目仅保留为运维候选。

**阿里云 OSS**已有显式 `AbortMultipartUpload`，应保留其回归测试，并评估以下生命周期补强：

- 提供显式异步 `abort().await` / `close().await`，由调用方确认远端清理结果；Rust `Drop` 无法等待网络请求，最多只能触发 best-effort 信号，不能承诺清理成功；
- 核心不批量列出或按创建时间清理孤儿 UploadId；只有独立运维工具具备可靠 owner namespace 时才重新评估；
- 推荐用户配置 lifecycle 的“清理未完成 multipart upload”规则，作为进程崩溃、runtime 关闭时的最终兜底；
- 取消失败返回可重试 cleanup handle，不应只写日志。

**Azure Block Blob 没有与 OSS 等价的“只中止本次 block 会话”API**。未提交 blocks 会在一段时间无活动后由服务清理；官方文档说明 `Put Block` 创建的未提交状态通常保留一周。当前直接删除目标 Blob 的做法应改为显式策略：

```rust
pub enum AzureAbortPolicy {
    LeaveUncommittedBlocks,
    DeleteExclusiveTemporaryBlob(ExclusiveTempBlobHandle),
}
```

- 默认 `LeaveUncommittedBlocks`，优先保护原有 committed Blob；
- 自动删除只允许针对 SDK 为本任务生成的、随机且独占的临时 Blob 名称；`ExclusiveTempBlobHandle` 不能由普通用户目标 key 直接构造；
- 普通目标 Blob 的删除是独立对象操作，必须由调用方显式请求并提供相应条件，不能包装成“abort”；仅凭任务开始前的 HEAD/ETag 无法原子证明当前对象属于本任务；
- 将 session 前缀写入 block ID，以便后续诊断哪些 blocks 属于哪个逻辑任务。

依据：[Azure Put Block 的未提交 block 生命周期](https://learn.microsoft.com/en-us/rest/api/storageservices/put-block)。

### 4.9 其他必须先补齐的协议边界

| 能力 | 要解决的问题 | 建议实现与验收 |
|---|---|---|
| endpoint 与资源身份强校验 | 当前 client 配置的 bucket/account 与任务 URL 绑定不够强，签名实现还可能对调用方传入的任意 host 发出授权信息 | 默认只允许 HTTPS 和已配置 endpoint；自定义域、Azurite、HTTP 模拟器必须显式启用；region/account/bucket/container 与 URL 必须一致；跨 origin 重定向不得转发 `Authorization`、SAS、STS token 或客户密钥 |
| “先完成请求、最后签名”管线 | query、headers 在签名之后再变化会导致签名错误，或让 metadata、条件、加密字段未被认证 | URL、canonical query、受控 headers、body hash 全部冻结后调用 provider signer；禁止调用方覆盖 `Host`、`Authorization`、日期、Range、checksum、lease 等 SDK 管理字段 |
| OSS part 限制预检 | OSS part number 范围为 1–10,000；非末片至少 100 KB；单 part 最大 5 GB | 网络请求前使用 checked arithmetic 校验；覆盖 99 KB、100 KB、5 GB、10,000/10,001 parts 和超大文件溢出测试；见 [UploadPart](https://www.alibabacloud.com/help/en/oss/developer-reference/uploadpart) |
| Azure block ID 约束 | **Base64 编码前的原始 ID**不超过 64 bytes；同一 Blob 传给 `blockid` 的 Base64 字符串必须等长，并且作为 query 值 URL 编码；不同上传者可能相互覆盖 uncommitted blocks | 使用固定长度的随机 session namespace + part index；checkpoint 保存 namespace；恢复只接纳 namespace、大小和 checksum 都匹配的 blocks；见 [Put Block](https://learn.microsoft.com/en-us/rest/api/storageservices/put-block) |
| 零字节对象 | 对象存储允许空对象，但现有任务模型拒绝零长度上传/下载 | `PutObject`/`Put Blob` 显式支持空对象；HEAD 返回 `Content-Length: 0` 是合法成功；空下载创建空文件并生成完成态 checkpoint |
| 有界 XML 解析 | OSS 与 Azure 的列表、错误、complete 响应是 XML；字符串切割会在 namespace、转义字符、分页字段或错误体变化时失效 | 使用流式或有响应体大小上限的 XML parser；正确处理实体转义、缺失可选字段、未知字段和命名空间；错误保留 provider 原始 code 与 request ID |
| complete 结果不确定性对账 | complete/commit 已被服务端接受后，客户端可能因连接中断看不到响应；OSS Complete 即使连接断开也可能继续执行；直接重试、abort 或新建会话会误判状态 | 返回 `Succeeded`、`NotCommitted`、`Indeterminate` 三态；只有任务专属强 checksum/metadata、Azure session block IDs、已知返回 revision，或受条件写保护的结果能证明本任务成功；仅 HEAD、当前 version ID 和相同长度不够，并发 writer 可能写出同长度对象；证据不足必须是 `Indeterminate` |
| 源文件代际校验 | 上传期间本地文件可能被覆盖、截断或替换，最终对象会混合旧、新内容 | checkpoint 保存完整文件 SHA-256 或覆盖全部 chunk grid 的强分块摘要，并辅以 file identity/长度/mtime；上传前、每次恢复及 complete 前复核；远端确认 part 后才原子发布 checkpoint；变化时停止并要求创建新任务 |
| 内存与背压 | 高并发 × 大 part 会把整个文件读入内存 | 并发读取、HTTP body、重试缓存使用统一字节预算；支持 seekable stream/临时 spool；请求队列有界，取消优先于新 part 调度 |

## 5. 阿里云 OSS 官方候选功能目录

阿里云完整对象 API 索引见 [OSS operations by function](https://www.alibabacloud.com/help/en/oss/developer-reference/list-of-operations-by-function)。本章说明功能和协议约束，表中旧 P 值不代表本次批准实现；是否纳入以 13.2～13.4 为准。

### 5.1 对象与传输候选能力

| 初版候选排序（已废弃） | 功能 | 官方 API/字段 | 对开发者的价值 | 实现注意点 |
|---|---|---|---|---|
| P1 | 小对象单请求上传与智能策略 | [`PutObject`](https://www.alibabacloud.com/help/en/oss/developer-reference/putobject) | 小文件不必创建 multipart 会话，减少请求数、延迟和孤儿 part | PutObject 单对象最多 5 GB；multipart 通过最多 10,000 个、单个最多 5 GB 的 part 支持约 48.8 TB 对象；提供 `UploadMode::Auto/Single/Multipart` 并预检限制；仍支持 MD5/CRC64、metadata、tags、加密和条件写 |
| P0 | 公开完整 multipart 会话 API | `InitiateMultipartUpload`、`UploadPart`、`ListParts`、`CompleteMultipartUpload`、`AbortMultipartUpload`、`ListMultipartUploads` | 业务可持久化、恢复、查询和清理任务，不再只能依赖执行器内部状态 | 返回强类型 `UploadId`；全部列表实现 paginator/stream；完成默认使用准确 ETag 清单 |
| P1 | 上传完成回调 | [`Callback`](https://www.alibabacloud.com/help/en/oss/developer-reference/callback)，`x-oss-callback`、`x-oss-callback-var` | OSS 成功写入后通知业务服务，适合订单、素材处理、数据库确认 | 仅 `PutObject`、`PostObject`、`CompleteMultipartUpload` 支持；业务服务须在官方规定的 5 秒窗口内响应；callback 失败不等于对象没写入，OSS 不自动重试；callback 响应会替换原完成响应 body |
| P1 | 获取对象属性、metadata 与 exists | [`HeadObject`](https://www.alibabacloud.com/help/en/oss/developer-reference/headobject)、[`GetObjectMeta`](https://www.alibabacloud.com/help/en/oss/developer-reference/getobjectmeta) | 上传前去重、下载准备、缓存验证、展示对象大小/类型/版本/存储类型 | `exists()` 只把明确的 404 映射为 false；403 不能误判不存在；保留原始 response meta |
| P1 | 列举对象与目录式遍历 | [`ListObjectsV2`](https://www.alibabacloud.com/help/en/oss/developer-reference/listobjectsv2-query) | 文件选择器、同步、备份、批量处理 | 支持 prefix、delimiter、continuation token、start-after、max-keys、encoding-type；单页最多 1,000 项；公开异步 paginator/stream，不能只返回第一页 |
| P1 | 删除单对象与批量删除 | [`DeleteObject`](https://www.alibabacloud.com/help/en/oss/developer-reference/deleteobject)、[`DeleteMultipleObjects`](https://www.alibabacloud.com/help/en/oss/developer-reference/deletemultipleobjects) | 清理临时文件、批量业务删除 | 单批最多 1,000 个对象；请求 XML 带 `Content-MD5`；支持 version ID、quiet 模式、每项结果；版本控制开启时区分 delete marker 与永久删除指定版本 |
| P1 | 服务端复制与大对象分片复制 | [`CopyObject`](https://www.alibabacloud.com/help/en/oss/developer-reference/copyobject)、[`UploadPartCopy`](https://www.alibabacloud.com/help/en/oss/developer-reference/uploadpartcopy) | 不经客户端搬运数据，可用于重命名式操作、备份、同地域迁移 | 源、目标 bucket 必须同 region；跨 bucket 且不改变加密/存储类型时 CopyObject 最多 5 GB，改变加密或存储类型时最多 1 GB，超过后使用 `UploadPartCopy`；同 bucket 且不改变上述属性时 CopyObject 可复制超过 5 GB；支持源 version/条件并提示低频或归档取回费用 |
| P1 | 条件下载与缓存协商 | `If-Match`、`If-None-Match`、`If-Modified-Since`、`If-Unmodified-Since` | 增量同步、避免重复下载、防止传输中对象被替换 | 现有执行器已拥有内部 `If-Match`；公共条件不能覆盖执行器的代际保护，应做合并冲突检查；参考 [条件下载](https://www.alibabacloud.com/help/en/oss/user-guide/conditional-download) |
| P1 | 指定版本下载 | `GetObject?versionId=...` | 回滚、审计、读取历史内容 | V4 canonical query 必须排序并签名；checkpoint identity 必须包含 version ID；需要 `oss:GetObjectVersion` 权限；参考 [下载指定版本](https://www.alibabacloud.com/help/en/oss/developer-reference/download-objects-16) |
| P1 | 对象 metadata、tags、ACL 管理 | `CopyObject`/`x-oss-meta-*`、`Put/Get/DeleteObjectTagging`、`Put/GetObjectACL` | 业务索引、HTTP 缓存、内容类型、访问控制 | metadata 与 tags 是不同概念；单对象最多 10 个标签；复制时明确 `COPY/REPLACE` 语义；ACL API 建议可选并默认鼓励 bucket policy/RAM 最小权限 |
| P1 | 客户端生成 V4 预签名 URL | [V4 presigned URL](https://www.alibabacloud.com/help/en/oss/developer-reference/add-signatures-to-urls) | 后端可直接用 Rust 为不可信客户端签发上传、下载、part、complete/abort URL | 绑定 method、query 和必要 headers；URL 视为 bearer token；AccessKey 最长 7 天，STS URL 还受 token 到期时间约束；支持一次生成惰性 part URL 而非为超大文件一次性分配全部 URL |
| P1 | 单连接服务端限速 | `x-oss-traffic-limit`；[单连接带宽限制](https://www.alibabacloud.com/help/en/oss/user-guide/single-connection-bandwidth-throttling-4) | 控制单请求带宽，避免 SDK 抢占业务网络 | 官方范围为 819,200～838,860,800 bit/s（100 KB/s～100 MB/s）；Header 与 query 同时指定会返回 400；开启后按限速值增大请求超时，避免 SDK 制造假超时；仍保留本地全局/任务级 limiter |
| P1 | transfer acceleration、内网、自定义 endpoint | [Transfer acceleration](https://www.alibabacloud.com/help/en/oss/user-guide/transfer-acceleration)、region internal endpoint、CNAME | 跨地域/跨境加速；同地域云服务走内网；适应 2025 年后中国内地新用户 CNAME 要求 | endpoint 与 signing region 分离；支持 virtual-host/CNAME；HTTPS CNAME 校验证书；加速 endpoint 有额外费用并应支持回退 |
| P2 | 存储类型选择与归档恢复 | `x-oss-storage-class`、[`RestoreObject`](https://www.alibabacloud.com/help/en/oss/developer-reference/restoreobject)、`CleanRestoredObject` | Standard/IA/Archive/ColdArchive/DeepColdArchive 成本管理，以及下载前恢复 | 归档恢复通常是异步状态；提供 `restore()` + `poll_restore_status()`；错误中区分未恢复、恢复中、权限和费用相关限制 |
| P2 | Appendable Object | [`AppendObject`](https://www.alibabacloud.com/help/en/oss/developer-reference/appendobject)、`SealAppendObject` | 日志、媒体流、持续增长文件 | 使用服务端返回的 next append position；用 position 条件避免并发写乱序；与 multipart 是不同协议，应单独 trait/类型 |
| P2 | HTML 表单上传策略 | [`PostObject`](https://www.alibabacloud.com/help/en/oss/developer-reference/postobject) 与 V4 POST policy | 浏览器/表单直传和受限上传 | 提供 policy builder、条件校验和签名数据结构；不要把浏览器表单 multipart 与 OSS multipart upload 混为一谈 |
| P2 | 服务端 SQL 选择性读取 | [`SelectObject`](https://www.alibabacloud.com/help/en/oss/developer-reference/selectobject)、`CreateSelectObjectMeta` | CSV/JSON 等对象只返回查询结果，降低下载量 | 返回异步字节流；单独处理 SQL、输入/输出格式和进度帧；不接入普通 Range checkpoint |
| P2 | 请求者付费 | `x-oss-request-payer: requester` | 访问 requester-pays bucket | 配置必须进入签名；结果和错误应明确费用由请求方承担；默认关闭 |
| P3 | Symbolic link、目录和 Rename | `Put/GetSymlink`、`Create/Rename/DeleteDirectory` | 特定 OSS 文件语义或层级命名空间场景 | 普通 OSS object key 不是 POSIX 文件；必须按 bucket 类型和 API 支持情况隔离，不能承诺通用原子 rename |

### 5.2 阿里云可选控制面能力

这些功能存在于官方 API，但本次评估结论是全部不进入 `MeowClient`；通常由 IaC、控制台或专用 SDK 完成。WORM 即使独立 crate 也要先经过高风险 gate：

| 类别 | 可增加 API | 用途与边界 |
|---|---|---|
| Bucket 基础管理 | `PutBucket`、`DeleteBucket`、`GetBucketInfo`、`GetBucketLocation`、`GetBucketStat` | 部署工具、管理后台需要；终端传输客户端通常不需要 |
| 生命周期与访问监控 | `Put/Get/DeleteBucketLifecycle`、[`Put/GetBucketAccessMonitor`](https://help.aliyun.com/zh/oss/developer-reference/putbucketaccessmonitor) | 自动转存低频/归档、删除历史版本、清理未完成 multipart；支持基于 Last Access Time 的规则；访问时间异步更新，存在 atime 生命周期规则时不能关闭 Access Monitor；策略修改是高权限操作 |
| 版本控制 | `Put/GetBucketVersioning`、`ListObjectVersions` | 数据保护与版本浏览；必须向用户说明存储成本和 delete marker 语义 |
| 默认加密 | `Put/Get/DeleteBucketEncryption` | 统一 SSE-OSS/SSE-KMS 策略；KMS 权限和密钥地域需单独验证 |
| CORS | `Put/Get/DeleteBucketCors`、`Options` | 浏览器直传/下载；规则更新应覆盖整份配置，防止误删现有规则 |
| 传输加速 | `Put/GetBucketTransferAcceleration` | 开关 bucket 加速；数据请求仍需显式使用加速 endpoint |
| Bucket WORM 保留 | [`Initiate/Abort/Complete/Extend/GetBucketWorm`](https://help.aliyun.com/zh/oss/developer-reference/initiatebucketworm) | 初始化后 24 小时内未 Complete 则策略失效；Complete 锁定后不可撤销，只能延长保留期；必须与普通上传 API 隔离并增加显式确认语义 |
| Object WORM（邀测） | `PutBucketObjectWormConfiguration`、`PutObjectRetention`、`x-oss-object-worm-mode`、`x-oss-object-worm-retain-until-date` | 要求 bucket 启用版本控制，与 Bucket WORM 互斥，开启不可逆；只能作为 experimental/high-risk feature，见 [Object WORM](https://help.aliyun.com/zh/oss/user-guide/object-level-retention-policy-object-worm) |
| 数据复制（CRR/SRR） | `Put/Get/DeleteBucketReplication`、进度 API；CRR 另有 RTC | 跨区域/同区域灾备与迁移；是异步控制面任务，不应复用文件上传进度模型；RTC 只适用于支持的 CRR 场景 |
| Inventory 与日志 | Bucket Inventory、Logging、实时日志字段 | 审计、离线盘点、成本分析；结果本身通常写入另一个 OSS object |
| Bucket Policy/ACL/Public Access Block | Policy、ACL、Public Access Block API | 权限治理；高风险且模型复杂，优先交给 IaC 工具，SDK 仅在明确需求时提供 |
| 请求者付费配置 | [`Put/GetBucketRequestPayment`](https://help.aliyun.com/zh/oss/developer-reference/putbucketrequestpayment) | 启用后禁止匿名访问，请求者的 GET/HEAD/POST 等请求按官方要求发送并签名 `x-oss-request-payer: requester`；数据请求选项已在对象层列出 |

### 5.3 阿里云专业化与数据处理能力

这些也是 OSS 官方能力，但受 bucket 类型、地域、计费或其他阿里云产品依赖影响。本节仅保留能力事实；除标准单 Range 外，本次没有能力获准进入近期核心，结论见 13.4：

| 初版候选排序（已废弃） | 功能 | 可增加的 API/用法 | 适用场景与边界 |
|---|---|---|---|
| P1 | Range 与多 Range GET | 单 Range 使用并签名 `x-oss-range-behavior: standard`；多 Range 使用 `Range: bytes=a-b,c-d` 与 `x-oss-multi-range-behavior: multi-range` | 一次最多 50 个不连续范围；响应为 `multipart/byteranges`，需可靠解析 MIME boundary；计费请求数按“调用次数 × 本次 Range 数量”计算，不能把一次 HTTP 调用理解为一次 GetObject 计费；见 [GetObject](https://help.aliyun.com/zh/oss/developer-reference/getobject) |
| P2 | MetaQuery 元数据索引 | `OpenMetaQuery`、`GetMetaQueryStatus`、`DoMetaQuery`、`CloseMetaQuery` | 按对象元数据、标签等条件搜索，适合内容资产库；索引建立和查询有异步/最终一致语义，应与实时 `ListObjectsV2` 分开 |
| P2 | 图片处理 | 在 GET URL/query 使用 `x-oss-process=image/...`，以及样式、签名和响应格式选项 | 缩放、裁剪、旋转、格式转换、水印等；返回的是处理结果而非原对象，checkpoint identity 必须包含完整处理参数；见 [图片处理](https://help.aliyun.com/zh/oss/user-guide/img-implementation-modes) |
| P2 | 媒体、文档与异步数据处理 | `x-oss-process` 同步处理或提交异步处理任务 | 转码、截图、文档预览等通常依赖数据处理服务和独立计费；应返回任务 ID/poller，不复用上传 task 状态 |
| P2 | 事件通知模型 | OSS 事件通知配置/消息数据类型 | 上传、删除、归档恢复等事件驱动业务；配置可能依赖 EventBridge、MNS 或函数计算；不进核心，出现多项目 schema 复用需求后可另立 types crate |
| P2 | 静态网站 | `Put/Get/DeleteBucketWebsite`，index/error page、routing rules | 静态站点部署；网站 endpoint、匿名访问、CORS、Public Access Block 和 HTTPS/CNAME 相互影响 |
| P2 | 自定义域名与证书 | CNAME、证书绑定/解绑/查询 API | 自有域访问 OSS；证书和域名所有权是高权限控制面；数据 client 只需正确支持 CNAME endpoint 和 host 签名 |
| P2 | 防盗链与 TLS 设置 | Referer、`Put/GetBucketHttpsConfig` 等 bucket 配置 | 限定来源站点和 TLS 版本；空 Referer、签名 URL、CDN 回源的行为必须写清楚 |
| P2 | Access Point | 创建/列举/删除接入点，设置接入点策略与 Public Access Block | 为不同应用隔离网络入口和权限；endpoint、ARN 和 policy 均为 provider-specific 模型 |
| P2 | Archive 直接读取 | `Put/GetBucketArchiveDirectRead` | Archive 对象免显式 Restore 的快速读取；仅适用于支持的存储类型/地域，会产生不同费用，必须显式启用 |
| P3 | 向量、LiveChannel、数据加速器 | Vector Bucket API、LiveChannel、Bucket Data Accelerator 等 | 向量检索、直播推流、低延迟缓存等专门产品形态；建议独立 crate/experimental feature，不放进通用 `ObjectStore` |
| P3 | Object FC Access Point 与高级 QoS | 函数计算接入点、Resource Pool QoS 等 | 需要额外云服务、角色和资源配置；核心只保留 endpoint/authorizer 扩展点 |

其中图片/媒体处理和 SelectObject 生成的内容不一定与原对象具有相同长度、ETag 或 checksum，不能直接复用普通对象的 Range 下载 checkpoint。

## 6. Azure Blob Storage 官方候选功能目录

Azure Blob 数据面完整索引见 [Operations on blobs](https://learn.microsoft.com/en-us/rest/api/storageservices/operations-on-blobs)。本章说明功能和协议约束，表中旧 P 值不代表本次批准实现；是否纳入以 13.5～13.7 为准。

### 6.1 对象与传输候选能力

| 初版候选排序（已废弃） | 功能 | 官方 API/字段 | 对开发者的价值 | 实现注意点 |
|---|---|---|---|---|
| P1 | `Put Blob` 小文件上传与智能策略 | [`Put Blob`](https://learn.microsoft.com/en-us/rest/api/storageservices/put-blob) | 小对象单请求写入，减少 stage/commit 请求 | `UploadMode::Auto` 根据大小、是否可重试、流长度决定 Put Blob 或 Put Block；必须指定 Blob 类型；覆盖会替换内容及 metadata |
| P0 | 公开 block 会话原语 | `Put Block`、`Get Block List`、`Put Block List` | 应用可持久化和控制 block 续传/提交 | block ID 定长且带 session namespace；解析 size；支持 committed/uncommitted/latest 三种 commit 来源；不能只暴露 index |
| P2 | 增量 block 更新 | [`Put Block List`](https://learn.microsoft.com/en-us/rest/api/storageservices/put-block-list) 的 `Committed`、`Uncommitted`、`Latest` | 仅上传变化 block，再与已有 committed blocks 组合，可减少大对象局部更新流量 | 必须基于 ETag/lease 条件；不适合内容定义分块不稳定的文件；提交会形成新的 Blob 版本/ETag |
| P1 | 服务端分块拉取 | [`Put Block From URL`](https://learn.microsoft.com/en-us/rest/api/storageservices/put-block-from-url) | Azure 服务端从 URL 读取数据，客户端不必下载再上传 | 支持 source range、source MD5/CRC64、source authorization 和条件；目标 block 仍需最终 `Put Block List` |
| P1 | 从 URL 创建 Blob | [`Put Blob From URL`](https://learn.microsoft.com/en-us/rest/api/storageservices/put-blob-from-url) | 将 HTTP/Azure 来源直接写成 Block Blob，省去客户端中转 | 当前文档限制和可用 headers 受 REST 版本影响；来源较大时改用 Put Block From URL；source URL/SAS 和 authorization 必须脱敏 |
| P1 | 服务端复制及任务控制 | [`Copy Blob`](https://learn.microsoft.com/en-us/rest/api/storageservices/copy-blob)、[`Abort Copy Blob`](https://learn.microsoft.com/en-us/rest/api/storageservices/abort-copy-blob) | 账户内/跨账户复制、备份、迁移 | 返回 copy ID；通过 `Get Blob Properties` 轮询 `pending/success/aborted/failed` 和进度；轮询时核对 copy ID；Abort Copy 会留下零长度目标 Blob 及完整 metadata，不是无痕撤销 |
| P1 | 属性、metadata 与 exists | [`Get Blob Properties`](https://learn.microsoft.com/en-us/rest/api/storageservices/get-blob-properties)、`Set Blob Properties`、`Get/Set Blob Metadata` | 下载准备、缓存、内容类型、复制状态、tier、lease、加密状态 | 结构体区分 HTTP properties、system properties、metadata；自定义 metadata 总量上限 8 KiB，设置是整体替换；只有明确 404 才是不存在；支持 snapshot/version 查询 |
| P1 | Blob index tags 与条件 | [`Get Blob Tags`](https://learn.microsoft.com/en-us/rest/api/storageservices/get-blob-tags)、[`Set Blob Tags`](https://learn.microsoft.com/en-us/rest/api/storageservices/set-blob-tags)、`Find Blobs by Tags`、`x-ms-if-tags` | 可检索业务标签并基于标签做条件写/删除 | 每个 Blob 最多 10 个标签；Set 是整体替换；单 Blob 读取强一致，二级索引查询可能最终一致；tag 与 metadata 不同；权限/SAS 需要 `t`/`f` 等对应 permission |
| P1 | 列举 container/blob | `List Containers`、[`List Blobs`](https://learn.microsoft.com/en-us/rest/api/storageservices/list-blobs) | 浏览、同步、批量任务 | 支持 prefix、delimiter、marker、maxresults、include(metadata/tags/versions/snapshots/deleted/uncommittedblobs)；单页最多 5,000 项，少于上限也可能返回 `NextMarker`，异步 paginator 不能漏页 |
| P1 | 删除 Blob | [`Delete Blob`](https://learn.microsoft.com/en-us/rest/api/storageservices/delete-blob) | 文件删除、清理任务 | 明确 snapshot 处理选项；支持 version ID、lease、ETag/tag conditions；返回软删除与永久删除语义差异 |
| P1 | 版本/快照感知读取 | `Get/Head Blob?snapshot=...` 或 `?versionid=...` | 一致性下载、读取明确历史 revision | `ObjectRevision` 区分 base/snapshot/version；查询参数进入 Shared Key/SAS canonical resource；download checkpoint identity 必须包含完整 revision |
| P2 | 创建快照与历史版本恢复 | [`Snapshot Blob`](https://learn.microsoft.com/en-us/rest/api/storageservices/snapshot-blob)、Copy Blob | 备份、回滚、审计 | API 返回并保留 snapshot/version ID；恢复旧版本通常通过 copy 提升为 current version；生命周期和删除 API 必须区分 base、snapshot 与 version |
| P2 | Soft delete 与恢复 | [`Undelete Blob`](https://learn.microsoft.com/en-us/rest/api/storageservices/undelete-blob)、[Soft delete](https://learn.microsoft.com/en-us/azure/storage/blobs/soft-delete-blob-overview) | 防误删与恢复 | versioning 开启时，undelete 与恢复 current version 的行为不同；Undelete 响应不枚举恢复项，如调用方需要明细，SDK 须在操作前后另行 List 并明确其一致性边界 |
| P2 | Access tier 与归档 rehydrate | [`Set Blob Tier`](https://learn.microsoft.com/en-us/rest/api/storageservices/set-blob-tier)、[Access tiers](https://learn.microsoft.com/en-us/azure/storage/blobs/access-tiers-overview) | Hot/Cool/Cold/Archive 成本优化和归档恢复 | Archive 不能直接读取；rehydrate 是异步的，可选择 priority；轮询 `x-ms-archive-status`/tier；提示提前删除和数据取回费用 |
| P1 | Lease 并发锁 | [`Lease Blob`](https://learn.microsoft.com/en-us/rest/api/storageservices/lease-blob) | 多写者协调、受保护的 commit/delete | 提供 acquire/renew/change/release/break；固定租期为 15～60 秒，`-1` 为无限；活动租约下 Put Block、Put Block List、覆盖和 Delete 需要有效 lease ID，普通 Get 读取不因此被禁止；不要把 lease 自动隐藏在任务内部 |
| P1 | Encryption scope 与客户提供密钥 | `x-ms-encryption-scope`；`x-ms-encryption-key`、`x-ms-encryption-key-sha256`、`AES256` | 合规、租户隔离、BYOK 场景 | CPK 必须对 stage、commit、read 一致提供且绝不能日志/持久化明文；检查 `x-ms-request-server-encrypted` 和返回的 key SHA-256；参考 [Put Block](https://learn.microsoft.com/en-us/rest/api/storageservices/put-block) |
| P2 | Immutability policy 与 legal hold | `x-ms-immutability-policy-*`、`x-ms-legal-hold` | WORM、法规留存 | 一旦 locked 可能不可逆；与覆盖、删除、生命周期冲突；建议单独 feature，并返回服务端最终策略；参考 [Immutable storage](https://learn.microsoft.com/en-us/azure/storage/blobs/immutable-storage-overview) |
| P1 | SAS 生成器 | service SAS、account SAS、user delegation SAS | Rust 后端可签发细粒度临时访问，不必让调用方手拼 query | 默认优先 user delegation SAS；user delegation key 的 Start/Expiry 均须位于当前日期起七天内且最长七天；User Delegation SAS 不支持 Stored Access Policy；类型化 permissions、resource、start/expiry、IP、HTTPS、response overrides；SAS 视为 secret；参考 [User delegation SAS](https://learn.microsoft.com/en-us/rest/api/storageservices/create-user-delegation-sas) |
| P1 | 可配置 REST service version 与 endpoint | `x-ms-version`、Azure Global/Government/China、自定义 endpoint、Azurite | 兼容主权云、模拟器和新特性，不被固定 `2023-11-03` 限死 | 用受支持版本枚举 + escape hatch；功能在发送前校验最低版本；account name、endpoint suffix 与 canonicalized resource 分离 |
| P2 | Append Blob | `Put Blob: AppendBlob`、[`Append Block`](https://learn.microsoft.com/en-us/rest/api/storageservices/append-block)、`Append Block From URL`、Seal | 日志、审计流、只追加数据 | 使用 append position/max-size conditions 防止并发错位；与可乱序 Block Blob 上传不同，必须串行或由条件保证顺序 |
| P3 | Page Blob | `Put Blob: PageBlob`、[`Put Page`](https://learn.microsoft.com/en-us/rest/api/storageservices/put-page)、`Put Page From URL`、`Get Page Ranges`、Incremental Copy | VHD、随机写、稀疏文件 | 512-byte 对齐、预声明最大长度、clear page、增量快照等模型与普通文件上传差异很大，应独立模块 |
| P2 | Blob Batch | [`Blob Batch`](https://learn.microsoft.com/en-us/rest/api/storageservices/blob-batch) | 一个请求批量 Delete Blob 或 Set Blob Tier | 单批最多 256 个同类型子请求；解析 `multipart/mixed` 的每个子响应；顶层 202 不代表每项成功；按失败项重试，不能重复错误地处理已成功项 |
| P3 | Query Blob Contents | [`Query Blob Contents`](https://learn.microsoft.com/en-us/rest/api/storageservices/query-blob-contents) | 在 Block Blob 端过滤 delimited/CSV 或 JSON 输入，输出 delimited、JSON 或 Arrow，减少下载量 | 不支持 Parquet 输入；独立 streaming query API；不复用普通 Range 下载 checkpoint；响应中的数据、进度、错误和结束帧需单独解析 |

### 6.2 Azure 可选控制面能力

本次评估不建议为这些能力建立 `azure-blob-management` feature；优先使用 ARM/Bicep/Terraform、Azure CLI、Portal 或官方 SDK：

| 类别 | 可增加能力 | 用途与边界 |
|---|---|---|
| Container 管理 | Create/Delete/Get Properties、metadata、ACL、Lease Container | 管理后台与部署工具；删除 container 是高风险操作 |
| Stored Access Policy | Container ACL 中的 signed identifiers | [每个 container 最多 5 条](https://learn.microsoft.com/en-us/azure/storage/blobs/scalability-targets)；为 Service SAS 提供可撤销、可修改的权限策略；User Delegation SAS 不使用 Stored Access Policy |
| CORS/服务属性 | Get/Set Blob Service Properties、CORS | 浏览器直传、日志、指标；更新通常替换整个配置 |
| Lifecycle | [Lifecycle management](https://learn.microsoft.com/en-us/azure/storage/blobs/lifecycle-management-overview) | 按时间、prefix、tags 转 tier 或删除 current/version/snapshot；不能用 lifecycle 把 Archive rehydrate 回 online |
| Versioning/Soft delete | 账户级 data protection 设置 | 防误删；属于 ARM/管理面配置，不完全是 Blob REST 数据面 |
| Object replication | [Object replication](https://learn.microsoft.com/en-us/azure/storage/blobs/object-replication-overview) | 异步跨账户复制；两端需 versioning、源端需 Change Feed；不支持 HNS、Append/Page Blob、CPK Blob，快照不复制；使用独立任务状态模型 |
| Inventory/Change feed | inventory policy、change feed | 大规模盘点、审计、增量处理；结果与延迟模型不同于在线 List Blobs |
| Immutability 管理 | container/version 级 retention policy、legal hold | 合规高权限操作；锁定不可逆风险要求显式 API 和额外审计 |
| 静态网站 | Blob Service 静态网站设置与 `$web` container | 独立 website endpoint；匿名读取、CORS、缓存和自定义域需要共同设计 |
| Point-in-Time Restore | ARM `restoreBlobRanges` 长任务 | [仅支持 Block Blob](https://learn.microsoft.com/en-us/azure/storage/blobs/point-in-time-restore-overview)；依赖 versioning、soft delete 和 change feed；同一账户一次只能运行一个，开始后不能取消；任务不可简单当作单对象下载恢复 |

### 6.3 Azure 专用与高级能力

| 初版候选排序（已废弃） | 功能 | 可增加的接口 | 适用场景与关键限制 |
|---|---|---|---|
| P1 | 账户与协议能力探测 | [`Get Account Information`](https://learn.microsoft.com/en-us/rest/api/storageservices/get-account-information)、可配置 `x-ms-version` | 获取 account kind、SKU、HNS 状态，结合 service version 生成 `Capabilities`；SAS 的 `sv` 与请求 API 行为版本不能混为一谈 |
| P2 | 次要 endpoint 只读回退 | [`Get Blob Service Stats`](https://learn.microsoft.com/en-us/rest/api/storageservices/get-blob-service-stats)，`{account}-secondary` | RA-GRS/RA-GZRS 容灾读取；异步复制可能返回旧内容或短暂 404，结果应标注来源 endpoint，写请求绝不能切到 secondary |
| P2 | 快照、版本与删除版本恢复 | `Snapshot Blob`、`versionid`、`Undelete Blob`、Copy Blob | `Base`、`Snapshot(id)`、`Version(id)`、soft-deleted 状态必须用不同类型；版本恢复通常是把旧版本复制为新的 current Blob；[HNS 不支持 Blob Versioning](https://learn.microsoft.com/en-us/azure/storage/blobs/versioning-overview)，且应以生命周期控制单 Blob 版本数，官方建议少于约 1,000 个以避免 List 延迟增长 |
| P2 | Append Blob 封存 | `Append Block`、`Append Block From URL`、[`Append Blob Seal`](https://learn.microsoft.com/en-us/rest/api/storageservices/append-blob-seal) | 用 append position/max-size 条件防止重试产生重复或乱序；Seal 后不可继续追加，但可修改允许的属性 |
| P3 | Page Blob 与增量复制 | `Put Page`、`Clear Page`、`Get Page Ranges`、`Put Page From URL`、Incremental Copy | VHD/稀疏文件；页范围需 512-byte 对齐，更新、序列号和快照链模型与 Block Blob 完全不同，应独立 trait |
| P3 | Azure Data Lake Storage Gen2/HNS | [Data Lake Storage Gen2 REST API](https://learn.microsoft.com/en-us/rest/api/storageservices/data-lake-storage-gen2)：file/directory、append/flush、rename、ACL | 使用 `dfs.core.windows.net`，支持真实目录、原子 path rename 和 POSIX ACL；与普通虚拟目录语义不同，建议 `azure-datalake` feature；部分 Blob 功能与 HNS 不兼容 |
| P3 | HNS Blob 到期 | [`Set Blob Expiry`](https://learn.microsoft.com/en-us/rest/api/storageservices/set-blob-expiry) | 为 HNS 文件设置相对/绝对到期时间；不适用于目录，到期文件不会进入 soft-delete 状态，不能通过 soft delete 恢复，必须标记确定的数据丢失语义 |
| P3 | Change Feed 与事件类型 | [Blob Change Feed](https://learn.microsoft.com/en-us/azure/storage/blobs/storage-blob-change-feed)、Event Grid Blob 事件 schema | 增量同步、审计和索引；Change Feed 是 `$blobchangefeed` 中的 Avro 日志且存在延迟；不进核心，出现多项目 schema 复用需求后可另立 types crate |
| P3 | Blob Inventory | [Blob Inventory](https://learn.microsoft.com/en-us/azure/storage/blobs/blob-inventory) | 周期性生成 CSV/Parquet 清单，适合离线盘点而非实时 list；ARM 策略与报告对象读取使用不同认证平面 |
| P3 | 客户端加密 v2 兼容层 | [Client-side encryption](https://learn.microsoft.com/en-us/azure/storage/blobs/client-side-encryption) | 若需与其他 Azure SDK 互操作，必须完整复现 AES-GCM envelope 格式和 metadata 并经过安全审计；若合规需求通过 gate，先评估 Encryption Scope，CPK 和客户端加密仍分别遵循 E 类高风险门槛 |

`Copy Blob` **可能**异步，Archive rehydrate、Point-in-Time Restore 等也是长任务。只有对应功能先通过需求 gate，才提取允许“初始即完成”的最小 `Poller<T>`/operation handle；不能为了候选功能预建公共 poller 框架。`Copy Blob` 即使返回 202 也可能同时给出 `x-ms-copy-status: success`，此时无需继续轮询。

## 7. 两家云应统一的范围：只统一传输概念

本次评估否决近期建立通用 `ObjectStore`。`put/get/head/list/delete/copy` 会把 0.3.x 的传输 facade 变成另一套对象 SDK，并形成长期兼容承诺；当前既无用户证据，也已有 OpenDAL 和云厂商 SDK 可选。若未来通过 13.9 的产品转型 gate，优先建立独立 `rusty-cat-object` 或 OpenDAL 适配 crate，而不是扩张 `BreakpointUpload`/`BreakpointDownload`。

当前可以稳定统一的只有传输语义：

| 统一概念 | 阿里云 OSS | Azure Blob Storage | 决策 |
|---|---|---|---|
| `TransferResource` | endpoint + bucket + key + version | endpoint + container + blob + version/snapshot | 核心；资源身份进入 checkpoint，secret query 不进入 |
| `UploadMode` | Single/Multipart | Single/Block | B；先提供 `Auto/Single/Multipart`，阈值由基准决定 |
| `UploadCheckpoint` | UploadId + part receipt | session namespace + block receipt | B；schema 由 SDK 定义、应用持久化 |
| `WriteCondition` | forbid-overwrite/ETag | ETag + 可选 lease ID | B；只覆盖安全提交所需子集 |
| `IntegrityPolicy` | MD5/CRC64 可得性 | transactional MD5/CRC64 | B；必须准确返回 Verified/NotProvided/Mismatch |
| 公共 `TransferReceipt` | ETag/version/request ID/checksum | ETag/version/request ID/checksum | B；内部证据保留是 A，对外以可选版本化 payload 兼容演进 |
| `CloudTransferError` | Code/RequestId/HostId | Code/request ID/client request ID | A；只建传输错误，不建全量服务错误 |
| cancel capability | 可 abort 指定 UploadId | 默认只能停止本地任务并保留 blocks | A；不伪造跨云等价语义 |

以下差异必须保留 provider 类型，不能为了统一而丢失：OSS `UploadId` 与 Azure block namespace、part number 与 block ID、version 与 snapshot、无损 abort 是否存在、ETag 与内容 checksum、条件写和 lease 规则。

### 7.1 不进入近期公共抽象的能力

- `ObjectStore`、`ObjectClient`、`ListPager`、`CopyOperation`、通用 `StorageTier`、`Presigner`、管理面 `Poller`；
- 运行时 `ObjectCapabilities` 大表。当前可用性由 crate feature、显式配置和结构化 provider 错误表达即可；权限也无法靠一次探测可靠推断；
- 把 metadata、tags、ACL、WORM、HNS、Append/Page Blob 全部塞进通用 options；
- 直接给现有公开 `TaskOutcome` 增加大量必填字段。应使用可选、版本化 receipt/payload，避免无必要的破坏性 API 变更。

### 7.2 性能能力的判断

| 能力 | 决策 | 原因与门槛 |
|---|---|---|
| provider 并行 Range | B | 符合传输定位；先确保原子 request 构造、强 revision、绝对偏移和有界内存 |
| 安全自动 chunk size | B | 先只保证不会超过 provider part/block 数量和大小边界 |
| 自适应并发 | C | 固定并发更可预测；需要吞吐、延迟、内存、429/503 的对照基准证明净收益 |
| 显式局部 Range 下载 | C | 需要媒体、稀疏文件或随机读取的真实调用场景，并定义局部校验语义 |
| secondary/acceleration 自动选择 | C | 有一致性、费用和回退风险；必须显式 opt-in 并记录实际 endpoint |

## 8. 推荐分层与 Cargo features

### 8.1 分层边界

| 层级 | 职责 | 建议模块 | 默认是否启用 |
|---|---|---|---|
| L0 传输核心 | 调度、并发、暂停/取消、重试、限流、checkpoint、文件一致性、内存预算 | 现有 executor、upload/download trait | 是 |
| L1 Provider 数据通道 | endpoint、认证/签名、multipart/block、Range、错误解析 | `aliyun_oss_direct`、`azure_blob_direct`、presigned/SAS adapter | 按 feature |
| L2 可选凭证桥接 | 把应用提供的可刷新 STS/Bearer source 接到 provider 请求 | 独立 adapter feature/crate | 需求成立后 |
| L3 通用对象 API | put/get/head/list/delete/copy、metadata、tags | 优先 OpenDAL 适配或独立 `rusty-cat-object` | 当前不立项 |
| L4 Provider 专业能力 | callback、append、select/query、page、HNS、媒体等 | 云厂商 SDK/独立 crate | 移出本路线图 |
| L5 Bucket/Container/账号管理 | lifecycle、replication、WORM、ARM、RAM/RBAC、网络与策略 | IaC、控制台、官方 SDK | 明确不做 |

`BreakpointUpload`/`BreakpointDownload` 继续只承担传输协议钩子。list、delete、tag、copy、lease 等不应持续塞进这两个 trait；否则所有 provider 都会出现大量没有意义的 `Unsupported`。

### 8.2 Cargo feature 决策

不要为尚未获批的对象、控制面和专业功能提前创建空 feature。每个 feature 都会带来组合编译、文档、semver 和真实云测试成本。近期保持现有细粒度 provider feature：

```toml
default = []

aliyun-oss-direct = []
aliyun-oss-presigned = []
azure-blob-direct = []
azure-blob-sas = []
```

凭证 adapter 只有通过需求 gate 并真正实现时才增加 feature。当前 README 已明确 `aliyun-oss` 与 `azure-blob` 别名只转发到 presigned/SAS；没有误用证据时不应为猜测提前弃用，若未来改名应在 major release 前先收集 issue/support 数据。

### 8.3 通用抽象必须保留的差异

下表是避免错误抽象的设计约束，不表示每一行对应能力都已批准；冷数据、追加、复制和目录行仅用于说明为什么不应硬统一。

| 主题 | 阿里云 OSS | Azure Blob Storage | API 设计结论 |
|---|---|---|---|
| 上传会话身份 | 明确的 `UploadId` | 没有 upload ID，uncommitted blocks 挂在 Blob 名下 | checkpoint 使用 provider enum，不能伪造一个相同会话模型 |
| 中止上传 | `AbortMultipartUpload` 可删除该 UploadId 的 parts | 无等价的无损 abort | 通用 cancel 只承诺停止本地任务；远端清理由 provider policy 描述 |
| 分片标识 | PartNumber 1–10,000 | Base64 block ID；最多 50,000 committed blocks | 核心统一 index/offset，wire ID 保留 provider 类型 |
| 对象版本 | version ID、delete marker | version ID、snapshot、soft-delete 状态 | `ObjectRevision` 必须是 provider-scoped enum |
| 覆盖保护 | `x-oss-forbid-overwrite`，versioning 时有特殊限制 | ETag/time/tag 条件与 lease | 公共 `WriteCondition` + Azure lease extension |
| 内容校验 | MD5、CRC64/XZ；multipart 最终 ETag 非整对象 MD5 | transactional MD5/CRC64、可存储 Content-MD5；ETag 也不是通用内容哈希 | 独立 `Checksum` 和 `EntityTag` 类型 |
| 认证 | V4 AK/STS、V4 presigned URL | Entra、Managed Identity、Shared Key、三类 SAS | 统一 credential/authorizer 生命周期，不统一凭证字段 |
| 冷数据恢复 | RestoreObject/CleanRestoredObject，部分场景可直接读 Archive | Set Tier/rehydrate | 若两家对应能力未来分别获批，可共享最小 poller 外壳；当前不预建 |
| 追加/随机写 | AppendObject | Append Blob、Page Blob | provider extension，不塞进普通 multipart 接口 |
| 复制 | CopyObject/UploadPartCopy | Copy Blob、Put Blob/Block From URL；可异步 | 若复制能力未来获批，再返回 provider-aware operation；当前不建公共抽象 |
| 目录 | 通常是 key prefix；特殊 bucket/接口另有层级语义 | 普通 Blob 是虚拟目录；HNS/ADLS 有真实 path API | 核心不承诺 POSIX 目录或原子 rename |

## 9. 推荐实施路线图

路线图只包含已经证明符合产品边界的工作。候选目录不是 backlog；C/D/E 类功能不得因为“顺手”进入阶段。

### 阶段 0：现有能力正确性热修复

目标：在扩充 API 前，保证已有分片上传、下载不会混合数据或删除旧对象。

1. Azure cancel 默认只停止本地任务并保留 uncommitted blocks，禁止无条件删除目标 Blob。
2. OSS 禁止按 object key 自动认领未知 multipart session；已知 UploadId 的 `ListParts` 完整分页，complete 提交准确的 `PartNumber + ETag`。
3. Azure block ID 加逻辑任务 namespace，防止两个 writer 混用 uncommitted blocks。
4. endpoint/resource/origin/redirect 强校验；合并最终 query/headers/body hash 后再签名；任何 secret 不进入日志和 checkpoint。
5. 回归已有 OSS part、Azure block 数量/大小检查，补齐 Azure block ID 原始长度/Base64 等长/URL 编码和 service-version 关联，并支持零字节对象。
6. 对当前协议实际使用的 XML error/list/complete 响应进行有界解析，保留 provider code 和 request ID。
7. complete/commit 证据不足时返回 `Indeterminate`，不得伪装成功、盲目创建新会话或执行破坏性清理。

**完成标准**：不会提交未知或陈旧分片；Azure cancel 不改变任务前已存在 Blob；两个 writer 不混用 OSS part/Azure block；授权信息只发往明确允许的 HTTPS resource；边界和故障注入测试通过。

### 阶段 1：逐项批准的传输增强

以下项目彼此独立，不要求整批立项。优先用 issue、用户访谈、匿名 opt-in 指标或生产故障证明需求：

1. provider 并行 Range 下载，并保持强 ETag/revision、绝对偏移、有界内存和稳定 resume identity；
2. 版本化 direct upload checkpoint、显式会话导入/导出与远端 reconcile；
3. transactional MD5/CRC64 与准确的 `Verified/NotProvided/Mismatch` 结果；
4. 应用提供的异步可刷新 STS/Bearer credential source；具体 IMDS/OIDC/Entra 登录器仍在外部；
5. `UploadMode::Auto`、安全自动 chunk size、最小 `WriteCondition`、revision 和 transfer receipt；
6. 少量上传期 HTTP 属性/metadata；不顺带加入 tags、ACL、tier、WORM。

**完成标准**：每项有独立公共 API 评审、兼容策略、基准或故障模型、两家 provider 的相关测试；不能以“阶段 1”名义捆绑发布。

### 阶段 2：当前 No-Go 的产品转型

通用 `ObjectStore`、完整对象 CRUD、presigner、copy/poller 和对象标签目前不进入路线图。只有通过 13.9 的对象 SDK 转型 gate 后，才另开设计文档，比较以下三种方案：

- 保持 `rusty-cat` 专注传输，由应用同时使用 OpenDAL/官方 SDK；
- 提供 `rusty-cat-opendal` 桥接 crate；
- 建立独立 `rusty-cat-object` crate，而不是扩大现有传输 trait。

### 阶段 3：移出当前产品路线图

Bucket/Container/ARM 控制面、WORM、网站、CORS、复制策略、媒体/图片/查询、事件、Page Blob、ADLS Gen2/HNS 和客户端加密不属于 `rusty-cat`。未来若出现明确产品，建立独立项目，不能把它们作为自然的“下一阶段”。

## 10. 测试与验收清单

以下是条件化测试目录：阶段 0 项目是近期发布门槛；checkpoint、并行下载、copy、rehydrate、lease、version、Blob Batch 等只有在对应功能通过 gate 后才启用，不能反过来用测试清单暗示功能已立项。极限大小和最大 part 数应优先用 checked-arithmetic、合成 fixture 和录制响应验证，不要求 CI 真上传最大对象。

### 10.1 通用传输测试

- 零字节、1 byte、刚好一个 part、末片极小、最大允许 part、最大 part 数边界；
- 文件名包含空格、`+`、`%`、中文、emoji、连续斜杠及需要签名的 query；
- 响应短读、重复 part、乱序完成、连接复用失败、DNS/连接/读超时、用户取消；
- 429/5xx/服务端 Retry-After、最大总重试时间、取消优先级和有界队列；
- 源文件上传途中被截断、替换、mtime 改变；目标对象下载途中 ETag/version 改变；
- checkpoint 写入中断、旧 schema 迁移、options/source/resource fingerprint 不匹配；
- complete/commit 已成功但响应丢失，重启后能对账为成功而不是重复创建会话；
- secret、SAS、预签名 URL、STS token、CPK 不出现在 `Debug`、错误和 tracing 中。

### 10.2 阿里云 OSS 契约测试

- 100 KB 非末片边界、5 GB part 上限、10,000 parts 上限；
- `ListParts` 超过 1,000 项和 `ListMultipartUploads` 多页 marker；
- 同一 object key 存在多个 UploadId 时，只恢复显式指定且与 checkpoint 匹配的会话；
- 使用准确 ETag 清单 complete，缺失、重复、错误顺序和 stale part 均失败；
- `versionId`、response override、callback 等 query/header 确实纳入 V4 canonical request；
- Range CRC64 不被误当作分片 CRC；完整下载完成后才比较对象 CRC64；
- STS token 刷新、签名时间偏差、加速/内网/CNAME endpoint 与 region scope。

### 10.3 Azure Blob 契约测试

- 两个进程向同一 Blob 上传时 block namespace 不碰撞、不混合 commit；
- 任务开始前已有 Blob，取消上传后旧 Blob 的 ETag、内容、metadata 均不变；
- block ID Base64/等长/长度限制，50,000 blocks 与服务版本相关大小限制；
- `Committed`/`Uncommitted`/`Latest` block list 组合、commit 条件与 lease 冲突；
- Range MD5/CRC64 的 4 MiB 边界以及二者互斥；
- version、snapshot、soft-deleted Blob 不被当作同一 revision；
- Blob Batch 顶层 202 但部分子请求失败；copy/rehydrate poller 的 pending、失败和超时；
- Azurite 契约测试加真实 Azure smoke test；模拟器不支持的能力必须在真实服务验证。

### 10.4 兼容性与发布门槛

- 测试当前默认 REST 版本以及至少一个较旧/较新受支持版本；
- 未知 XML 字段和未知枚举值不能让整个响应反序列化失败；
- 公开 checkpoint/schema、error code、revision token 版本化；
- feature 任意合理组合均可编译，`default = []` 保持轻量；
- 真实云测试凭证只来自 CI secret，测试对象使用随机前缀并有兜底清理；
- destructive 测试仅在隔离 bucket/container 运行，不测试自动 break lease、永久删除版本或锁定 WORM。

## 11. 不建议纳入核心或不应承诺的内容

- 核心不直接实现所有 OSS STS/Azure Entra 身份流程，只定义 credential-provider 生命周期；AssumeRole、IMDS、managed identity、OIDC 等由可选适配器或调用方 provider 获取。
- 请求签名和刷新缓存不可避免会在内存中短暂持有 AccessKey、account key、SAS、OAuth token 或客户密钥，但不得持久化或记录它们；使用 redacted/可清零 secret 类型，checkpoint 中绝不包含 secret 或 signed URL。
- 不内置 SQLite/业务数据库；若版本化 upload checkpoint 通过 Gate B，SDK 只定义 schema 和存储回调，由应用持久化。
- 不统一 RAM、Azure RBAC、Bucket Policy、Stored Access Policy 的策略语言。
- 不把 Azure ARM account provisioning、网络规则、private endpoint、密钥轮换塞进数据面 client。
- 不把 lifecycle、replication、WORM/immutability/legal hold 假装成跨云完全等价模型。
- 不默认执行递归删除、批量永久删除、break lease、清除保留策略、删除非本任务对象等高风险操作。
- 不把“移动/重命名”宣传成普通对象存储的原子操作；多数情况是 copy + delete，只有特定 HNS/目录 API 才有不同保证。
- 不把 ETag 当作统一 MD5，也不承诺 exactly-once；对外承诺应是幂等 part identity、条件请求和可对账状态机。
- 不默认实现跨云同步、镜像、FUSE、CDN 或数据库；这些应作为使用 `rusty-cat` 的上层产品。
- 客户端信封加密不是首轮核心能力；若合规需求成立，应先评估服务端加密 typed options，跨 SDK 客户端加密则需独立格式设计、安全审计和测试向量。

## 12. 建议近期版本范围

近期只批准第 9 章阶段 0：安全取消、会话隔离、准确 commit、endpoint/签名边界、provider 限制、零字节、有界解析和不确定结果语义。源码已经具备的源文件代际校验和内存预算补回归测试，不重复计为新能力。

下一次版本最多从“并行 Range、direct checkpoint、transactional checksum、可刷新 credential source、`UploadMode::Auto`”中选择证据最充分的一至两项，不启动通用对象数据面。第 13 章 A～E 结论覆盖第 4～6 章遗留 P 值。

## 13. 逐项必要性筛查

表中“有用性”评价的是功能对某类业务是否有价值；“决策”才表示是否应由当前 `rusty-cat` 实现。C 类不是承诺排期，D 类不是说云能力无价值，E 类也不能仅靠 feature flag 消除风险。

### 13.1 第 4 章：跨云传输可靠性候选

| 候选能力 | 有用性 | 决策 | 筛查结论、最小边界与反转条件 |
|---|---|---|---|
| provider 同文件并行 Range 下载 | 高 | **B 窄实现** | 符合大文件传输定位，能兑现 `max_parts_in_flight` 的直觉预期，但串行下载本身仍正确，不能与数据安全 P0 混为一谈。先改成原子构造 request，再保证绝对偏移、短读/206/Content-Range 校验、强 revision 和有界内存。若公开文档已明确承诺这些 provider 并行，则“修文档或实现”升级为 A。 |
| OSS standard Range、version identity、presigned `resume_identity` | 高 | **A 核心** | 新增点是 OSS `x-oss-range-behavior: standard`、可签名 version identity 和换签稳定身份；现有强 ETag/If-Match 属回归项。共同防止越界 Range 被当整对象、不同代际混合和错误复用 checkpoint。 |
| 禁止 OSS 自动认领未知 UploadId；Azure block session namespace | 高 | **A 核心** | 两个并发 writer 不得串片。恢复只接纳显式会话和与本地 receipt 匹配的远端 part/block；Azure namespace 必须固定长度、随机且进入 checkpoint。无安全的通用应用层补救。 |
| OSS `ListParts` 分页和准确 complete | 高 | **A 核心** | 当前传输路径会依赖它，漏页或 `complete-all` 会破坏可靠完成语义。只提交已核实的 `(PartNumber, ETag)`；边界和异常 XML 必须测试。 |
| `ListMultipartUploads` 全前缀发现/分页 | 中 | **C 门控** | 对诊断和孤儿运维有用，但不是显式 UploadId 恢复所需，扫描还可能认领或清理其他应用会话。只有可证明 owner namespace、具名运维场景时做独立工具。 |
| 版本化 direct upload checkpoint、导入/导出/reconcile | 高（长任务） | **B 窄实现** | 产品方向一致，但 README 已披露 direct 尚不能跨进程注入旧会话，因此不是现有承诺的安全热修。SDK 定义 schema 和原子回调，应用持久化；出现至少两个下游或一个已承诺生产集成后立项。 |
| transactional MD5/CRC64 | 中高 | **B 窄实现** | 能覆盖 TLS 之外的请求级传输错误，适合可靠执行器；策略必须允许服务端未提供并返回 `NotProvided`，不能伪报 Verified。先用故障模型和请求成本决定默认值。 |
| Azure ≤4 MiB Range MD5/CRC64 | 中 | **B 可选策略** | 能直接验证小 Range，但若为此强制把所有 chunk 限到 4 MiB 会增加请求数；默认 BestEffort，大小和算法显式选择。 |
| OSS 完整对象 CRC64 | 中高 | **B BestEffort** | 仅完整下载后比较整对象 CRC，不能拿对象 CRC 校验单个 Range；老对象或 copy part 可能不提供，必须返回 `NotProvided`。 |
| 最终长度/revision/receipt 对账 | 高 | **A 核心语义** | 成功结果必须说明服务端接受了什么；仅相同长度或 ETag 不是内容证明。证据不足返回 `Indeterminate`。完整 readback 或业务 SHA metadata 不是默认要求。 |
| 完整对象 readback/业务 SHA 校验 | 中 | **C 门控** | 证明力度强但可能增加一遍全量 I/O、费用和 metadata 约定；只在明确故障模型或合规要求下启用。 |
| 自动写业务 SHA-256 metadata | 中 | **C 门控** | 会改变用户 metadata、增加预扫和命名约定。只允许调用方显式提供摘要/key；不得作为核心成功条件。 |
| “最终请求冻结后签名”与保留 header 冲突检查 | 高 | **A 核心** | 直接关系签名正确性和凭证泄露。URL、query、headers、body hash 冻结后签名；调用方不能覆盖认证、Host、Range、checksum、lease 等字段。 |
| direct provider 对公共 builder headers 的契约处理 | 高 | **A 兼容修复** | 当前 direct 路径会丢弃调用方 headers。若公共 API 承诺支持，就在冲突检查后传入并签名；若不支持，就应尽早明确拒绝，不能静默忽略。typed 便利字段仍另按 B/C 决策。 |
| content-type、HTTP 属性、metadata 上传选项 | 高 | **B 窄实现** | 常用且与上传结果直接相关；只做上传期最小 typed options，并确保 multipart 各阶段一致。不因此建立完整 metadata CRUD。 |
| tags、tier、request-payer、流量、加密等 provider options | 特定场景中高 | **C 门控** | 确实可能让特定 bucket 可用，但频率未知、计费和安全语义不同。每项独立获批，不使用一个任意 header map 绕过保护边界。 |
| 内部安全 commit 条件 | 高 | **A 核心** | 只包含防止 SDK 自身错误覆盖、并发 commit 所需条件。失败必须结构化返回。 |
| 公共 `CreateOnly/MatchEtag/lease-id` 等条件 | 中高 | **B 窄实现** | 与可靠上传直接相关，但只暴露跨云最小集合和 provider extension；time/tag 条件保持 C。若目标永远是随机唯一 key，可降低优先级。 |
| 可刷新 STS/Bearer credential source 接口 | 高（direct 长任务） | **B 窄实现** | 核心只消费应用提供的不可变 credential snapshot、到期时间和 async refresh，支持 single-flight 并脱敏。先确认 direct 使用率和 token 过期故障。 |
| 内置 STS AssumeRole、IMDS、OIDC、Entra/Managed Identity 登录器 | 中 | **C 门控，优先外置** | 与 README 的应用职责边界冲突，并引入 SSRF、代理、重定向、依赖和平台矩阵。优先接受 closure/trait；多个用户重复实现且有 owner 时才做独立 adapter crate。 |
| 获取 delegation key、生成 OSS URL/Azure SAS | 中高（后端） | **D 不纳入核心** | 当前库消费后端签发的预签名计划，签发属于权限边界。若产品明确转为 presigner toolkit，另做经安全审查的 crate。 |
| 结构化 provider error、request ID、Retry-After、幂等分类 | 高 | **A 核心** | 调度器掌握重试，所以必须理解 provider code、429/5xx、总预算、jitter 和取消优先级；不是从零重建已有通用重试，而是补齐 provider 语义。 |
| endpoint 级熔断、完整 tracing 集成 | 中 | **C 门控** | 熔断需要应用全局视角，库内共享状态可能误伤无关任务；暴露错误分类和 hook，交给 tower/宿主应用。基准证明显著收益后再选配。 |
| Azure 安全 cancel | 高 | **A 核心** | 默认不得 DELETE 旧 Blob，只停止本地任务并保留 uncommitted blocks；独占临时 Blob 删除另做显式能力。 |
| OSS `AbortMultipartUpload` | 高 | **已有，需回归** | 只 abort 已知 UploadId，保留现有能力和失败结果测试；不能靠 `Drop` 承诺网络清理。 |
| 显式 async close/可重试 cleanup handle | 中高 | **B 窄实现** | 补足任务生命周期和失败可恢复性，但不包含全前缀扫描或删除未知会话。 |
| 按年龄扫描/批量删除孤儿会话 | 中（运维） | **D 核心不做** | 误删其他应用任务的风险高。文档推荐 bucket lifecycle 兜底；若有 owner tag/prefix，可在独立显式运维工具中重新评估。 |
| endpoint/resource/origin/redirect 校验 | 高 | **A 核心** | 防止 Authorization、SAS、STS token、CPK 发往错误 origin。默认 HTTPS；模拟器/HTTP 必须显式 opt-in，跨 origin redirect 不转发 secret。 |
| provider part/block 限制预检 | 高 | **A 补齐与回归** | OSS 限制、Azure block 数量/大小已有部分检查；保留回归，并补齐 block ID 原始长度、Base64 等长、URL 编码和 service-version 关联，使用 checked arithmetic 与合成边界测试。 |
| 零字节对象 | 高 | **A 核心** | 空文件是正常输入，不应因 multipart 假设失败。 |
| 有界 XML 解析 | 高 | **A 核心** | 只覆盖现有传输会使用的 list/error/complete 响应；处理 namespace、实体、未知字段和体积上限，不借此扩张全量对象 API。 |
| `Succeeded/NotCommitted/Indeterminate` | 高 | **A 核心** | 结论强度必须匹配证据；先保证不虚假成功或破坏性清理，自动成功对账可随 checkpoint 能力增强。 |
| 源文件代际校验 | 高 | **已有，非新增** | 当前 README/源码已有本地源代际与分片 SHA-256 机制；保留竞态、截断、替换回归测试。是否强制额外全文件预扫需用性能与故障模型证明。 |
| 统一内存预算与背压 | 高 | **已有，非新增** | 当前已有客户端级内存预算；继续做 OOM/取消回归测试。seekable stream、临时 spool 属于 C 类新能力。 |

### 13.2 第 5.1 节：阿里云对象与传输候选

| 功能 | 有用性 | 决策 | 筛查结论与替代方案 |
|---|---|---|---|
| `PutObject` + `UploadMode::Auto` | 高 | **B 窄实现** | 小/空文件省去 multipart 往返，仍属于上传执行器。阈值需按对象大小分布和 benchmark 配置，可靠性热修优先。 |
| 公开完整 multipart REST 原语 | 中高 | **B 收窄** | 公开 checkpoint/session import/export/reconcile/abort 即可；不必把每个 wire API 做成通用 raw SDK。`ListMultipartUploads` 仅为 C 类运维。 |
| OSS Callback | 中 | **C 门控** | 业务确认有价值，但 callback 失败不等于对象失败，且 presigned complete 请求已能由后端描述。多个后端用户证明通用 header/request 描述不足时再做。 |
| `HeadObject/GetObjectMeta/exists` | 高 | **C 对象层门控** | 内部 HEAD 已是传输必需，公开通用查询却是对象 SDK 边界；需要同步/素材管理产品证据。现阶段只返回传输准备/结果所需属性。 |
| `ListObjectsV2` | 高（同步/浏览） | **C 对象层门控** | 对点对点传输不需要，OpenDAL/OSS SDK 已提供。只有产品明确支持同步/备份，且需稳定 paginator 时放独立对象 crate。 |
| Delete/批量 Delete | 高（生命周期） | **D 当前 crate 不做** | 属于破坏性业务动作，与 multipart abort 不同；交给应用对象 SDK。产品转型后也必须条件化、无默认递归、逐项结果。 |
| CopyObject/UploadPartCopy | 高（同云迁移） | **C 门控** | 可省出口和客户端带宽，但地域、加密、tier 限制多，不是本地文件传输。先用 OpenDAL/OSS SDK；有可测迁移收益再做独立扩展。 |
| 内部强 If-Match 代际保护 | 高 | **已有，需回归** | 继续保证下载期间对象不换代，任何公共条件都不能覆盖它。 |
| 公共条件下载/缓存协商 | 中高 | **C 对象层门控** | 面向缓存的通用时间条件只在对象层立项；现阶段用 OpenDAL/OSS SDK。 |
| 指定 version ID 下载 | 高（审计/回滚） | **B 窄实现** | 作为 `TransferResource` revision 很小且能获得确定性下载；query 必须签名并进入 checkpoint identity。不扩展成版本浏览/恢复平台。 |
| 上传期 metadata/HTTP 属性 | 高 | **B 窄实现** | 与上传直接相关；typed options 进入初始化与完成阶段。公开 metadata CRUD 留给对象 SDK。 |
| Object tags | 中 | **C 门控** | 业务索引有用但非传输必需；真实跨云标签工作流出现后单独 feature。 |
| Object ACL | 低至中 | **D 不纳入** | 权限高风险且更适合 RAM/bucket policy/专用 SDK；不鼓励对象级 ACL 扩散。 |
| V4 presigned URL/POST policy 生成 | 高（后端） | **D 不纳入** | `rusty-cat` 当前消费预签名请求，后端签发是独立安全边界。若建立 presigner 产品则独立 crate、安全测试向量和权限评审。 |
| `x-oss-traffic-limit` | 中 | **C 门控** | 优先通用 task/global 本地 limiter；只有服务端单连接限速在真实网络中有可测优势时才加 typed provider option。 |
| 预配置 custom/internal/CNAME endpoint 兼容 | 高 | **B 核心兼容** | 在 13.1 的 A 类身份/HTTPS 校验基础上，正确分离 endpoint、region 和 host 签名；不负责开通服务。 |
| 自动探测/选择 acceleration 与回退 | 中 | **C 门控** | 有费用、地域和可预测性风险；先让应用显式选择，只有基准和失败策略齐备才自动化。 |
| Bucket acceleration 开关 | 中 | **D 不纳入** | 属于控制面，用控制台、IaC 或 OSS SDK。 |
| 上传 storage class | 中 | **C 门控** | 是低成本 typed option，但频率和费用语义未知；冷数据用户出现后增加。 |
| Restore/CleanRestoredObject/poller | 高（冷数据） | **C 门控** | 归档下载可能需要，但等待、费用和业务决策不应隐式发生。核心先结构化返回未恢复/恢复中；应用可用 OSS SDK 发起恢复。 |
| AppendObject/Seal | 高（日志流） | **D 当前 crate 不做** | 顺序、position、重试语义与可乱序 multipart 不同。只有项目转向日志流，才建独立类型/crate。 |
| PostObject | 高（浏览器） | **D 不纳入** | 属于浏览器表单和后端签名，不由 native Rust 传输执行器消费。 |
| SelectObject | 中高（查询） | **D 不纳入** | SQL、格式和帧协议是数据查询产品，不能复用 Range checkpoint；交给专用 OSS 客户端。 |
| requester-pays 请求 header | 高（特定 bucket） | **C 门控** | 缺它时特定 bucket 无法传输，实现成本低但涉及费用确认。出现一个真实 requester-pays 用户即可重新评估为 B。 |
| symlink、目录、Rename | 中（特殊 bucket） | **D 不纳入** | 普通 object key 不是 POSIX；会制造错误跨云可移植性承诺。文件系统产品应另建 crate。 |

### 13.3 第 5.2 节：阿里云控制面候选

| 功能 | 有用性 | 决策 | 原因/应使用的替代 |
|---|---|---|---|
| Bucket CRUD/Info/Location/Stat | 高（管理后台） | **D** | 不是文件传输；使用 IaC、控制台或 OSS SDK。 |
| Lifecycle/Access Monitor | 高（成本/清理） | **D** | `rusty-cat` 只应在文档推荐清理未完成 multipart 的 lifecycle 规则，不修改整桶策略。 |
| Versioning 配置 | 高（保护） | **D** | 账户/桶治理和费用决策；`ListObjectVersions` 若有对象产品需求可走 C 类独立对象层。 |
| 默认加密配置 | 高（合规） | **D** | KMS 权限、地域和策略管理由 IaC/官方 SDK；传输端仅兼容已配置策略。 |
| CORS | 高（Web） | **D** | 规则整体替换且与浏览器产品相关，不属于传输。 |
| Bucket transfer acceleration 开关 | 中 | **D** | 数据 client 可兼容显式 endpoint，不负责开通、计费和回退。 |
| Bucket WORM | 高（合规） | **E 独立高风险** | 锁定后不可逆；只有具名合规客户、人工确认、审计、模拟和真实账户隔离时在独立管理项目评估。 |
| Object WORM（邀测） | 低至中 | **E 独立高风险** | 对当前生产传输 crate 为 No-Go；只有能力转为稳定、独立合规产品通过 Gate E 时再评估。 |
| CRR/SRR 与进度 | 高（灾备） | **D** | 异步治理任务，有独立成本和状态机；用 OSS 管理 SDK/IaC。 |
| Inventory/Logging | 高（审计） | **D** | 离线盘点和日志配置，不是在线传输。 |
| Bucket Policy/ACL/Public Access Block | 高（安全） | **D** | 高权限、误配置影响整桶；使用 IaC 和专用权限审计。 |
| requester-pays 配置 | 中 | **D** | 桶级计费政策由管理面负责；传输端请求 header 仍按 C 类评估。 |

### 13.4 第 5.3 节：阿里云专业能力候选

| 功能 | 有用性 | 决策 | 筛查结论与替代方案 |
|---|---|---|---|
| 标准单 Range | 高 | **A 核心** | 属于当前下载正确性；越界、206、长度、ETag 必须严格校验。 |
| Multi-Range GET | 中 | **C 实验门控** | MIME boundary、分范围计费和局部重试复杂；先证明比并发单 Range 至少有稳定收益，再做 fuzz/故障测试。 |
| MetaQuery | 高（资产检索） | **D** | 搜索/索引产品，不是传输；使用 OSS SDK。 |
| 图片处理专用 API | 高（图片业务） | **D 不纳入** | 应用/后端生成含 `x-oss-process` 的 URL，再交给通用下载；通用 URL/resume identity 正确处理参数即可。 |
| 媒体、文档、异步数据处理 | 高（内容平台） | **D** | 任务、计费和 poller 属于数据处理服务，不能复用 transfer task。 |
| 事件通知模型/订阅 | 高（事件驱动） | **D** | 依赖 EventBridge/MNS/函数，属于消息系统；确有稳定 schema 复用需求时另立 types crate。 |
| 静态网站 | 高（托管） | **D** | 部署/匿名访问/路由策略不属于传输。 |
| CNAME 与证书管理 | 高（域名） | **D 不纳入** | client 不签发、绑定或续期证书；使用预配置 CNAME 的兼容性已在 13.2 单独评为 B。 |
| Referer/防盗链/TLS bucket 设置 | 中高 | **D** | 属于网站安全策略和整桶配置，使用 IaC/控制台。 |
| Access Point 创建/策略/删除 | 高（隔离） | **D 不纳入** | 属于管理面，使用 OSS SDK/IaC。 |
| 已配置 Access Point endpoint 兼容 | 中高 | **C 门控** | 只有真实传输失败样本证明通用 endpoint 配置不足时做薄适配。 |
| Archive Direct Read 开关 | 中 | **D** | 是 bucket 成本策略；核心只识别相关状态/错误，不自动开通。 |
| Vector Bucket/LiveChannel/Data Accelerator | 高（各自产品） | **D** | 完全不同的检索、直播、加速产品域，使用专用 SDK/独立 crate。 |
| Object FC Access Point/Resource Pool QoS | 中 | **D** | 需要额外云服务和角色；核心仅保留 endpoint/authorizer 扩展点。 |

### 13.5 第 6.1 节：Azure 对象与传输候选

| 功能 | 有用性 | 决策 | 筛查结论与替代方案 |
|---|---|---|---|
| `Put Blob` + `UploadMode::Auto` | 高 | **B 窄实现** | 小/空对象可省 stage/commit，属于上传执行器。阈值必须可配置，并用大小分布、延迟和请求成本决定。 |
| 公开完整 block REST 原语 | 中高 | **B 收窄** | 核心需要 session/checkpoint/reconcile，未必需要暴露任意 raw `Put Block`/`Latest` 操作；安全 namespace 和 receipt 校验优先。 |
| 增量 block 更新 | 中（镜像/备份） | **C 门控** | 固定分块大镜像可能受益，普通编辑会偏移全部块；要求 base ETag/lease、稳定分块和可测节省，否则整对象重传更安全。 |
| `Put Block From URL` | 高（迁移） | **C 门控** | 可省客户端带宽，但来源鉴权、SSRF、URL secret、条件和失败模型复杂；先用 Azure SDK/AzCopy。 |
| `Put Blob From URL` | 中高（小对象导入） | **C 门控** | 与上一项重叠，不应一次实现两套；真实来源大小分布决定是否需要。 |
| `Copy Blob`/`Abort Copy Blob` | 高（同云复制） | **C 对象层门控** | 非本地文件传输且可能异步；需要 copy ID/poller，Abort 还会留下零长度目标。用官方 SDK，出现明确发布/迁移工作流再评估。 |
| 内部 `Get Blob Properties`/HEAD | 高 | **A 核心内部** | 下载准备、revision 固定和结果对账已依赖它；应保留 ETag、version、长度、request ID。 |
| 公共 properties/exists/Set Properties/metadata CRUD | 高 | **C 对象层门控** | 403 不能当不存在，metadata 是整体替换；当前只公开传输 receipt 与上传期最小属性，完整 CRUD 用 Azure SDK/OpenDAL。 |
| 上传期 HTTP headers/metadata | 高 | **B 窄实现** | 与最终 Blob 直接相关，须在 commit 阶段一致应用；不因此承诺通用对象客户端。 |
| Get/Set Blob Tags | 中高 | **C 门控** | 标签与业务检索相关，不是传输必需；有跨云标签需求后放独立对象 feature。 |
| Find Blobs by Tags | 中高（资产检索） | **D 当前 crate 不做** | 查询索引、权限和一致性与 List 不同；使用 Azure SDK。 |
| List Containers | 高（浏览） | **D** | 账户/容器管理职责。 |
| List Blobs paginator | 高（同步/备份） | **C 对象层门控** | 对同步产品有价值，对点对点传输无必要；OpenDAL 已提供。产品转型后必须正确处理 NextMarker。 |
| Delete Blob | 高（生命周期） | **D 当前 crate 不做** | 破坏性业务动作，与安全 cancel 不同；交给应用，避免 snapshot/version/soft-delete 语义污染核心。 |
| version/snapshot-aware Get/Head | 高 | **B 窄实现** | 作为 provider-scoped revision 直接提高下载确定性；进入签名和 checkpoint identity，不等于负责创建或恢复历史版本。 |
| Snapshot Blob/历史版本恢复 | 高（备份） | **C 门控** | 属于备份对象层；先用 Azure SDK，有具名版本化备份客户再评估。 |
| Undelete/Soft delete 恢复 | 高（恢复） | **C 门控** | 依赖账户策略和业务判断，响应也不枚举全部恢复项；不放默认传输 API。 |
| Set Tier/rehydrate | 高（归档） | **C 门控** | 下载归档对象可能需要，但费用和长等待不能自动发生。核心返回结构化状态；调用方使用官方 SDK，明确支持 archive workflow 后再加 poller。 |
| 消费 caller-supplied lease ID | 高（leased blob） | **B 窄实现** | lease 会影响 stage/commit/delete 能否成功；作为 provider condition 传入，不由核心偷偷获取或续租。 |
| Acquire/Renew/Change/Release Lease | 中高 | **C 对象层门控** | 需要续租、所有权和故障恢复；优先由应用管理。`Break Lease` 属破坏性操作，不进入默认 API。 |
| Encryption scope | 高（合规用户） | **C 门控** | typed option 成本不高，但需求和账户约束未知；出现生产合规场景后添加，并验证 stage/commit 一致。 |
| Customer-provided key（CPK） | 高（特定合规） | **E 独立高风险** | secret 每请求传播、日志/错误脱敏和一致性要求很高；需真实云、零泄漏和安全审查。 |
| Immutability/legal hold | 高（合规） | **D 不纳入** | 高权限、可能不可逆，与传输无直接关系；使用 Portal/CLI/IaC/官方 SDK。独立合规产品必须重新通过 Gate E。 |
| SAS 生成器 | 高（后端） | **D 不纳入** | 当前 SAS provider 消费后端生成的请求；签发扩大权限攻击面。使用 Azure 身份/存储 SDK。 |
| 可配置 REST service version | 高（兼容） | **B 核心兼容** | 当前版本固定会限制主权云、模拟器和新特性；使用受支持版本枚举和显式 escape hatch，不静默升级。 |
| Global/Government/China/custom endpoint/Azurite | 高 | **B 核心兼容** | 资源绑定与防泄漏已在 13.1 单独评为 A；本项安全支持显式 endpoint/service suffix，HTTP 模拟器只能显式 opt-in。 |
| Append Blob/Append Block/Seal | 高（日志流） | **C 独立协议门控** | 顺序、条件和 sealing 与乱序 Block Blob 不同；只有多个日志/审计流用户时用独立 trait/feature。 |
| Page Blob/增量复制 | 高（VHD） | **D 不纳入** | 稀疏随机写、512-byte 对齐、快照链是另一产品模型；使用 Azure SDK/AzCopy，VM 镜像产品另建 crate。 |
| Blob Batch | 中高（批量管理） | **D 当前 crate 不做** | multipart 子响应和逐项重试复杂，是对象批处理而非文件传输。 |
| Query Blob Contents | 中高（查询） | **D 不纳入** | 查询格式、进度和错误帧与 Range checkpoint 无关；使用专用 API。 |

### 13.6 第 6.2 节：Azure 控制面候选

| 功能 | 有用性 | 决策 | 原因/应使用的替代 |
|---|---|---|---|
| Container CRUD/properties/ACL/Lease Container | 高（管理） | **D** | 高权限、破坏性并偏离传输；使用 Azure SDK/CLI/IaC。 |
| Stored Access Policy | 高（SAS 治理） | **D** | 权限策略和整份 ACL 更新属于安全管理面。 |
| CORS/Blob Service Properties/日志指标 | 高（Web/运维） | **D** | 服务级配置，更新可能覆盖整个文档；用 ARM/Bicep/Terraform。 |
| Lifecycle | 高（成本） | **D** | 规则会自动转 tier/删除对象版本，费用和数据风险应由 IaC 管理。 |
| Versioning/Soft Delete 账户设置 | 高（保护） | **D** | 数据保护策略不是单次传输选项。 |
| Object Replication | 高（灾备） | **D** | 依赖 versioning/change feed，有独立限制和长任务状态。 |
| Inventory/Change Feed 策略 | 高（审计） | **D** | 离线清单和增量日志管理，不是在线 transfer。 |
| Immutability 管理 | 高（合规） | **D 不纳入** | 不可逆、高权限；当前使用官方管理工具，独立合规产品须另过 Gate E。 |
| 静态网站 | 高（托管） | **D** | 网站 endpoint、匿名访问和 CORS 是部署产品。 |
| Point-in-Time Restore | 高（灾备） | **D** | ARM 长任务、依赖多项账户设置且可能不可取消；不能包装成普通下载恢复。 |

### 13.7 第 6.3 节：Azure 专用与高级候选

| 功能 | 有用性 | 决策 | 筛查结论与替代方案 |
|---|---|---|---|
| `Get Account Information`/Capabilities | 中 | **C 门控** | SKU/account kind/HNS 不能证明当前 token 有权限或某操作一定可用；当前用配置、编译 feature 和结构化错误即可。 |
| 可配置 `x-ms-version` | 高 | **B** | 与账户探测拆开；属于 provider 兼容能力。 |
| secondary endpoint 只读回退 | 高（RA-GRS/GZRS） | **C 门控** | 可能读旧数据或短暂 404；必须显式 opt-in、只读、固定 revision 并记录实际来源。 |
| snapshot/version 引用读取 | 高 | **B** | 纳入 revision 模型；防止混合代际。 |
| 创建/恢复/undelete 历史版本 | 高（备份） | **C** | 独立对象恢复工作流，不属于默认传输。 |
| Append Blob Seal | 中 | **C 随 Append** | 只有 Append 协议获批才实现，不能单独预建。 |
| Page Blob/增量复制 | 高（VHD） | **D** | 另一个数据模型，见 13.5。 |
| ADLS Gen2/HNS path API | 高（数据湖） | **D 当前 crate 不做** | DFS endpoint、目录、append/flush、rename、ACL 是文件系统语义；需要时独立 provider/crate。 |
| HNS Blob Expiry | 中 | **D** | 确定性数据删除且不能 soft-delete 恢复，由应用/管理策略负责。 |
| Change Feed/Event Grid 类型 | 高（增量处理） | **D** | 事件与索引系统，不是传输；确有复用需求时另立 types crate。 |
| Blob Inventory | 高（盘点） | **D** | 离线清单与 ARM 策略，不是实时对象传输。 |
| 客户端加密 v2 兼容层 | 高（端到端加密） | **E 实验 crate** | 只有微软 SDK 双向测试向量、密码学审查、nonce/key 生命周期、损坏/截断/重放测试和长期格式 owner 齐备时才可推进。 |
| 通用 `Poller<T>` | 中高 | **C 按需内部件** | 只有 Copy/rehydrate 等获批后再提取最小 poller；不要为未获批功能预建框架。 |

### 13.8 第 7～8 章：跨云抽象与工程范围

| 提案 | 决策 | 说明 |
|---|---|---|
| provider-scoped `TransferResource` | **A 核心内部模型** | endpoint、namespace、key、revision 构成资源身份，SAS/预签名 secret 不进入持久化 identity。 |
| L0 传输核心 + L1 provider 数据通道 | **A 保留** | 与当前产品定位一致；安全修复和经批准的传输增强都留在这两层。 |
| L2 凭证桥接层 | **C 门控** | 核心先接收应用 credential source；只有重复需求成立才建独立 adapter。 |
| L3 对象层、L4 专业层、L5 管理层 | **D 当前不立项** | 分别交给 OpenDAL/官方 SDK、专用产品和 IaC；若产品转型则建立独立 crate/RFC。 |
| 公共 `ObjectStore` + put/get/head/list/delete/copy | **D 当前 No-Go** | 这是产品转型，不是传输自然补全；OpenDAL 和官方 SDK 已覆盖。 |
| `rusty-cat-opendal` 桥接 | **C 候选替代** | 若用户需要“对象选择/列举 + rusty-cat 可靠传输”，桥接比重复实现两套对象 SDK 更小；必须先验证 API 可组合性。 |
| 内部证据保留与 `CloudTransferError` | **A 核心** | 结构化保留 provider code、request ID、revision 和实际校验状态，服务重试与诊断。 |
| 新增公共版本化 `TransferReceipt` payload | **B 窄实现** | 以可选、版本化 payload 演进现有 `TaskOutcome`，避免直接增加大量必填字段。 |
| 大型运行时 `ObjectCapabilities` | **D 当前不做** | 权限与账户配置无法被可靠预知，容易产生虚假保证；等对象平台化后再设计 Unknown 状态。 |
| `StorageTier/Presigner/CopyOperation/ListPager` 通用抽象 | **D 当前不做** | 都依赖未获批对象能力，且云差异大。 |
| 一次建立十多个 Cargo feature | **D** | 增加组合编译、文档和 semver 负担；功能获批并实现时再加。 |
| 弃用 `aliyun-oss`/`azure-blob` 别名 | **C 暂缓** | README 已说明行为，当前没有真实误用证据；若改名应收集支持数据并走 major release。 |
| 安全自动 chunk size | **B** | 先做 provider 上限满足和 checked arithmetic，不承诺网络自适应最优。 |
| 自适应并发/局部 Range/自动 endpoint 选择 | **C** | 必须有可复现 workload 和预设收益阈值；显式配置在当前阶段更可预测。 |

### 13.9 立项与反转门槛

以下数量阈值是**建议的产品政策**，不是外部事实，也不是统计学定律。其作用是防止单个 issue 或“官方支持”直接变成永久公共 API。

#### Gate A：现有安全与正确性

无需用户需求证明，直接 Go。必须满足：Azure cancel 不改变任务前 Blob；两个 writer 不串片；授权请求只发往允许的 HTTPS resource；最终请求再签名；secret 不进入日志/checkpoint；零字节、provider 边界、有界 XML 和不确定完成均有测试。

#### Gate B：新的传输能力

checkpoint、凭证 source、checksum 策略或新的 revision/options 同时满足以下条件才 Go：

- 至少两个独立下游项目，或一个已承诺上线且失败成本明确的生产集成；
- 有具体调用流程和现有做法的可观察成本，而不是“以后可能需要”；
- 有 API/schema 兼容 owner、两家云中相关 provider 的真实服务 CI，以及失败降级策略；
- 不能用很少的应用代码或已有 SDK 更安全地解决。

这组证据会把 13.1/13.2/13.5 中相应 C 项提升为 B；若 direct 模式实际使用率极低，也可让 checkpoint/credential source 延后。

#### Gate C：通用对象 SDK 转型

只有以下条件全部满足，才重开第 7 章 `ObjectStore` 设计：

- 产品负责人明确决定从“传输 SDK”扩为“对象存储 SDK”；
- 至少三个独立用户需要高度重叠的对象工作流，而不是各要一个互不相关 API；
- 用户明确希望由 `rusty-cat` 而非 OpenDAL/云厂商 SDK 承担这些操作；
- 有阿里云与 Azure 长期 live test、文档、权限和兼容维护预算；
- 完成主 crate、桥接 crate、独立对象 crate 三种方案比较。

未满足时，list/delete/copy/tags、阶段 2 和控制面均保持 No-Go。

#### Gate D：性能增强

并行、自适应、Multi-Range、secondary/acceleration 需要可复现 workload、固定配置基线，以及吞吐、P95 延迟、峰值内存、请求成本和错误率联合指标。可先把“目标 workload 至少约 20% 吞吐改善且不增加错误率/成本失控”作为提案阈值，实际数值应由维护者在 benchmark RFC 中确认。

#### Gate E：高风险能力

WORM/immutability、CPK、客户端加密、自动清理、break lease、永久删除等必须有具名生产需求、独立 feature/crate、最小权限模型、审计/显式确认、真实隔离账户和专项安全评审。密码学能力还要有跨 SDK 互操作测试向量和长期格式兼容 owner。

### 13.10 最终 Go/No-Go 汇总

**现在 Go**：第 9 章阶段 0 的安全修复；保留并回归测试已有源文件代际与内存预算。

**收集证据后 Go**：并行 Range、direct checkpoint、transactional checksum、可刷新 credential source、`UploadMode::Auto`、最小 revision/condition/receipt 和上传期 metadata。

**当前 No-Go**：通用对象 CRUD、预签名/SAS 签发、完整身份 SDK、Bucket/Container/ARM 控制面、网站/媒体/查询/事件、Page Blob/HNS、WORM 与客户端加密。若未来需求成立，优先独立 adapter/crate，而不是继续加宽核心 trait。

## 14. 主要依据与官方文档索引

### 项目定位、需求信号与替代方案

- [`rusty-cat` README（产品定位与职责边界）](../README.md)
- [`rusty-cat` Cargo.toml（包描述、features 与依赖）](../Cargo.toml)
- [crates.io：rusty-cat](https://crates.io/crates/rusty-cat) 与 [公开 API 快照](https://crates.io/api/v1/crates/rusty-cat)（2026-09-03 需求信号快照；下载量不能单独证明需求）
- [GitHub 仓库](https://github.com/0barman/rusty-cat) 与 [公开 issues](https://github.com/0barman/rusty-cat/issues)（没有公开功能请求不等于没有私有需求）
- [Apache OpenDAL Rust 文档](https://opendal.apache.org/docs/rust/opendal/)、[服务列表](https://opendal.apache.org/docs/rust/opendal/services/index.html)、[Azure Blob](https://opendal.apache.org/docs/rust/opendal_service_azblob/index.html) 与 [OSS](https://opendal.incubator.apache.org/services/oss/)（通用对象操作的现成替代）
- [Microsoft：安装稳定版 Azure SDK for Rust](https://learn.microsoft.com/en-us/azure/developer/rust/sdk/installation)

### 阿里云 OSS

- [OSS API 按功能分类](https://www.alibabacloud.com/help/en/oss/developer-reference/list-of-operations-by-function)
- [Multipart upload 使用指南](https://www.alibabacloud.com/help/en/oss/user-guide/multipart-upload)
- [InitiateMultipartUpload](https://www.alibabacloud.com/help/en/oss/developer-reference/initiatemultipartupload)
- [UploadPart](https://www.alibabacloud.com/help/en/oss/developer-reference/uploadpart)
- [CompleteMultipartUpload](https://www.alibabacloud.com/help/en/oss/developer-reference/completemultipartupload)
- [ListParts](https://www.alibabacloud.com/help/en/oss/developer-reference/listparts)
- [ListMultipartUploads](https://www.alibabacloud.com/help/en/oss/developer-reference/listmultipartuploads)
- [GetObject](https://help.aliyun.com/zh/oss/developer-reference/getobject)
- [CRC64 数据完整性校验](https://help.aliyun.com/zh/oss/user-guide/check-data-transmission-integrity-by-using-crc-64)
- [Signature V4 URL](https://www.alibabacloud.com/help/en/oss/developer-reference/add-signatures-to-urls)
- [STS 临时访问凭证](https://www.alibabacloud.com/help/en/oss/developer-reference/use-temporary-access-credentials-provided-by-sts-to-access-oss)
- [基础对象操作](https://www.alibabacloud.com/help/en/oss/developer-reference/basic-operations-1)
- [数据加密](https://www.alibabacloud.com/help/en/oss/user-guide/data-encryption/)

### Microsoft Azure Blob Storage

- [Blob Storage REST API](https://learn.microsoft.com/en-us/rest/api/storageservices/blob-service-rest-api)
- [Operations on blobs](https://learn.microsoft.com/en-us/rest/api/storageservices/operations-on-blobs)
- [Operations on containers](https://learn.microsoft.com/en-us/rest/api/storageservices/operations-on-containers)
- [Put Blob](https://learn.microsoft.com/en-us/rest/api/storageservices/put-blob)
- [Put Block](https://learn.microsoft.com/en-us/rest/api/storageservices/put-block)
- [Put Block List](https://learn.microsoft.com/en-us/rest/api/storageservices/put-block-list)
- [Get Block List](https://learn.microsoft.com/en-us/rest/api/storageservices/get-block-list)
- [Get Blob](https://learn.microsoft.com/en-us/rest/api/storageservices/get-blob)
- [Get Blob Properties](https://learn.microsoft.com/en-us/rest/api/storageservices/get-blob-properties)
- [Blob 条件请求](https://learn.microsoft.com/en-us/rest/api/storageservices/specifying-conditional-headers-for-blob-service-operations)
- [Microsoft Entra ID 授权](https://learn.microsoft.com/en-us/azure/storage/blobs/authorize-access-azure-active-directory)
- [SAS 总览](https://learn.microsoft.com/en-us/azure/storage/common/storage-sas-overview)
- [User delegation SAS](https://learn.microsoft.com/en-us/rest/api/storageservices/create-user-delegation-sas)
- [Blob access tiers](https://learn.microsoft.com/en-us/azure/storage/blobs/access-tiers-overview)
- [Immutable storage](https://learn.microsoft.com/en-us/azure/storage/blobs/immutable-storage-overview)
