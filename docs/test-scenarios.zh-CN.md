# rusty-cat 测试场景与验证矩阵

[English](test-scenarios.md)

本文按调用方可观察到的公开行为整理 `rusty-cat` 的测试覆盖、关键安全边界和最近一次验证结果。离线自动化与可运行端到端场景分别列出，避免把本地模拟、真实服务验证和未执行项混为一谈。

本文不会记录凭据、账号或存储空间标识、服务地址、对象名称、签名 URL、令牌、原始校验器或内容摘要。真实服务结果是 2026-08-27 的验证快照，会受到外部服务、临时授权和网络环境影响，并不承诺第三方服务持续可用。

“最新验证结果”表示该场景在 2026-08-27 验证会话中的最后一次实际运行结果，并不表示每次后续代码变化都会重跑全部真实服务场景；最终修订已重新执行两个 workspace 的默认自动化回归和真实 `azure-direct` 验收。

## 验证快照

以下命令均从仓库根目录运行：

- `cargo test --manifest-path test-app/Cargo.toml --workspace`：176 项通过，0 项失败，4 项显式忽略。
- `cargo test --workspace --all-features`：根 workspace 的所有非忽略单元、集成和文档测试通过，其中包括 270 项库单元测试和 116 项文档测试。
- 默认忽略的 100 轮随机进程强杀恢复 soak 曾单独运行并通过；后续下载请求构建调整后未再次运行该 soak。
- 4 个默认忽略项不是隐藏失败：其中 2 项是真实后端契约，1 项是 100 轮 soak，1 项仅供父测试启动子进程。

测试数量会随用例增加而变化；上面的命令和 CI 配置始终是当前事实来源。
表格中的源码证据链接以完整仓库 checkout 为基准；单独发布的 crate 归档不包含 `test-app`。

## 自动化公开行为矩阵

默认自动化测试以公开行为契约为主，并辅以 crate 内部纯函数单测；网络型用例只使用随机 loopback HTTP(S) 服务、虚构账号和确定性测试数据，不需要外网或真实云凭据。

| 场景 | 详细覆盖 | 核心断言与安全边界 | 最新验证结果 |
|---|---|---|---|
| [公共 API 与配置](../../test-app/tests/public_api_surface.rs) | Builder、getter、上传与下载选项、Binary 配置、Provider plan helper、状态、错误码和结构化日志字段 | 合法值可完整回读；零值、越界值和非法组合在任务执行前返回稳定错误 | 通过（默认离线回归） |
| [非法输入与上传源](../../test-app/tests/invalid_input_api.rs) | 上传和下载必填字段、文件与内存字节源、source override、零字节上传、文件缺失或构建后删除 | 无效输入不得触发网络 I/O 或成功回调；错误类型和错误码保持可诊断 | 通过（默认离线回归） |
| [日志与敏感信息保护](../../test-app/tests/logging_api.rs) | Listener 注册、替换、清除、延迟构造、重复注册、URL 与自由文本脱敏、回调 panic 隔离 | 日志和错误链不得出现原始签名参数、凭据或令牌；Listener panic 不得中断传输 | 通过（默认离线回归） |
| [Binary 下载](../../test-app/tests/binary_api.rs) | 内存响应、Content-Type、自定义 Header、响应大小上下限、重定向、超时、断连重试、取消、关闭和并发限制 | 跨源重定向移除敏感 Header；超限响应失败关闭；失败尝试的部分数据不得混入成功结果 | 通过（默认离线回归） |
| [默认上传与 Range 下载](../../test-app/tests/transfer_protocol_api.rs) | 内存上传、prepare 快速完成与独立重试、JSON 计划、方法与 Header、自定义 HTTP client、已知大小跳过 HEAD、串并行 Range 和 HTTP 错误矩阵 | 分片偏移、长度和拼装结果 byte-exact；`Content-Range` 必须匹配；硬 4xx 快速失败，瞬态错误按预算重试 | 通过（默认离线回归） |
| [并发下载与对象代次](../tests/concurrent_download_test.rs) | 串行和并行 Range、乱序完成、强 ETag、Azure 非引号 ETag、`If-Match`、响应 ETag 变化、同目标文件租约和 checkpoint 恢复 | 不同远端代次的字节不得混合；缺少可证明校验器时失败关闭或安全重下；Provider 不得删除、替换或追加执行器准备的 `If-Match` | 通过（39 项聚焦回归） |
| [自定义传输协议](../../test-app/tests/custom_protocol_api.rs) | Download 全部 hooks、独立 URL/Header/大小解析/恢复身份；Upload prepare/chunk/complete、上下文 getter、串行降级和并行 opt-in | 所有 hook 接收准确任务与分片上下文；未声明并行能力时不得并行；完成 payload 原样传递 | 通过（默认离线回归） |
| [Client 生命周期与控制](../../test-app/tests/client_control_api.rs) | active/queued 快照、pause、resume、cancel、close、全局 Listener、重复任务、命令队列背压和并发关闭 | Task ID 稳定；每个任务只有一个终态；队列满时快速失败；close 不死锁且关闭后拒绝新任务 | 通过（默认离线回归） |
| [预签名生命周期](../../test-app/tests/presigned_lifecycle_api.rs) | multipart part 与 ETag、内置或自定义 complete body、complete 非 2xx、abort、上传和下载 URL 刷新、已提交分片恢复 | 过期 URL 只按协议刷新；恢复不得重复上传已确认分片；取消只 abort、不 complete | 通过（默认离线回归） |
| [Checkpoint 正常恢复](../../test-app/tests/checkpoint_safety_api.rs) | 强 ETag 绑定、只补缺片、目标删除或截断、远端换代、成功后 sidecar 清理 | 只复用摘要和对象身份均可证明的分片；目标或远端代次不可信时安全全量重下 | 通过（默认离线回归） |
| [真实进程崩溃恢复](../../test-app/tests/process_crash_resume_api.rs) | 子进程不调用 `close()` 即被强制终止；覆盖串行前缀、并行离散分片、sidecar 发布前崩溃、远端换代和文件锁释放 | 新进程只复用已持久化且可证明的分片；发布前崩溃从零恢复；成功后清理 sidecar；锁可立即重新获取 | 当前确定性用例通过；100 轮显式 soak 此前专项验证通过，最终修订未重跑 |
| [Checkpoint 对抗性损坏](../../test-app/tests/checkpoint_adversarial_api.rs) | 破坏头部、长度、位图、摘要、尾字节和目标内容；覆盖超大稀疏快照、网格切换、临时文件、私有命名空间、目录与符号链接冲突 | 伪造完成位不能跳过缺片；只复用摘要可证明的分片；冲突路径失败关闭且不改写用户文件或哨兵 | 通过（默认离线回归） |
| [上传源代际一致性](../../test-app/tests/active_upload_source_mutation_api.rs) | build/排队后以及 prepare、串行、并行、pause/resume、finalize 和 cancel 期间覆盖、截断、扩展、替换或重命名源文件 | 不同文件代际的字节不得混传；相同内容的物理换版可以继续；异常后先收敛在飞 future，再只 abort、不 complete | 通过（默认离线回归） |
| [上传失败诊断](../../test-app/tests/upload_source_safety_api.rs) | 完全或部分截断、同长度换版、主错误与 abort 清理错误组合 | 只允许完整且属于原代次的分片出站；清理失败不得覆盖主错误或泄露敏感诊断 | 通过（默认离线回归） |
| [Direct Provider 通用契约](../../test-app/tests/provider_direct_api.rs) | Aliyun OSS V4 与 Azure Shared Key 请求、并行分片上传、complete、abort、Range 下载、取消清理和非法 Azure key | 分片偏移与请求体准确；清理请求同样签名；非法 key 在网络 I/O 前失败；上传与回下载 byte-exact | 通过（7 项聚焦回归） |
| Azure 条件 Range 签名（[固定向量](../src/azure-blob-direct/signing.rs)、[串并行契约](../tests/concurrent_download_test.rs)、[独立 wire 验签](../../test-app/tests/provider_direct_api.rs)） | 固定签名向量、HEAD 与首片 ETag 锁定、`Range` 和 `If-Match` 的最终请求签名、已知大小免 HEAD 路径 | 独立验签器必须拒绝签名后篡改 `Range` 或 `If-Match`；串行和并行执行器路径中的 Provider hook 均在签名前看到最终条件头；冲突在发请求前失败 | 通过（离线独立验签及真实 Azure 验证） |
| [TLS、DNS、代理与网络故障](../../test-app/tests/network_fault_api.rs) | 私有 CA 信任与拒绝、域名不匹配、DNS stub、HTTPS CONNECT、代理拒绝、TCP RST、半开、延迟、限速和 wire body 截断 | 不可信 CA 和主机名错误必须在 TLS 握手阶段、进入应用层 HTTP 处理前失败；RST/截断按预算安全重试；失败响应数据不得污染目标文件 | 通过（默认离线回归） |
| [本地 HTTP(S) 故障服务器](../../test-app/test-server/src/lib.rs) | 随机端口 HTTP/HTTPS、并发、延迟、断连、RST、半开、限速、任意状态/Header/Body、CONNECT 代理和请求捕获 | HEAD 不输出 Body；连接并发正确；fixture Drop 可中止延迟响应、关闭活动连接并释放端口 | 通过（默认离线回归） |
| [后端 HTTPS 契约](../../test-app/tests/loonadm_https_backend_contract.rs) | 虚构账号登录、multipart init、并行 PUT、complete、abort、确认分片恢复、控制面错误矩阵和数据面重试分类 | 控制面与数据面均使用受私有测试 CA 保护的 HTTPS；回读 byte-exact；abort 后拒绝继续；恢复不重复上传已确认分片；状态和错误语义保持 | 本地契约通过；真实部署契约另见下表 |

## 可运行端到端与真实服务场景

前 14 个命令入口由同一个 `test-app` 场景调度器管理，最后一项是需显式启用的集成测试。状态记录本次真实运行结果；它们不替代上面的确定性回归，也不保证外部服务以后仍保持相同状态。

| 场景 | 详细覆盖 | 核心断言与安全边界 | 最新验证结果 |
|---|---|---|---|
| [`loonadm`](../../test-app/src/lib.rs) | 后端登录、多个下载来源、multipart init、并行上传、complete/abort、指标及上传后回下载 | 单个下载来源失败不会跳过后续来源，也不会中止上传流程；上传失败执行 abort；只有后端提供可读对象地址或读取权限时才允许把上传后回下载判为通过 | **部分通过，整体失败**：TLS/登录、真实下载、20 分片上传和 complete 已通过；后端只写授权导致上传后回下载 403 |
| [`direct-download`](../../test-app/src/download/direct.rs) | 对调用方提供的 Range 地址执行探针，并比较串行和多路并发下载 | 必须返回合法 206 与 `Content-Range`；配置预期大小或摘要后不一致即失败 | **通过**：串行和 4 路并发均完成，大小与 MD5 一致 |
| [`otacdn-x86-64`](../../test-app/src/scenarios/otacdn_x86_64.rs) | 真实发布制品下载，在非零进度后 pause，确认文件停止增长，再以同一 Task ID resume | 必须真实经历 `Paused`；恢复后完成；最终大小和发布摘要一致；下载文件不执行 | **通过**：pause/resume 轨迹、大小和 MD5 均通过 |
| [`azure-download`](../../test-app/src/download/oss_azure.rs) | 使用只读临时授权执行四路 Range 下载和报告生成 | 校验 206、`Content-Range`、最终文件大小和 300 个分片的完整拼装；报告不得保存签名参数 | **通过**：300 个 Range 分片完成，最终大小一致 |
| [`azure-sas-roundtrip`](../../test-app/src/scenarios/azure_sas.rs) | Put Block、一次有序 Put Block List、并行 Range 回下载和 SHA-256 校验 | 发请求前检查资源范围与读写权限；拒绝覆盖既有对象；上传与回下载大小和摘要必须一致 | **配置阻塞**：本次可用授权只有读取权限，缺少上传所需写权限，未发起上传 |
| [`aliyun-presigned`](../../test-app/src/download/aliyun_presigned.rs) | Aliyun OSS V4 预签名 Range 下载、请求前到期检查和对象大小验证 | 已知过期 URL 在网络请求前拒绝；探针大小、配置大小和落盘大小必须一致 | **通过**：31 个 Range 请求完成，最终大小一致 |
| [`aliyun-direct`](../../test-app/src/scenarios/cloud_direct.rs) | 官方 Aliyun Direct Provider 的 multipart 上传、签名 HEAD/Range 下载和内容回读 | 使用隔离对象；上传后回下载必须 byte-exact；Direct 凭据只适用于可信测试环境 | **通过**：5 MiB、1 MiB 分片、4 路上传及回下载 byte-exact |
| [`aliyun-prepare-files`](../../test-app/src/scenarios/aliyun_upload_matrix.rs) | 生成 0 B、1 B、1 KiB、分片边界前后、5 MiB 边界和多分片尾部的确定性文件 | 同尺寸文件可复用；只访问本地文件系统，不连接云服务；它是 fixture 生成步骤，不是独立传输验证 | **通过**：9 个边界文件生成或复用成功 |
| [`aliyun-upload-matrix`](../../test-app/src/scenarios/aliyun_upload_matrix.rs) | 对边界文件分别执行 Direct 和 Presigned multipart 上传，再回下载检查大小与 SHA-256 | 非空文件必须 byte-exact；0 B 按当前公共契约在发请求前预期拒绝；任一非预期失败使矩阵失败 | **通过**：16 个有效传输通过，2 个 0 B 用例按预期拒绝，0 个非预期失败 |
| [`azure-direct`](../../test-app/src/scenarios/cloud_direct.rs) | 官方 Azure Shared Key Provider 的 Put Block、Block List、HEAD 和条件 Range 回下载 | 最终签名同时覆盖 `Range` 与可信 `If-Match`；首片锁定 ETag 后的后续请求重新签名；上传与回下载 byte-exact | **通过**：5 MiB、1 MiB 分片的真实上传和回下载 byte-exact，原 403 未复现 |
| [`local-http`](../../test-app/src/scenarios/local.rs) | 本地自定义上传与标准 Range 下载，两条任务均真实 pause/resume | 上传内容与服务端接收内容一致；下载 byte-exact；暂停与恢复到达正确状态 | **通过** |
| [`resume-restart`](../../test-app/src/scenarios/local.rs) | 首轮注入 Range 故障并留下可信 checkpoint，随后用新 Client 恢复 | 首轮必须失败；恢复只请求缺失分片；已验证分片不重复下载；最终 byte-exact | **通过**：仅补取缺失分片 |
| [`restore-paused`](../../test-app/src/scenarios/local.rs) | 导入多个暂停任务，并只恢复调用方选中的任务集合 | 导入阶段零 HTTP I/O、零目标文件创建；选中任务完成，未选中任务保持暂停且无文件 | **通过** |
| [`local-all`](../../test-app/src/scenarios/local.rs) | 顺序运行 `local-http`、`resume-restart` 和 `restore-paused` | 聚合入口不重复计算为第四套覆盖；任一子场景失败即整体失败 | **通过**：三个子场景全部通过 |
| [`loonadm_live_contract`](../../test-app/tests/loonadm_live_contract.rs) | 真实部署的登录/init/abort/鉴权拒绝，以及首片确认落盘后重建任务、跳过已确认前缀并 complete | 仅允许专用测试账号显式 opt-in；已确认分片不得重复上传；回执可验证时检查大小或 byte-exact | **未执行**：2 项真实部署契约需要专用配置并显式启用 |

`full` 只是 `loonadm` 的兼容别名，`help` 不是测试场景；二者不重复计入覆盖数量。

## 如何复现

先分别运行根 workspace 和独立 `test-app` workspace 的确定性自动化回归：

```bash
cargo test --workspace --all-features
cargo test --manifest-path test-app/Cargo.toml --workspace
```

发布前可额外运行默认忽略的 100 轮进程崩溃恢复 soak：

```bash
cargo test --manifest-path test-app/Cargo.toml \
  --test process_crash_resume_api \
  randomized_process_crash_soak_100_rounds \
  -- --ignored --nocapture --test-threads=1
```

再按需运行某个显式端到端场景：

```bash
cargo run --manifest-path test-app/Cargo.toml -- <scenario>
```

真实服务入口可能创建隔离测试对象，应只使用专用测试资源和最小权限配置。默认自动化测试不会访问这些服务，也不会把未执行的真实服务场景算作通过。
