use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Method;
use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::file_transfer_record::FileTransferRecord;
use rusty_cat::meow_config::MeowConfig;
use rusty_cat::transfer_status::TransferStatus;
use rusty_cat::MeowClient;

fn parse_range_header(req: &str) -> Option<(u64, u64)> {
    // 查找 Range 头所在行，格式例如 `Range: bytes=4-7`。
    let line = req
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("range:"))?;
    // 抽取 `bytes=...` 右侧部分并去除空白。
    let value = line.split_once(':')?.1.trim();
    // 仅支持 bytes range，和客户端实现一致。
    let range = value.strip_prefix("bytes=")?;
    // 拆分 start-end 并解析为整数。
    let (start, end) = range.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

fn spawn_range_server(
    payload: Vec<u8>,
    get_delay_ms: u64,
) -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    // 绑定本地临时端口，供测试客户端访问。
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
    // 记录监听地址用于构造 URL。
    let addr = listener.local_addr().expect("read local addr");
    // 采用非阻塞 accept，方便通过 stop 标记优雅退出线程。
    listener
        .set_nonblocking(true)
        .expect("set listener nonblocking");
    // 关闭标记：测试结束时置 true，server 线程会退出。
    let stop = Arc::new(AtomicBool::new(false));
    // 在线程中共享 stop 标记。
    let stop_for_thread = stop.clone();
    // 在线程中共享静态 payload 内容。
    let payload_for_thread = payload.clone();
    // 拉起 server 线程，按请求动态返回 HEAD/Range GET。
    let handle = thread::spawn(move || {
        loop {
            // 若收到停止信号，结束线程。
            if stop_for_thread.load(Ordering::Acquire) {
                break;
            }
            // 尝试 accept 连接；非阻塞模式下可能返回 WouldBlock。
            let accepted = listener.accept();
            let (mut stream, _) = match accepted {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            };

            // 限制读取时间，避免异常请求导致线程卡住。
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            // 读取请求原文用于判断方法与 Range。
            let mut buf = [0u8; 4096];
            let n = match stream.read(&mut buf) {
                Ok(n) => n,
                Err(_) => continue,
            };
            // 将请求字节转成文本，非法 UTF-8 字节用占位字符替换。
            let req = String::from_utf8_lossy(&buf[..n]).to_string();

            // HEAD 请求返回总长度，供 prepare 阶段探测远端大小。
            if req.starts_with("HEAD ") {
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload_for_thread.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
                continue;
            }

            // 非 HEAD 路径按 Range GET 处理；若缺 Range 则返回 400。
            let Some((start, end)) = parse_range_header(&req) else {
                let _ = stream.write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                let _ = stream.flush();
                continue;
            };

            // 按测试参数模拟服务端处理延迟，便于稳定触发 pause。
            thread::sleep(Duration::from_millis(get_delay_ms));
            // 保护性裁剪，避免请求区间越界导致 panic。
            let start_idx = start as usize;
            let end_idx = end as usize;
            if start_idx >= payload_for_thread.len() || end_idx < start_idx {
                let _ = stream.write_all(
                    b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                let _ = stream.flush();
                continue;
            }
            // 将结束位置限制在 payload 最后一个字节。
            let bounded_end = end_idx.min(payload_for_thread.len() - 1);
            // 切片出本次分片响应体。
            let body = &payload_for_thread[start_idx..=bounded_end];
            // 生成标准 206 + Content-Range 响应。
            let resp = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                start_idx,
                bounded_end,
                payload_for_thread.len(),
                body.len()
            );
            // 回写响应头和响应体。
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
        }
    });
    // 返回可访问 URL、停止标记和线程句柄。
    (format!("http://{addr}/download"), stop, handle)
}

fn temp_download_path(case: &str) -> PathBuf {
    // 在系统临时目录下生成唯一文件路径，避免并发测试互相干扰。
    let mut p = std::env::temp_dir();
    // 使用纳秒时间戳保证路径唯一性。
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    // 组装最终测试文件名。
    p.push(format!("rusty_cat_pause_resume_{case}_{ts}.bin"));
    p
}

async fn wait_until<F>(timeout_ms: u64, mut pred: F)
where
    F: FnMut() -> bool,
{
    // 使用轮询等待条件成立，最多等待 timeout_ms。
    let mut waited = 0u64;
    while waited < timeout_ms {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += 20;
    }
    // 超时直接失败，避免测试静默挂起。
    panic!("condition wait timed out after {timeout_ms}ms");
}

#[tokio::test]
async fn pause_then_resume_keeps_same_task_id_and_finishes() {
    // 构造 12 字节数据，配合 chunk=4 可形成 3 个分片便于观察 pause/resume。
    let payload = b"abcdefghijkl".to_vec();
    // 启动动态 Range server，并对 GET 增加延迟提升测试稳定性。
    let (url, stop, handle) = spawn_range_server(payload.clone(), 250);
    // 创建本地下载目标路径。
    let path = temp_download_path("happy_path");
    // 初始化客户端，单并发便于简化时序判断。
    let client = MeowClient::new(MeowConfig::new(1, 1));
    // 存储回调里观察到的状态轨迹。
    let statuses: Arc<Mutex<Vec<TransferStatus>>> = Arc::new(Mutex::new(Vec::new()));
    // 在回调里共享状态轨迹容器。
    let statuses_for_cb = statuses.clone();
    // 构建下载任务，chunk 固定为 4。
    let task = DownloadPounceBuilder::new("payload.bin", &path, 4, url, Method::GET).build();
    // 入队并持有 task_id，后续将使用同一个 id 做 pause/resume。
    let task_id = client
        .enqueue(
            task,
            move |record: FileTransferRecord| {
                statuses_for_cb
                    .lock()
                    .expect("lock statuses")
                    .push(record.status().clone());
            },
            |_, _| {},
        )
        .await
        .expect("enqueue task");

    // 等待至少收到一次传输中状态，确保任务已经实际跑起来。
    wait_until(3000, || {
        statuses
            .lock()
            .expect("lock statuses")
            .iter()
            .any(|s| matches!(s, TransferStatus::Transmission))
    })
    .await;

    // 触发暂停：这里不应销毁任务实体，只应进入可恢复状态。
    client.pause(task_id).await.expect("pause task");

    // 等待回调观察到 Paused，确认暂停状态对外可见。
    wait_until(3000, || {
        statuses
            .lock()
            .expect("lock statuses")
            .iter()
            .any(|s| matches!(s, TransferStatus::Paused))
    })
    .await;

    // 轮询调用 resume，兼容“pause 正在收敛”的短暂竞态窗口。
    let mut resumed = false;
    // 最多尝试 150 次，每次间隔 20ms，总等待约 3s。
    for _ in 0..150 {
        if client.resume(task_id).await.is_ok() {
            resumed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 若超过重试窗口仍失败，说明 pause/resume 状态机存在问题。
    assert!(resumed, "resume did not succeed within retry window");

    // 等待终态（Complete/Failed/Canceled）出现，再据此断言最终结果。
    let mut terminal: Option<TransferStatus> = None;
    for _ in 0..250 {
        let snapshot = statuses.lock().expect("lock statuses").clone();
        terminal = snapshot
            .iter()
            .rev()
            .find(|s| {
                matches!(
                    s,
                    TransferStatus::Complete | TransferStatus::Failed(_) | TransferStatus::Canceled
                )
            })
            .cloned();
        if terminal.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 关闭客户端，释放内部调度线程。
    client.close().await.expect("close client");
    // 通知 server 线程退出。
    stop.store(true, Ordering::Release);
    // 等待 server 线程收尾。
    handle.join().expect("join server thread");
    // 读取最终文件，验证下载内容完整无损。
    let bytes = fs::read(&path).expect("read downloaded file");
    // 清理测试临时文件。
    fs::remove_file(&path).expect("remove temp file");

    // 断言终态必须为 Complete，否则打印状态轨迹方便定位问题。
    assert!(
        matches!(terminal, Some(TransferStatus::Complete)),
        "unexpected terminal status: {:?}, full statuses: {:?}",
        terminal,
        statuses.lock().expect("lock statuses")
    );
    // 断言最终内容与服务端 payload 完全一致。
    assert_eq!(bytes, payload);
}
