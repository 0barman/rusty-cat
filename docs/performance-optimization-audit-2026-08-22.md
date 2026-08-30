# rusty-cat 全量性能审计与优化计划

- 审计日期：2026-08-22（Asia/Singapore）
- 审计版本：`36d60a654202`，`rusty-cat 0.2.4`
- 环境：Apple M2 Pro / arm64 / macOS 26.2 / Rust 1.88.0 / Cargo 1.88.0
- 范围：`rusty-cat` 库、workspace 合同测试与 Loom 模型、根目录演示程序、`test-app`、示例、微基准、live 测试脚本
- 本轮边界：**只做扫描、执行与方案设计；没有修改 Rust 源码、公开 API 或行为。**
- 二次设计复核：2026-08-22；结论是原计划中的 D1/S1 不能按最简方案直接实现，必须先完成下述 D0/U0 正确性前置项和可观测行为冻结。
- 文档修订状态：已把二次复核发现的 checkpoint 死锁、远端/本地换版、上传源变化、跨进程目标竞争、调度顺序和测试完备性缺口纳入正式门禁；仍未开始生产代码优化。

## 1. 结论摘要

当前最高优先级不是调大默认并发，而是消除并发下载路径中的逐分片持久化瓶颈。595.8 MiB、1 MiB/片、本机 Range HTTP 的三轮发布模式测试中，串行下载中位数为 1.001 s（报告值 595.25 MiB/s），并发 2/4/8 的中位数却分别为 8.229/6.215/6.302 s（72.41/95.87/94.55 MiB/s）。所有轮次文件大小和 MD5 一致。结果与代码中的每片 `sync_data`，随后在互斥锁内同步执行 sidecar `create + write_all + sync_all + rename` 完全吻合。

上传路径的首要问题是：网络分片可以并行，但文件源读取被一个 `tokio::Mutex<Option<File>>` 包住的 `seek + read_exact` 串行化。另有上传前必须全文件计算 MD5、调度器反复扫描队列、每个进度事件复制监听器/DTO、预签名下载计划反复深拷贝等次级热点。

二次静态复核还发现两个必须先处理的正确性风险：当前 `.rcdl` 只绑定 range URL、total、chunk、并发数和目标文件长度，不能识别“同 URL、同长度但远端内容已换版”或“本地目标被同长度替换”；上传 MD5 计算完成后，真正上传时又按路径重新打开/读取文件，源文件在两阶段之间变化会使 `file_sign` 与上传内容不一致。它们不是本轮性能测试新引入的问题，但任何性能重构都不能原样放大这些窗口。

**测试状态必须如实区分：现有测试覆盖较广，但目前不能称为“完备”。** 本文列出的 D0/D1/U0/U1 故障注入、跨平台 durability、远端换版、32-bit/溢出等测试尚未实现；它们是后续 TDD 的合入前置条件，不是已经存在的测试。

建议实施顺序：

1. 先修复测试门禁（live 回调 panic、pause/resume 等待、云端清理权限、一次模拟服务偶发失败），冻结公开 API 与可观测行为基线。
2. P0 正确性前置：为 sidecar 增加可证明的远端 generation、completed-part 内容验证、checked arithmetic/资源上限；为上传冻结同一文件 generation。
3. P0 性能：设计有明确 epoch/fence 的批量 checkpoint，同时严格保留“数据先 durable、bit 后 durable”的崩溃一致性顺序。
4. P1：上传文件改为定位读取；调度器先跳过 Progress 后的无效重调度并改 O(1) 活跃计数，只有 profiling 证明必要时才引入保持全局顺序的 ready index。
5. P2：在独立基准支持下优化上传前 MD5 与 binary body 扩容。
6. P3：只有 profile 命中后再优化预签名 plan、进度 fan-out、请求级小对象 clone 和演示程序 DB 写入。

## 2. 优先级清单

| ID | 优先级 | 路径 | 问题 | 主要收益 | API 影响 |
|---|---|---|---|---|---|
| D0 | P0 前置 | 并发下载恢复 | sidecar 未绑定强远端版本，也没有可验证的已完成分片内容摘要；仅靠 URL、长度或本地 metadata 可能复用被改写的数据；part 数与 offset 算术缺少完整上限 | 先消除静默损坏、溢出和 OOM 风险 | 无；内部 sidecar v2，v1 安全降级 |
| D1 | P0 | 并发下载 | 每个 1 MiB part 都做数据 `sync_data`，随后同步 fsync+rename 整个 sidecar，并受单锁串行化 | 大幅降低 syscall/fsync/锁等待；恢复并发下载价值 | 无；只改内部持久化协议 |
| U0 | P0 前置 | 上传一致性 | MD5 阶段与分片读取阶段没有冻结同一文件 generation，源文件变化可使 sign 与上传内容不同 | 防止优化后并行读扩大混合版本窗口 | 无；保持错误类型和 builder 不变 |
| U1 | P1 | 并发上传 | 文件分片的 `seek + read_exact` 共用单文件游标和异步锁，磁盘读取实际串行 | 提高本地高速盘、低延迟网络下的上传吞吐 | 无；保持 `UploadSource` 和 builder 不变 |
| S1 | P1 | 调度器 | 槽位满时整队列 pop/requeue，并逐次扫描 active；每个 worker event 后重跑 | 大队列、多进度事件下减少 O(Q×E) 工作 | 无；必须保持现有“最老可运行任务”顺序 |
| U2 | P2 | 上传建单 | 入队前对整个文件做 MD5，64 KiB buffer；上传要读文件两遍 | 降低 time-to-first-byte 和 CPU/读取开销 | 无；MD5/去重语义必须保留 |
| PR1 | P3（需 profile） | 预签名下载 | 每片锁 `std::sync::Mutex` 并深拷贝 URL/HeaderMap/plan | 降低短分片和高并发时的锁与分配 | 无；公开 `plan()` 返回类型不变 |
| B1 | P2 | binary 下载 | 已知 Content-Length 时初始容量仍封顶 64 KiB，产生增长与复制 | 降低中等大小 body 的 allocator/copy 成本 | 无 |
| E1 | P3 | 进度回调 | 每事件构造 listener Vec，clone 每个 callback 和每个 listener 的 DTO | 高 listener 数/高频事件下减分配和引用计数 | 无；回调顺序和重入语义不变 |
| D2 | P3 | 串行下载 | 持有文件锁覆盖整个响应流，每 chunk 后 flush/metadata 检查 | 降低串行路径 syscall/锁开销 | 无；需先证明不会破坏删除检测和断点语义 |
| A1 | P3 | 根演示应用 | 每个进度帧 `tokio::spawn` 一次并执行一次 SQLite upsert | 防止 demo/consumer 反压掩盖库吞吐 | 不属于库 API；单独改 demo |

## 3. 基线执行结果

### 3.1 自动化测试与质量门禁

| 命令/范围 | 结果 | 说明 |
|---|---|---|
| `cargo test --workspace --all-features --no-fail-fast -- --test-threads=1` | **通过**；80.66 s；峰值 RSS 239,124,480 B | 单元、集成、合同、Loom 与 doctest 全部通过；live 测试仍为 ignored；doctest 116 通过、1 ignored |
| `cargo test --workspace --all-features --all-targets --no-fail-fast -- --test-threads=1` | 产品测试通过；bench target 参数冲突 | `--test-threads=1` 被转发给 `harness=false` 的 Criterion，Criterion 报未知参数；不是产品失败，基准已用独立命令补跑 |
| `azure_direct_parallel_test` | 首次全量运行 1 次 `IncompleteMessage`；隔离连续 5 次通过；完整复跑通过 | 当前属于模拟 HTTP 服务/连接 framing 的偶发不稳定，必须在性能改动前消除 |
| `cargo test --manifest-path test-app/Cargo.toml -- --test-threads=1` | **17/17 通过**；9.35 s（含编译） | test-app 单元测试通过 |
| `cargo clippy --workspace --all-features --all-targets -- -D warnings` | **失败：59 个诊断** | 多数是 `uninlined_format_args`、`manual_inspect` 等既有 lint；不是本轮改动引入，但说明 CI 尚不能用 `-D warnings` 作门禁 |

注意：第一次 all-targets 命令在偶发测试失败后提前停止，不能取代随后退出码为 0 的标准全量命令。Criterion 必须单独运行，不能附加 libtest 参数。

### 3.2 示例与演示程序

| 项目 | 结果 | 可用于性能结论？ |
|---|---|---|
| `http_local_chunk_transfer` | 通过；包含 5 MiB 上传和 5 MiB 下载 | 只能作正确性 smoke；未分离上传、下载、MD5 和调度时间 |
| `resume_after_restart` | 通过；从 102,400/262,144 B 恢复，最终 byte-exact | 可作恢复正确性基线，数据量太小不能测速 |
| `restore_import_paused` | 通过；导入 3 个 paused，恢复 2 个，另 1 个未发生 I/O | 可作状态机基线，不能测速 |
| 6 个 Aliyun/Azure provider example | 均成功启动后因占位凭证/URL而安全跳过 | 只证明示例可构建和占位检查有效；没有真实吞吐 |
| `binary_download_demo` | 通过；下载 103,840 B JPEG | 对象太小，且首次 release 编译占主要墙钟，不用于吞吐 |
| 根 `rusty-cat-test` demo | 进程退出 0；3.56 MB 小文件完成，314 MB 文件按示例逻辑取消 | 回调线程反复 panic（没有 Tokio reactor），不能视为健康通过或性能样本 |
| `test-app` 默认远端流程 | 登录请求失败；未进入上传 | 无远端上传速率 |
| `test-app` 旧 314 MB 公网 OSS 地址 | 探针返回 403 | 无公网下载速率；不能继续把该地址称为公开可读 |

`http_local_chunk_transfer` 在 release 编译完成后的四次总墙钟为 0.49/0.60/0.50/0.50 s。它合并了建文件、MD5、5 MiB 上传、5 MiB 下载和清理，因此不得用 10 MiB / 墙钟伪装成上传或下载吞吐。

### 3.3 发布模式下载矩阵

测试对象使用仓库现有 624,788,675 B（595.8 MiB）文件；本地 Node Range server 只提供 HTTP 206，客户端和服务端位于同一台机器；分片大小 1 MiB。顺序经过轮换，避免只看固定顺序。报告源码以 1024² 计算，虽然字段名/输出写作 `MB/s`，本文按 **MiB/s** 解读。

| concurrency | 三轮墙钟 (ms) | 中位数 (ms) | 中位数吞吐 (MiB/s) | 相对串行吞吐 | size/MD5 |
|---:|---:|---:|---:|---:|---|
| 1 | 1118, 811, 1001 | **1001** | **595.25** | 1.00x | 全部一致 |
| 2 | 8813, 8229, 6267 | **8229** | **72.41** | 0.12x | 全部一致 |
| 4 | 5830, 6215, 12657 | **6215** | **95.87** | 0.16x | 全部一致 |
| 8 | 6718, 6302, 6189 | **6302** | **94.55** | 0.16x | 全部一致 |

这不是公网网络结论：本地回环和文件缓存放大了落盘/持久化开销，正适合定位客户端上限。三轮样本仍偏少，且 c=4 有明显离群值；实施阶段应至少 warm-up 1 次、正式 5–10 次、报告中位数与 p95。

### 3.4 Criterion 微基准

独立执行 `cargo bench -p rusty-cat --bench transfer_hotspots -- --noplot`：

| benchmark | 结果 | 判断 |
|---|---:|---|
| `chunk_buffer/resize_truncate_per_chunk` | 约 419 ps，报告 4552 GiB/s | **无效基准**；核心工作被编译器消除，不能证明 buffer 快 |
| `md5_stream/consume_64k_chunks` | 6.785 ms / 4 MiB，约 589.55 MiB/s | 可作为纯 MD5 CPU 基线；不包含文件系统读取 |
| `global_listener_fanout/snapshot_and_invoke_32_callbacks` | 118.50 ns | 当前微负担较小；需要加入真实 DTO clone、dispatcher 和多线程竞争后再决定优先级 |

### 3.5 ignored live 测试

`scripts/run_live_tests.sh` 计划运行 7 个 target。实际结果：

- `aliyun_live_upload_resume` 失败：回调线程调用 `tokio::spawn` 时没有 reactor；pause 后立即 resume 还命中“target is still stopping”。
- `aliyun_live_download_resume` 同样失败。
- `aliyun_live_edge_cases` 启动后出现相同回调 panic、DB 状态等待超时；同时云端清理请求返回 403。
- 为避免继续创建无法由测试账号回收的远端对象/分片，本轮主动中止；其余 4 个 target 未执行。

因此不能宣称 live 测试全量通过。中止是外部状态保护，不是跳过失败。运行期间可能已经在 `rusty-cat-live/aliyun/` 前缀下产生完整对象或未完成 multipart；需要有 bucket 管理权限的人检查并清理。

## 4. 详细优化方案与 TDD 要求

### D0（P0 前置）：先证明 sidecar 属于同一远端对象，并验证本地已完成内容

#### 二次复核证据

- `src/dflt/default_http_transfer.rs:408-417` 的 identity 只是 range URL 的 64-bit FNV-1a；HEAD 得到的 ETag 在 `:679-705` 只被记录日志，没有进入 sidecar，也没有用于 `If-Match`/`If-Range`。
- `src/dflt/download_progress.rs:74-105` 仅以 identity、total、chunk、`max_parts` 和目标长度决定是否信任 bitmap。同 URL、同长度的远端内容换版，或本地文件被同长度替换/原位修改时，旧 bit 仍可能被信任，最终文件可能混合两个 generation。
- 预签名 URL 的 query 刷新后，即使对象未变，URL hash 也会变化，导致安全但不必要的全量重下。反过来，不能简单删除 query 后复用 sidecar，因为没有对象 validator 时会把安全退化为静默拼接风险。
- `max_parts_in_flight` 被写进 sidecar，但分片网格实际由 total + chunk 决定；只调整并发度也会使断点失效。
- `part_count_for` 在 u64→usize 失败时饱和为 `usize::MAX`，随后直接分配 bitmap；`max_parts as u32` 会截断；`offset + chunk`、`start + chunk - 1` 等位置存在边界溢出风险。恶意/错误 Content-Length 和极端配置可能造成 panic、OOM/abort 或错误 range。

#### 安全设计约束

D1 之前先定义内部 sidecar v2，不改变任何公开类型：

1. sidecar generation 优先绑定强 ETag/version-id；弱 ETag、Last-Modified 的可接受条件必须明确。Range 请求携带相应条件头，并验证每个响应仍属于同一 generation。
2. 若 provider/`with_total_size` 路径拿不到可证明稳定的 validator，则跨进程恢复必须安全降级：重新下载，或先重新验证所有拟复用 part；不能仅凭 URL+长度猜测内容没变。
3. 有 `client_file_sign` 时可把它加入 identity，但除非库实际验证最终内容，否则不能把“用户提供了 sign”等同于已验证。
4. 文件 ID、size、mtime/ctime 等 metadata 只能作快速拒绝条件，**不能作为内容未变化的充分证明**：外部进程可以原位写回同长度内容，某些文件系统时间戳粒度较粗，甚至可以恢复时间戳。若要在恢复时直接信任 completed bit，sidecar v2 必须同时保存每个 committed part 的强内容摘要，并在复用前重新读取对应区间验证；另一种安全但可能更慢的路径是使用可信整对象摘要，在拼装完成后强制验证，失败时删除结果/sidecar 并从零重下，且绝不能先发 Complete。FNV/路径 hash 不能充当内容摘要。摘要算法、sidecar 体积和恢复扫描成本必须单独 benchmark，并受 part 数/sidecar 大小上限约束。
5. v1 sidecar 只能安全读取后降级为 fresh，或提供有证明的迁移；升级/降级都不得把旧 bit 当成 v2 已验证 bit。
6. v2 不再把纯调度参数 `max_parts_in_flight` 当作内容 identity；改变 chunk/total/validator 仍必须失效。
7. 所有 part count、byte length、offset、end、capacity 和 `usize/u32` 转换使用 checked arithmetic + `try_reserve`/显式错误；不得以 `usize::MAX` 继续分配。
8. 明确 Range 与 content-encoding 的关系；压缩后的 representation、ETag 变化和 CDN 多副本不一致都必须拒绝混拼。
9. 同一目标 path 需要进程内跨 client、必要时跨进程的独占 lease/lock，或第二个任务受控失败。当前 task mutex 只保护单个任务，两个 client/进程仍可同时写目标和固定 `.rcdl.tmp`。lease 必须处理 symlink/hardlink/大小写等路径别名并有 stale-owner 恢复，不能因崩溃永久锁死。

这里的本地摘要用于发现误写、并发写、bit rot 等非恶意变化。若威胁模型包含“攻击者同时改写目标文件和 sidecar”，未加密钥的摘要不能提供真实性；这种场景只能依赖来自可信远端的签名/MAC/摘要或重新下载。实现和测试不得把普通 hash 宣称为防篡改认证。

#### TDD

- 相同 URL/total，服务端从 payload A 切到同长度 payload B：恢复必须全新下载或报 generation mismatch，最终不能 A/B 混合。
- 预签名 query 变化但强 ETag 不变：可安全恢复；ETag 变化则所有旧 bit 失效。
- 本地目标被同长度 replace、原位改 1 byte、truncate 后再扩回原长度：旧 bit 均不可直接信任；即使把 size/mtime 恢复为原值，恢复扫描也必须通过 part 摘要发现变化。
- 篡改一个已 committed part 的首/中/末字节以及摘要字段本身；不得信任损坏摘要，也不得只抽样验证部分 completed range。
- weak ETag、无 ETag、Last-Modified 秒级碰撞、HEAD 与 GET validator 不一致、某一 part 来自不同 CDN generation。
- `Content-Encoding: gzip/br`、代理改写 ETag、206 不带 validator、服务器错误返回 200。
- sidecar v1→v2、v2→旧版本安全降级、未知 version、损坏 header、遗留 `.tmp`。
- 两个 `MeowClient`、两个进程同时下载同一 target（相同/不同 URL、相同/不同 total），以及 symlink/hardlink/大小写别名指向同一文件；只能有一个 owner，失败方不得改动 target/sidecar。owner crash 后 lease 可安全恢复。
- total/chunk/max_parts 覆盖 0、1、`u32::MAX`、`usize::MAX`、`u64::MAX` 邻域；32-bit target 编译和测试；任何输入都只能返回受控错误，不能 panic/OOM/offset wrap。

### D1（P0）：批量持久化并发下载 part 和 sidecar

#### 证据

- `src/dflt/default_http_transfer_chunks.rs:876-937`：每个 part 都 open、metadata、seek、write，并在返回前 `sync_data().await`。
- `src/dflt/default_http_transfer.rs:812-820`：每个 part 完成后获取 `download_progress` mutex，在锁内调用同步函数。
- `src/dflt/download_progress.rs:189-213`：每次置一个 bit 都重新 encode 整份 sidecar，`std::fs::File::create`、`write_all`、`sync_all`、`rename`；同步文件 I/O 会直接占住 Tokio worker 和 mutex。
- 本地矩阵中并发路径只有串行路径约 12%–16% 的吞吐，文件 MD5 一致，强烈指向持久化固定成本而非网络或数据错误。

596 个 part 当前至少触发约 596 次 data sync + 596 次 sidecar file sync + 596 次 rename；还不含 open/stat/seek/write。分片越小，放大越严重。

#### 不改变 API 的方案

在 D0 完成后增加内部 `DownloadCheckpointCoordinator`（名称仅建议），把“part 网络完成”“数据已写入”“generation 已 durable”和“可对外确认 committed”分开。原文仅写“按 N/M/T 批量”不够严谨；必须先定义如下 epoch/fence：

1. part 仍按 offset 定位写，完成后以当前 epoch 提交 `(offset, len)`；coordinator 在锁内 freeze 该 epoch 后，后续写入只能进入下一 epoch，不能越过 barrier 被误标 committed。
2. 单个 epoch 只能有一个 flusher。达到安全阈值、最后一片、窗口即将因等待 checkpoint 耗尽，或定时器触发时执行 durability barrier。
3. **仅在该 epoch 的所有写入完成且 barrier 成功后**，才把该 epoch 的 bit 与对应内容摘要一起合入同一份 sidecar snapshot；sidecar 原子替换成功后才把 part 状态改为 committed、唤醒 waiter 和推进对外 watermark。不得出现“bit 已提交但摘要仍属于上一代”或相反的半更新状态。
4. 阈值不得大于可取得进展的在飞窗口，最后不足一批也必须触发。若每个 part 等待 checkpoint 才释放窗口，而 batch threshold 大于窗口，会形成永久死锁；实现必须以状态机测试证明不会发生。
5. sidecar 使用 generation 唯一临时名，写入、fsync、平台正确的 atomic replace；Unix 需要在要求 crash durability 时同步父目录，Windows 需要验证“目标已存在”和杀毒软件占用时的 replace 行为，并清理 stale temp。
6. checkpoint 尚未落盘时进程崩溃，只会重复下载该 epoch，绝不能跳过未 durable 的数据。
7. pause 必须在发出 Paused 前把已承诺保留的完成 part 收敛为 committed；cancel 是“提交已完整写入 part”还是“安全丢弃 pending bit”需冻结为与现状一致的策略，并设置可测试的最大取消延迟。complete/close 必须等待所有后台 flusher quiescent，不能留下访问已 drop task 的任务。
8. 同步 sidecar I/O 放入有界 `spawn_blocking` 或专用持久化线程，避免阻塞 Tokio worker；队列也必须有界并对 backpressure/cancel 明确定义。

不要简单删除 `sync_data` 或先写 sidecar bit。那会把性能问题变成断点续传静默损坏。也不要假定一个 handle 的 sync 在所有平台都覆盖其他 handle 的写入；如果无法证明跨 handle barrier，应使用拥有明确写入/同步所有权的单文件 writer/coordinator，而不是依赖平台偶然行为。

#### TDD（先红后绿）

先增加可注入的内部 persistence backend，仅用于精确故障点测试；不进入公开 API。

- `crash_before_data_barrier_redownloads_all_uncommitted_parts`
- `crash_after_data_barrier_before_sidecar_redownloads_but_never_skips`
- `crash_after_sidecar_fsync_before_rename_uses_last_valid_generation`
- `crash_after_rename_recovers_exact_committed_bitmap`
- `out_of_order_batch_resume_produces_byte_exact_file`
- `pause_and_close_force_checkpoint_while_cancel_follows_frozen_policy`
- `checkpoint_failure_surfaces_io_error_and_keeps_resume_safe`
- `corrupt_or_truncated_sidecar_starts_safe_fresh_download`
- `batch_threshold_greater_than_window_cannot_deadlock`
- `final_partial_batch_is_committed_without_waiting_forever`
- `part_arriving_during_frozen_epoch_belongs_only_to_next_epoch`
- `only_one_flusher_runs_and_all_waiters_wake_on_success_or_error`
- `cancel_during_write_barrier_sidecar_fsync_replace_and_parent_fsync`
- `windows_replace_existing_sidecar_and_stale_temp_recovery`
- 多进程 kill/restart 黑盒测试：在随机 part/故障点杀进程，恢复后 size、MD5、逐字节均一致。
- property test：任意 part 完成顺序与任意 crash 边界下，sidecar 的 set bits 必须是 durable parts 的子集。

性能验收：同一 595.8 MiB fixture、1 MiB part、release、相同机器，正式 5–10 次。主对照必须是“优化前并发路径 vs 优化后相同 durability 语义”，串行路径只作诊断，因为当前串行 `flush` 与并发 `sync_data+sync_all` 并非相同 durability 等级，不能用 c=4 必须达到 c=1 的固定比例作为唯一合入门槛。要求 size/MD5/逐字节全部一致、checkpoint syscall 明显下降；具体吞吐阈值在加入 syscall 计数和稳定 5–10 轮基线后冻结。公网限速场景另测 c=1/2/4/8，避免只针对回环优化。

### U0（P0 前置）：冻结上传源文件 generation

#### 二次复核证据

`src/inner/inner_task.rs:97-165` 先打开文件并计算 MD5；真正分片读取时，`src/dflt/default_http_transfer_chunks.rs:129-170` 按保存的 path 延后打开/复用另一个 handle。源文件若在 hash 后被 replace、truncate 后扩回、同长度原位修改，公开记录的 `file_sign` 可能不再代表实际上传字节。并行定位读取会让多个 part 更快地同时观察变化，可能把不同时间点的数据混入一个 multipart。

#### 不改变 API 的安全前置方案

- hash 与上传读取应引用同一个稳定打开的普通文件 generation，而不是 hash 后再按 path 无条件重开；记录平台 file ID、size、mtime/ctime 等可用证据。
- 在 prepare 前、每次异常短读时和 complete 前重新验证 generation。检测到变化时停止派发、等待在飞任务、调用 provider abort，并返回现有兼容错误类别；不能 complete 一个 sign 与内容不一致的对象。
- 普通 metadata 无法对所有文件系统和恶意“修改后还原”给出数学证明。若产品需要强快照语义，只能使用平台 snapshot/clone、应用保证源不可变，或付出完成前复验成本；文档和测试必须明确保证边界，不能宣称 metadata 检查解决所有并发写入。
- 第一阶段禁止引入 hash cache。inode/size/mtime/ctime 在网络文件系统、时间戳粒度和 inode 复用下都可能碰撞，收益不足以抵消错误去重/错误 sign 风险。

#### TDD

- hash 后、首片前 replace 为同长度文件；hash 途中和上传中原位改写；truncate 后扩回；rename 旧文件并在 path 创建新文件。
- 修改发生在最后一片完成与 provider complete 之间时必须 abort，不能完成远端对象。
- source 是 symlink/hardlink、稀疏文件、只读文件、非 UTF-8 path、网络/FUSE 文件系统可模拟的不稳定 metadata。
- 检测失败、abort 失败、cancel 与 source-change 同时发生时，只产生一个确定终态，原始错误优先级固定。
- 稳定普通文件的 MD5、上传内容、重试内容与优化前完全一致。

### U1（P1）：并发上传使用定位读取，去掉共享游标锁

#### 证据

`src/dflt/default_http_transfer_chunks.rs:114-172` 中，文件上传每个 part 都获取 `task.upload_file_slot().lock().await`，在锁内 open/seek/read_exact。锁在网络发送前释放是正确的，但所有并行 part 的本地读取仍排队执行。

#### 不改变 API 的方案

- 在 U0 的稳定 handle 上，Unix 使用内部 `FileExt::read_at`/`pread`，Windows 使用对应的 `seek_read`/overlapped 定位读取；优先避免每 part 新开 handle，防止高并发触发 FD/handle 上限。阻塞式平台 API 放到有界 `spawn_blocking`。
- positioned read 必须循环处理 short read/EINTR，并对所有 u64→usize、offset+len 做 checked conversion；32-bit target 不得截断。
- 读取直接进入每 part `BytesMut`，保留 `freeze()` 和 retry 时 `Bytes::clone()` 的零额外复制优势。
- 并发内存目标仍为 `max_parts_in_flight × chunk_size`，但当前公开配置没有乘积上限；执行前必须用 checked multiplication/可分配性验证并受控失败，不得让极端参数 OOM/abort，也不得创建无界读取预取或无界 blocking job。
- task 的公开 builder、`UploadSource`、协议 trait 和错误码均不变。

#### TDD

- 乱序读取 offsets `[3,0,2,1]`，服务端最终内容逐字节等于源文件。
- 使用 barrier 型 fake reader 证明两个 part 能同时进入 read，而不是只证明网络并行。
- 读取中途 truncate/delete/replace，错误分类和“不发送短/错 part”语义与现状一致。
- retry 多次发送同一 `Bytes` 内容，不重新读出不同数据。
- 非 chunk 对齐尾片、空文件、超大 offset、short read/EINTR、Windows/Unix 和 32-bit 特定测试。
- FD/handle 压力、blocking pool 饥饿、取消已经排队但未开始的 read；峰值内存不超过经过 checked 计算的预算。

性能验收必须把纯文件读取、协议 prepare、网络 PUT 分开计时；至少测 tmpfs/高速 SSD/限速网络三类场景。不能再用合并上传+下载 demo 推算上传速度。

### S1（P1）：跳过无效重调度、维护 O(1) 计数和保序 ready index

#### 证据

- `src/inner/exec_impl/exec.rs:19-86`：一次 `try_start_next` 最多遍历当前完整队列，槽位满/paused/active 时反复 pop 和 requeue。
- `src/inner/exec_impl/exec.rs:174-184`：每个候选任务再扫描 active map 统计同方向数量。
- `src/inner/executor.rs:431-444`：每个 worker event 处理后调用 `try_start_next`；进度事件多时会把队列扫描成本放大。

#### 不改变 API 的方案

- 第零阶段先让 `Progress` event 不再调用 `try_start_next`：它不会释放 file-level active slot。只有 Completed/Failed/Canceled 或 enqueue/resume 等可能改变 capacity/readiness 的事件才需要重调度。这可直接消除每个 part progress 引起的整队列扫描，同时保留现有队列结构。必须用调度不变量测试证明不存在“只能靠 Progress 偶然补启动”的旧状态。
- 第一阶段增加 `active_uploads`、`active_downloads` O(1) 计数，并保留单一 FIFO；这是收益明确、改变行为最小的改动。每次候选判断不必再扫描 active map。
- 是否仍需消除队列扫描必须由 1/100/10,000 queue benchmark 和 production profile 决定。不能直接拆成两个互不关联的 FIFO：那会改变 upload/download 的全局入队顺序和 callback 可观察顺序。
- 若第二阶段确需 ready index，为每个 group 保存单调 enqueue sequence，从有槽位的候选中选择“全局最老的可运行任务”；paused/full-direction 项用 index/tombstone 跳过，仍保持现有 oldest-eligible 语义。
- pause/resume/cancel/dedupe 更新必须原子维护 queue + set + count 不变量。
- debug/test 构建定期从 active map 重算并断言计数一致，避免 release 中静默漂移。
- 明确定义并测试跨方向 FIFO、同方向 FIFO 与方向公平性，不借优化偷偷改变可观察调度顺序。

#### TDD

- 10,000 queued、两个方向槽位均满时，单个 progress event 不应遍历 10,000 项（使用内部计数器断言，不使用脆弱墙钟断言）。
- 在任意合法状态中，处理 Progress 前后 ready/capacity 不变；跳过 `try_start_next` 后可启动集合与旧实现一致。
- enqueue/resume/Completed/Failed/Canceled 仍各自触发必要调度，不能因事件分类遗漏而让队列闲置。
- upload 满但 download 有槽位时，download 能启动；反向同理。
- paused 队首不会饿死后续 runnable；resume 后顺序符合定义。
- 混合 enqueue sequence（U1,D2,U3,D4）在不同槽位组合下与旧调度器产生相同启动序列。
- group 内重复任务/dedupe 不能被误计为多个 active slot。
- 完成、失败、part panic、外层 panic、pause-cancel、用户 cancel、close、重复/迟到 worker event 后 active count 恰好减一次且不下溢。
- 现有 Loom 模型扩展 queue/set/count/index/tombstone 一致性与 callback ack 交错。

### U2（P2）：降低上传前 MD5 的 time-to-first-byte

#### 证据

- `src/inner/inner_task.rs:97-165`：文件上传任务构造期间必须等待 `calculate_sign` 完成，之后才进入正常调度/prepare。
- `src/inner/sign.rs:6-34`：用 64 KiB buffer 顺序读取整个文件。
- 微基准纯 MD5 约 589.55 MiB/s；1 GiB 文件仅 hash CPU 理论上约 1.74 s，实际还叠加文件 I/O。随后上传会再次读取文件。

#### 约束与方案

`file_sign` 用于公开记录和重复任务判定，不能删除、延迟到已经发出上传后或改变算法，否则会改变行为。可先做：

- Criterion 同时测 64/256/1024 KiB buffer，选择跨平台稳健值。
- 先 profile Tokio file I/O 与 MD5 CPU 占比；只有证据表明 async worker 被 CPU hash 占用时，才把完整 read+hash 放进有界 blocking 工作。`spawn_blocking` 不能强制取消，必须增加协作取消点并验证 close 延迟。
- 本轮明确不做基于 `(device,inode,size,mtime,ctime)` 的 hash cache；它与 U0 的 generation 风险冲突。
- provider 若协议在 prepare 前就必须使用 MD5，不做“边上传边算”的错误优化。

#### TDD

已存在 known MD5/empty 测试，需补：hash 期间 truncate/replace、读取错误、取消/close、超大文件、同一文件并发入队、blocking queue 饱和。性能测试报告“调用 try_enqueue 到首个 prepare/首个 PUT”的延迟，并确认缺失文件/读取失败仍在与当前兼容的阶段返回。

### PR1（P3，需 profile）：预签名下载使用不可变 Arc 快照

#### 证据

`src/presigned/range_download.rs:35-177` 的热路径会：

- 锁 `std::sync::Mutex<PresignedRangeDownloadPlan>`；
- clone 整个 plan（URL、两个 HeaderMap 等）；
- `range_url()` 又锁并 clone URL；
- 刷新时再次 clone plan 并写回。

#### 不改变 API 的方案

先用含长 URL/HeaderMap 的真实基准证明 clone/锁确实进入 top profile；否则保留 P3。需要优化时，内部可改为 `Arc<PlanSnapshot>` 的 copy-on-write/RwLock 快照：普通 part 只 clone Arc；只有刷新成功时构造新 snapshot 并原子替换。公开 `plan() -> Result<PresignedRangeDownloadPlan, _>` 仍按原签名返回 owned clone，避免 API 破坏。不得未检查 MSRV/依赖体积就引入 `ArcSwap` 等新依赖。

当前同步 refresher 还有 refresh stampede 风险：多个 part 可同时拿到旧 snapshot 并重复刷新。内部需要按 generation single-flight；同步 trait 不能改成 async（否则破坏 API），因此必须验证慢 refresher 不会永久占住 async worker或造成锁死。TDD：高并发只触发一次刷新、旧请求与新 generation 并存、刷新时 cancel/close、过期无 refresher、刷新失败后可重试、刷新前后 total/validator 不一致、并发读者只看到完整 old/new snapshot、公开 `plan()` 内容不变。

### B1（P2）：binary body 的有界容量增长

#### 证据

`src/binary/download.rs:143-180` 即使收到可信且不超过 `max_body_bytes` 的 Content-Length，初始 `BytesMut` 容量也最多 64 KiB。默认上限 16 MiB 时会发生多次增长/复制。

#### 方案与 TDD

不要盲目按远端声明一次预留最大值，避免高并发 binary task 的内存尖峰。建议只做受当前 `max_body_bytes` 和并发模型约束的渐进 reserve；若引入全局内存 semaphore，会改变任务启动/完成顺序，必须先冻结行为，不能作为“小优化”顺手加入。测试：0/63/64/65 KiB、16 MiB 边界、虚假大/小 Content-Length、chunked、短 body、超过上限一字节、retry、cancel、很多并发 binary task、32-bit capacity 转换、分配失败；基准必须记录 allocation 次数和峰值 RSS，而不仅是墙钟。

### E1（P3）：进度 listener 快照与 DTO fan-out

`src/inner/exec_impl/emit.rs:47-71` 每条进度构造 `Vec<ProgressCb>`，逐个 clone callback，并为每个 listener clone `FileTransferRecord`。现有 32 callback 微基准仅 118.50 ns，暂不应抢占 D1/U1 优先级。

若真实 profiling 证明占比高，可内部维护 `Arc<[ProgressCb]>` 的 copy-on-write snapshot，注册/注销时重建，发送时只 clone 一个 Arc。由于公开 callback 接收 owned `FileTransferRecord`，每个 listener 的 DTO clone 不能在不改 API 的前提下消失；最多把 clone 延迟到 dispatcher 调用前，而当前字符串本来就是 `Arc<str>`。因此不能把“共享 DTO”写成预期主要收益。必须保留：listener ID/注册顺序、监听器可在自身回调中注册/注销、panic 隔离、队列满只丢 `Transmission`、terminal callback 不丢、close 等待 callback ack。现有 `global_listener_rwlock` 与 callback queue Loom/压力测试需扩展。

### D2（P3）：串行下载 flush/stat 频率

`src/dflt/default_http_transfer_chunks.rs:599-772` 持有 file slot mutex 覆盖整个 HTTP body 读取与写入，并在每个 SDK chunk 后 flush，再 metadata 检测文件被删/替换。串行任务没有并行 file cursor 冲突，但这些检查承载真实正确性语义，不能直接删除。

只有在 syscall profiling 证明占比后，才可考虑：把文件所有权移入单 worker、按字节/时间节流 presence check、pause/terminal 强制 flush。当前检查只以 path 存在和最小长度判断，无法识别同长度 replace；优化前应与 D0 共用 file generation 校验。TDD 必须覆盖写入中删除、同长度/更长文件 replace、原位修改、磁盘满、短 body rollback、pause 后恢复、进程终止；任何静默写入已 unlink inode 的情况都不可接受。

### A1（P3）：演示程序 DB 写入合并

根 `src/main.rs:61-88` 及 `tests/common/engine.rs:35-61` 在每个进度 callback 中直接 `tokio::spawn`，再执行 SQLite upsert。除高频 task/DB 压力外，callback 实际运行在线程 `rusty-cat-cb`，没有 Tokio runtime，因此 demo/live 测试反复 panic。

该问题不在库热路径，但会掩盖库吞吐并破坏 live 门禁。callback 中可使用预先捕获的 runtime `Handle`，或向 runtime 内已有 receiver 发送消息；不能在 `rusty-cat-cb` 上调用 `Handle::current()`。若用有界 channel/coalescing map，`Transmission` 可合并，但 Pending/Paused/Complete/Failed/Canceled 必须保证送达且顺序可证明，不能因为 DB 队列满而阻塞库 callback dispatcher 形成新的全局反压。单 writer 批量事务后，terminal 状态和 shutdown 必须强制 flush。不要在库中为 demo 特例改变 callback API。

## 5. 实施前必须补齐的测试与基准基础设施

### 5.1 冻结公开 API

1. 保留并扩展 `rusty-cat-contract-tests`，对 default features 和 all features 都运行。
2. CI 增加 `cargo public-api` 或 `cargo-semver-checks`，以发布的 0.2.4 为基线；任何 removed/changed public item 直接失败。
3. 保存 rustdoc JSON/public item 快照，覆盖 feature-gated provider API。
4. 本计划中的 coordinator、reader、queue、persistence injection 全部 `pub(crate)` 或 test-only，不能进入 `api` re-export。
5. 签名不变不等于行为兼容。建立 golden behavior tests，冻结 task ID/去重、错误 code、callback 线程模型与顺序、哪些 `Transmission` 可丢、progress 单调性、pause/resume/cancel/close 时机、完成回调 payload、默认值与 feature 行为。
6. 编译断言公开类型的 `Send/Sync/Clone` 等 auto-trait 不退化；所有 feature 组合（不只是 default/all 两端）至少 `cargo check`。
7. 在 `Cargo.toml` 明确并 CI 验证 MSRV；新依赖或新标准库 API不能无意提高第三方用户的 Rust 最低版本。
8. `.rcdl` 虽不是 Rust API，却是跨版本持久数据。v1/v2 升级、降级、旧文件安全失效属于兼容性门禁。

### 5.2 正确性矩阵

- debug + release；default features + all features；单线程 + 默认并行测试。
- Linux、macOS、Windows，加 32-bit compile/test；文件 durability、atomic replace 和 positioned I/O 不能只在 macOS 验证。
- 现有单元/集成/合同/Loom/doctest/所有示例。
- pause/resume/cancel/close 与 part 完成、retry、callback unregister 的交错。
- 200 忽略 Range、206 short/long/mismatched Content-Range、validator/Content-Encoding 不一致、断连、慢流、重定向、401/403、408/429/5xx、`Retry-After`、URL 刷新。
- HTTP/1.1 keep-alive/close、HTTP/2 多路复用、代理、TLS/DNS 失败、IPv4/IPv6；这些属于 integration/system，不应伪装成纯 unit test。
- 文件删除/同长度替换/原位修改/截断、权限变化、磁盘满/配额、只读目录、稀疏文件、非 UTF-8 path、FD 耗尽、sidecar 损坏、进程 kill/restart。
- total/chunk/offset/capacity 的 0、1、边界值和 checked conversion；恶意 Content-Length 不得 panic/OOM/无限分配。
- multipart retry 的幂等性、重复 complete、abort 失败和 orphan 清理；provider ETag/block-id/part-number 必须与 offset 稳定对应。
- callback 慢、panic、重入注册/注销、队列饱和、close 从 callback 附近发生；terminal 帧与 complete callback 均不丢。
- 每轮结果至少验证 size + MD5；崩溃测试再做逐字节比较。

### 5.3 性能基准设计

- 修复 `chunk_buffer` 基准：使用 `black_box` 消费结果并验证最终长度，防止编译器删除工作。
- 新增 download checkpoint benchmark：参数化 part size、part count、batch size、并发度，记录 fsync/rename 次数。
- 新增 upload positioned-read benchmark：共享游标旧路径对比定位读取，记录读取阶段与网络阶段。
- 新增 scheduler benchmark：1/100/10,000 queued，槽位空/满、paused 比例、每 10,000 progress events 成本。
- listener benchmark 使用真实 `FileTransferRecord`、dispatcher、多线程注册/注销。
- 公网只作补充；主基准使用可控 Range/PUT server，支持带宽、RTT、断连和错误注入。
- 区分 page-cache warm/cold、读盘与写盘是否同一设备、APFS/COW、server CPU/磁盘是否成为瓶颈；记录机器、电源/温控和后台负载。
- 性能 CI 比较中位数和置信区间，不把单次墙钟写成容易 flaky 的普通 unit test；功能不变量仍用确定性单测断言。

### 5.4 合入门槛

每个优化独立 PR，严格按 Red → Green → Refactor：

1. 先提交能在旧实现上稳定暴露问题/缺失不变量的测试或基准。
2. 只改一个热点，保持公开 API diff 为空。
3. 全量测试、Loom、故障注入和示例通过。
4. benchmark 至少 5 次，中位数达到该项阈值；无关基准回退不超过 5%，峰值内存不突破预算。
5. D1/U1 还需 30–60 分钟 soak、随机 pause/resume/cancel 和 kill/restart。
6. live 测试只有在 callback runtime、状态等待和云端 delete/abort 权限修好后才允许进入自动门禁。
7. sanitizer/Miri 能覆盖的纯 Rust unsafe/别名问题应运行；真实 fsync、进程崩溃、网络和云 provider 必须由 integration/system 测试补齐，不能要求 unit test 承担它做不到的证明。

### 5.5 “完备单元测试”现状与可追踪性

结论：**现在没有完备到足以安全实施全部优化的新增单元测试。** 原因不是现有测试少，而是生产代码尚未改动，本计划要求的红灯测试也尚未创建；此外 crash durability、跨平台 rename、真实网络和云清理本来就不能仅靠 unit test 完整覆盖。

| 工作项 | 当前已有覆盖 | 二次复核后仍缺 | 未补齐时能否改生产代码 |
|---|---|---|---|
| D0 | sidecar 编解码、长度/identity mismatch、基本恢复 | 远端同 URL 同长度换版、本地同长度替换、metadata 伪装、completed-part 摘要验证、validator、v1/v2、溢出/OOM、32-bit | **不能** |
| D1 | 每 part durable 后置 bit、基本 crash resume | epoch fence、窗口/阈值死锁、final batch、cancel 各阶段、Windows replace、父目录 durability | **不能** |
| U0/U1 | truncate/delete、基本并行上传、retry Bytes | 同一 file generation、同长度修改、same-handle positioned read、FD/memory/blocking pool 边界 | **不能** |
| S1 | PartWindow、部分 scheduler/Loom | Progress 事件分类、跨方向 golden 顺序、group/dedupe 计数、迟到事件、index/tombstone 模型 | 只能先做有测试的“跳过无效重调度”与 O(1) 计数 |
| U2 | known MD5、empty file、纯 MD5 benchmark | source generation、取消、端到端 TTFB、blocking 饱和 | **不能** |
| PR1 | refresh/plan 的现有功能测试 | single-flight、慢同步 refresher、old/new generation 并存、cancel/close | **不能** |
| B1 | body 上限与基本下载测试 | allocation/峰值、虚假长度、很多并发、32-bit/分配失败 | **不能** |
| E1/A1 | callback panic 隔离、部分 Loom | DB coalescing 顺序、terminal flush、队列饱和；live harness 当前实际失败 | **不能** |

TDD 执行规则：每个工作项先把该行缺口转成会在旧实现上失败或暴露缺失不变量的测试，确认 Red；再做最小生产改动到 Green；最后才重构和跑性能验收。不能先实现后补一组只验证 happy path 的测试。

### 5.6 本计划中“测试完备”的可执行定义

“完备”不等于覆盖率达到某个百分比，也不等于一次 `cargo test` 退出码为 0。某个工作项只有同时满足以下条件，文档才允许把它从“计划”改写为“已验证”：

1. 该工作项列出的正常、错误、取消、恢复和边界不变量均有自动化测试；旧实现上应失败的 Red 已被实际观察并记录，不能提交一个从未证明有效的回归测试。
2. 单元测试、property test、Loom、故障注入、跨进程 kill/restart、跨平台 integration/live 测试按各自能力分工；不得用 mock 单测替代真实 fsync/rename、网络或 provider 语义。
3. default/all features、支持的 feature 组合、MSRV、32-bit compile/test、Linux/macOS/Windows 门禁通过；平台限定测试只能有写明原因和替代证据的豁免。
4. 公开签名、auto-trait、错误分类、callback 顺序/线程、默认值、去重和 pause/resume/cancel/close 等 golden behavior 无回归。
5. 性能基准采用相同 durability/正确性语义，至少 5 次并报告中位数、p95、吞吐、syscall、峰值内存；不能用降低数据安全等级换取通过。
6. 新增测试必须至少连续稳定运行 20 次，soak 与随机交错无失败；live 测试还必须证明远端对象和 multipart cleanup 成功，不能以“任务主体成功但清理失败”算通过。
7. 所有已知失败、ignored 测试、平台缺口和未决产品语义均显式列出。任何一项仍影响该优化的正确性时，该工作项状态只能是“阻塞”或“部分验证”，不能写“测试完备”。

## 6. 二次静态复核发现的逻辑漏洞与遗漏场景

| 风险 | 原文漏洞/容易误做的方案 | 可能产生的新 bug | 现在的处置 |
|---|---|---|---|
| checkpoint 与窗口 | 只写“按 N/M/T 批量”，未定义 epoch 和唤醒条件 | threshold > in-flight window 时所有 part 等 checkpoint、checkpoint 又等更多 part，永久死锁 | D1 增加 freeze/fence/single-flusher/final batch 状态机与死锁测试 |
| barrier 竞态 | sync 期间仍把新完成 part 加入同一 bitmap | sidecar bit 可能包含 barrier 之后才写完、尚未 durable 的数据 | freeze epoch；新完成 part 只能进下一代 |
| 远端换版 | 只以 URL+total 识别 sidecar | 同 URL 同长度对象换版后拼接旧/新 part，size 正确但内容损坏 | 新增 D0，强 validator；无证明时安全重下 |
| 本地换版 | 只比较目标长度，或误把 file ID/size/mtime 当成内容证明 | 同长度 replace、原位修改或恢复 metadata 后旧 bit 被复用 | metadata 只作快速拒绝；恢复前逐个验证 completed-part 强摘要，无法证明时 bitmap 失效 |
| 多 client/进程 | 只依赖单 task mutex，temp 名固定 | 同一 target/sidecar 被交叉写、rename 覆盖或静默损坏 | target lease；双 client/双进程与 stale-owner 测试 |
| 上传源变化 | hash 与 upload 使用不同时间/handle 的 path 内容 | `file_sign` 与已上传内容不一致，或 multipart 混合两个版本 | 新增 U0；same generation、complete 前检查、检测后 abort |
| 调度顺序 | 直接拆 upload/download 两个 FIFO | callback/启动顺序、饥饿和公平性发生可观察变化 | S1 先跳过 Progress 无效重调度并做 O(1) 计数；后续以 enqueue sequence 保持 oldest-eligible |
| 跨平台 atomic replace | 假设 `rename(tmp, sidecar)` 各平台相同 | Windows 目标已存在/杀软占用时报错；崩溃留下 temp 或丢 generation | 平台矩阵、唯一 temp、replace/父目录 durability 测试 |
| 资源与算术 | `usize::MAX` 饱和、`as usize/u32`、offset 加法 | panic、wrap、OOM/abort、错误 Range、过量 FD/blocking jobs | checked arithmetic、`try_reserve`、32-bit/极值测试 |
| cancel/close | 背景 flusher 生命周期未定义 | terminal 已回调但 sidecar 仍写；use-after-drop 型逻辑、close 卡死或丢进度 | 所有后台工作 quiescent 后才能 terminal/close；冻结 cancel 延迟策略 |
| callback/DB | bounded channel 简单替换 `tokio::spawn` | terminal 被丢或 callback dispatcher 被 DB 反压 | Transmission 合并、terminal 保证送达、独立 flush/关闭协议 |
| listener DTO | 认为 `Arc<DTO>` 可消除所有 clone | callback 要 owned DTO；若改签名会破坏 API | 承认 per-listener clone 仍存在，只有 profile 命中才优化 snapshot |
| 性能门槛 | 把并发 c=4 与 durability 较弱的串行 c=1 硬比较 | 为过阈值而削弱 fsync 语义，制造崩溃恢复 bug | 以优化前后相同 durability 路径为主对照 |
| “API 未变” | 只跑签名 diff | 错误码、callback 顺序、MSRV、feature/auto-trait 仍可能回归 | 加 golden behavior、MSRV、feature matrix 和 auto-trait 门禁 |

仍需产品层明确的场景：没有强 ETag/version-id 时是否宁可放弃跨进程并发续传；cancel 是否承诺保留已写 part；上传源文件被外部修改时的正式错误契约。这些选择会改变可观察行为，不能由实现者自行猜测。未明确前只能采用“检测不到同一 generation 就安全重下/失败、不完成可能损坏对象”的保守策略。

## 7. 不应采用的“优化”

- 不删除上传 MD5、不改变 `file_sign` 或重复任务判定。
- 不先置 sidecar bit 再 flush 数据，不无条件去掉 fsync。
- 不在没有强 validator 时仅凭 URL、Content-Length 或抽样 part 猜测远端对象没变。
- 不仅凭 file ID、size、mtime/ctime 判断本地 completed part 未被修改；跨进程复用必须验证内容摘要或安全重下。
- 不让 checkpoint batch threshold 脱离实际 in-flight window，也不让 background flusher 越过 terminal/close 生命周期。
- 不通过增大公开默认并发掩盖每片固定成本。
- 不直接拆成两个方向 FIFO 而改变全局 oldest-eligible 调度顺序。
- 不改变 callback 线程、顺序、panic 隔离或 close ack 的公开语义。
- 不把短小本地 demo 的合并墙钟换算成某一个方向的吞吐。
- 不仅以文件大小一致作为成功；并发定位写必须校验 MD5/逐字节。
- 不为优化方便修改公开 trait、builder 方法、返回类型或 error code。

## 8. 扫描中发现的非性能发布阻塞项

这些问题不属于吞吐优化，但作为第三方库/公开仓库不能忽略：

1. 根测试 crate 和 `test-app` 源码中存在硬编码云访问凭证、账号密码和带签名 URL。**应立即轮换/吊销，改为环境变量或 secret manager，并清理 Git 历史。** 本文不记录任何具体值。
2. provider 的远端错误 body 可能包含 AccessKeyId、canonical request、signature 等内容；现有日志脱敏没有覆盖所有 XML/云厂商字段。需要增加脱敏单测，禁止 CI artifact 泄露。
3. live 测试账号没有成功清理对象的权限，且本轮中止可能留下对象/未完成 multipart。需用管理员权限检查 `rusty-cat-live/aliyun/` 前缀并清理，再设置生命周期规则兜底。
4. callback panic 被 dispatcher 隔离后，根 demo 最终仍退出 0，容易让脚本误报成功。demo/live harness 应把 callback side-effect 失败汇总为测试失败。

## 9. 已有的良好实现，优化时必须保留

- `reqwest::Client` 在默认传输 backend 中复用，连接池不会为每个 part 重建。
- 并发下载 part 使用独立文件 handle 和不重叠 offset，避免共享 cursor 数据竞争。
- 文件上传使用 `BytesMut::freeze()`，内存上传使用 `Bytes::slice()`；retry clone `Bytes` 是 O(1)。
- 下载严格校验 HTTP status、Content-Range、body 长度和 total，异常时不会把短 body 当成功。
- sidecar 已遵循“数据 durable 后才置 bit”的正确基本方向；D1 必须优化频率而不是破坏顺序。
- callback dispatcher 有 panic 隔离和 close ack，Loom 已覆盖若干关键交错。

## 10. 建议的第一批 TDD 工作包

第一批只做测试与内部基准，不改公开 API：

1. 修复 live harness 的 runtime Handle/channel、pause 状态等待和 cleanup 权限；让 7 个 target 可重复通过。
2. 修复偶发 `azure_direct_parallel_test` 的 mock HTTP framing，并要求连续 20 次通过。
3. 冻结 0.2.4 public API、golden behavior、feature matrix、auto-trait 和 MSRV。
4. 建立 D0 的远端/local generation、completed-part 强摘要、metadata 伪装、sidecar v1/v2、checked arithmetic 红灯测试；先决定“无 validator 时安全重下”的正式行为。
5. 建立 U0 的 hash→upload 源变化、complete 前变化和 abort 红灯测试。
6. 建立 D1 可注入 persistence backend、epoch/window 状态模型和 crash-boundary property tests。
7. 修复无效 buffer benchmark，新增 D1 syscall 计数与 U1 并行 read barrier 基准。

完成以上门禁后，先实现 D0/U0，再进入 D1/U1；不要把多个热点塞入同一个修改，以便任何回归都能快速定位和回退。

## 11. 实施结果（2026-08-22，待人工审核、未提交）

本节记录基于 `75b9b03e1dcf7ed5ebad7a780287e46be9fb0b5a`、`009b5453cfb6419d6cfb01616db92fcae4237669` 的复核修正。第 1–10 节仍是实施前审计原文，不应倒推为当时已经完成。当前工作区没有创建新 commit，公开 trait/builder 签名未改变。

### 11.1 工作项状态

| ID | 状态 | 实施结果 |
|---|---|---|
| D0 | 核心逻辑完成；跨平台门禁待 CI | sidecar v2 绑定强 ETag 和语义 URL、保存并验证每 part SHA-256；无 strong validator 时不信任旧 sidecar。HEAD 已提供 strong ETag 时每个 206 必须一致；无 HEAD validator 的多 range 运行从首个 206 锁定并要求同一 strong ETag，单 range 可无 ETag。query 只剔除已识别的 AWS/Google/OSS/Azure 签名字段；目标 lease 覆盖进程内、跨进程、symlink/hardlink，并在 macOS 保守覆盖大小写别名；part/offset/sidecar 分配均受 checked arithmetic 和上限保护。 |
| D1 | 完成 | 8 part epoch checkpoint，最后不足一批强制收敛，并增加真实 250 ms one-shot 定时触发；数据 barrier 成功后才原子替换含 bitmap+digest 的 sidecar；checkpoint I/O 移到 blocking worker；失败保留 pending，pause/cancel/final 路径重试或传播错误。 |
| U0 | 完成 | 排队/暂停任务在初始 MD5 后关闭 FD；激活时重开并先强复验内容，再让所有 positioned read 共享同一 handle；完成前再次全内容校验，换版则 abort，不 complete。仅 metadata 一致不再被当成内容证明。 |
| U1 | 完成 | Unix `read_at` / Windows `seek_read` 循环读取，去掉共享 cursor 锁；blocking read、short read、offset/usize 转换受控；实际进入并行 driver 后按真实 part grid 检查，最多 256 个 part task；上传 body+verification scratch、下载 response frame+目标 Vec 双缓冲受每客户端共享预算约束（64 位 512 MiB、32 位 64 MiB），取消/panic 通过 RAII 释放 permit，串行降级和小文件不会被虚假的大配置拒绝。 |
| S1 | 计划内必做阶段完成 | Progress event 不再触发无意义重调度；上传/下载活跃计数改为 O(1)，有计数一致性测试。ready index 仍按计划保持条件化：当前基准已证明 Progress 扫描被直接消除，尚无证据需要承担改变调度结构的风险。 |
| U2 | 安全子项完成 | MD5 buffer 从 64 KiB 增至 1 MiB，初始 hash blocking jobs 由全局 semaphore 限制为 1–4；MD5/去重语义不变。为满足 U0，激活重开和完成前仍需强内容复验，因此不宣称减少整文件安全校验次数。 |
| PR1 | 完成 | plan 使用不可变 `Arc` 快照和 single-flight refresh；URL 与 headers 来自同一快照；同步 refresher 在 blocking worker 中执行；递归刷新和 panic 转为受控错误。没有为此增加公开 trait 方法。 |
| B1 | 完成 | 已知 Content-Length 的初始 reserve 上限由 64 KiB 提高到 1 MiB，仍受全局/任务 body 上限和 `try_reserve` 保护；极端 u64 输入先 clamp 再做架构安全转换。 |
| E1 | 完成 | 全局 listener 列表以不可变 `Arc<[...]>` 发布；事件热路径只 clone 一个 Arc 快照，回调顺序、重入和 panic 隔离语义不变。 |
| D2 | 按计划不启用 | P3 条件项要求 syscall profile 命中后才改。现有串行发布基线已约 595 MiB/s，当前没有证据证明 flush/stat 是主要瓶颈；贸然降低检查频率会削弱删除检测与断点语义。 |
| A1 | 完成 | 根演示应用改为单 DB writer：Transmission 合并、生命周期/终态保序、批量事务；失败批次按 sequence 全量回队，`flush`/`shutdown` 返回错误并可显式重试；终态去重缓存有 4096 项上限。 |

### 11.2 自动化验证结果

| 命令/范围 | 实施后结果 |
|---|---|
| `cargo test --workspace --all-features --no-fail-fast -- --test-threads=1` | **通过**；库单测 175/175，workspace 单元、集成、合同、Loom 和 doctest 全部通过；doctest 116 通过、1 ignored。 |
| `cargo test --manifest-path test-app/Cargo.toml -- --test-threads=1` | **17/17 通过**。 |
| `intra_file_parallel_test` | **7/7 通过**，包含真实 executor 的 257-task 资源拒绝，以及不支持并行的协议在相同大配置下安全串行降级。 |
| `concurrent_download_test` | **16/16 通过**，包含无/弱 ETag、同长度换版、并发 generation 切换、预签名刷新、语义 query 变化和恢复完整性。 |
| upload source / checkpoint / presigned / DB writer 定向测试 | 全部通过；包括 metadata 碰撞模拟后的重开强复验、250 ms wall-clock checkpoint、刷新重入无死锁、DB 首次失败后原序重试。 |
| `cargo clippy -p rusty-cat --all-features --lib -- -D warnings` | 仍失败于 **13 个既有 lint**（未修改的 Aliyun/Azure/default HTTP/HTTP config/client/presigned multipart 行）；本轮新增代码未产生新的 strict Clippy 诊断。原审计的 all-targets 基线为 59 个诊断。 |

live 云测试仍 ignored，需要有效凭证、远端清理/abort 权限和 Linux/Windows CI；本机只实际执行了 macOS arm64，不能把 32-bit、Linux、Windows、真实云和 kill/restart soak 写成已验证。

### 11.3 同机 Criterion 对比

独立命令：`cargo bench -p rusty-cat --bench transfer_hotspots -- --noplot`。以下是热点简化模型，不含公网、provider、真实 fsync 延迟或端到端调度，不能替代发布模式矩阵。

| 工作项/模型 | 优化前 | 优化后 | 变化 |
|---|---:|---:|---:|
| D1：596 part checkpoint snapshot | 293.00 µs；596 次 snapshot/barrier 模型 | 37.371 µs；75 次 | **7.84x**；时间减少 **87.25%**；次数减少 **87.42%** |
| U1：4×1 MiB 共享 cursor 读取 | 217.17 µs | positioned read 93.946 µs | **2.31x**；时间减少 **56.74%** |
| S1：Progress 触发 queue=1 扫描 | 26.400 ns | 0.30213 ns | **87.4x**（被消除操作的微模型） |
| S1：Progress 触发 queue=100 扫描 | 189.63 ns | 0.29996 ns | **632x** |
| S1：Progress 触发 queue=10,000 扫描 | 17.584 µs | 0.29665 ns | **约 59,275x**；复杂度从 O(Q) 变为该事件不调度 |
| E1：32 listener snapshot+invoke | 118.05 ns | 31.812 ns | **3.71x**；时间减少 **73.05%** |
| U2：4 MiB 纯 MD5，64 KiB vs 1 MiB buffer | 6.9879 ms / 572.42 MiB/s | 7.0127 ms / 570.39 MiB/s | 约 **-0.35%**，属于噪声；没有 CPU 提升证据，收益只可能来自文件 read syscall 减少 |
| 修正后的 2 KiB chunk buffer 模型 | 原审计约 419 ps，已判定被优化器消除 | 20.928 ns / 91.137 GiB/s | 只说明基准已有效，不能当作产品吞吐提升 |

未给出伪精确数字的项目：D0/U0/PR1 主要是正确性或尾延迟保护；A1 需要真实 SQLite 压力与队列分布；B1 的确定变化是已知 body 初始 reserve 从最多 64 KiB 到最多 1 MiB（以 16 MiB 且按倍增估算，扩容阶梯约由 8 次降到 4 次），但尚未采集 allocator/峰值数据；D2 没有实施，提升为 0。

原 595.8 MiB 发布模式矩阵的 fixture/Range runner 不在当前工作树中，因此本轮没有制造一个不同条件的“优化后吞吐”与旧数据硬比较。最终性能验收仍应恢复同一 fixture、同一 durability 语义，warm-up 1 次、正式 5–10 次，报告中位数、p95、syscall 和峰值内存。
