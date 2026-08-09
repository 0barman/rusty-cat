use std::future::Future;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::JoinHandle;
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue};
use rusty_cat::api::{
    BinaryDownloadConfig, BinaryDownloadOutput, BinaryTask, InnerErrorCode, MeowClient, MeowConfig,
};

struct BinaryThreadWake(std::thread::Thread);

impl Wake for BinaryThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on_without_tokio<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(BinaryThreadWake(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park_timeout(Duration::from_secs(2)),
        }
    }
}

struct BinaryTestServer {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    peak: Arc<AtomicUsize>,
    partial_hits: Arc<AtomicUsize>,
    redirect_target: Arc<Mutex<Option<String>>>,
}

impl BinaryTestServer {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let address = listener.local_addr().expect("local address");
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let partial_hits = Arc::new(AtomicUsize::new(0));
        let redirect_target = Arc::new(Mutex::new(None));
        let thread_stop = Arc::clone(&stop);
        let thread_peak = Arc::clone(&peak);
        let thread_active = Arc::clone(&active);
        let thread_partial_hits = Arc::clone(&partial_hits);
        let thread_redirect_target = Arc::clone(&redirect_target);
        let join = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let peak = Arc::clone(&thread_peak);
                        let active = Arc::clone(&thread_active);
                        let partial_hits = Arc::clone(&thread_partial_hits);
                        let redirect_target = Arc::clone(&thread_redirect_target);
                        std::thread::spawn(move || {
                            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(now, Ordering::SeqCst);
                            handle_connection(stream, &partial_hits, &redirect_target);
                            active.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) if thread_stop.load(Ordering::SeqCst) => break,
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Self {
            address,
            stop,
            join: Some(join),
            peak,
            partial_hits,
            redirect_target,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.address, path)
    }

    fn set_redirect_target(&self, target: String) {
        *self.redirect_target.lock().expect("redirect target lock") = Some(target);
    }
}

impl Drop for BinaryTestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn handle_connection(
    mut stream: TcpStream,
    partial_hits: &AtomicUsize,
    redirect_target: &Mutex<Option<String>>,
) {
    stream
        .set_nonblocking(false)
        .expect("set accepted stream blocking");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set accepted stream read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set accepted stream write timeout");
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    while request.len() < 16 * 1024 {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    let request_text = String::from_utf8_lossy(&request);
    let path = request_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    match path {
        "/image" => write_response(
            &mut stream,
            "200 OK",
            &[("Content-Type", "image/jpeg")],
            b"\xff\xd8\xff\xe0binary-jpeg",
        ),
        "/json" => write_response(
            &mut stream,
            "200 OK",
            &[("Content-Type", "application/json")],
            br#"{"kind":"json"}"#,
        ),
        "/pdf" => write_response(
            &mut stream,
            "200 OK",
            &[("Content-Type", "application/pdf")],
            b"%PDF-1.7\nbinary-pdf",
        ),
        "/octet" => write_response(
            &mut stream,
            "200 OK",
            &[("Content-Type", "application/octet-stream")],
            b"\0\x01\x02\xff",
        ),
        "/header" if has_header_value(&request_text, "x-test-token", "meow") => {
            write_response(&mut stream, "200 OK", &[], b"header-ok")
        }
        "/reject-pounce-client-header"
            if !request_text
                .to_ascii_lowercase()
                .contains("x-pounce-client-only:") =>
        {
            write_response(&mut stream, "200 OK", &[], b"isolated-client")
        }
        "/empty" => write_response(&mut stream, "204 No Content", &[], b""),
        "/non-utf8-content-type" => {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/\xffbinary\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx",
            );
        }
        "/exact-eight" => write_response(&mut stream, "200 OK", &[], b"12345678"),
        "/redirect-image" => {
            write_response(&mut stream, "302 Found", &[("Location", "/image")], b"")
        }
        "/redirect-twice" => write_response(
            &mut stream,
            "302 Found",
            &[("Location", "/redirect-image")],
            b"",
        ),
        "/redirect-cross" => {
            let target = redirect_target
                .lock()
                .expect("redirect target lock")
                .clone()
                .unwrap_or_else(|| "/status".to_owned());
            write_response(
                &mut stream,
                "302 Found",
                &[("Location", target.as_str())],
                b"",
            );
        }
        "/reject-sensitive-headers"
            if !request_text.to_ascii_lowercase().contains("authorization:")
                && !request_text.to_ascii_lowercase().contains("cookie:") =>
        {
            write_response(&mut stream, "200 OK", &[], b"stripped")
        }
        "/status" => write_response(&mut stream, "404 Not Found", &[], b"not-found"),
        "/large" => write_response(&mut stream, "200 OK", &[], &[0; 4096]),
        "/huge-length" => {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551615\r\nConnection: close\r\n\r\n",
            );
        }
        "/conflicting-length" => {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\nConnection: close\r\n\r\nx",
            );
        }
        "/chunked-large" => {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n8\r\n12345678\r\n8\r\nabcdefgh\r\n0\r\n\r\n",
            );
        }
        "/partial-retry" => {
            if partial_hits.fetch_add(1, Ordering::SeqCst) == 0 {
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc",
                );
            } else {
                write_response(&mut stream, "200 OK", &[], b"abcdef");
            }
        }
        "/always-partial" => {
            partial_hits.fetch_add(1, Ordering::SeqCst);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc");
        }
        "/slow" => {
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n");
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(700));
            let _ = stream.write_all(b"slow");
        }
        "/concurrency" => {
            std::thread::sleep(Duration::from_millis(100));
            write_response(&mut stream, "200 OK", &[], b"ok");
        }
        _ => write_response(&mut stream, "500 Internal Server Error", &[], b"unexpected"),
    }
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
}

fn has_header_value(request: &str, expected_name: &str, expected_value: &str) -> bool {
    request.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case(expected_name)
                && value.trim().eq_ignore_ascii_case(expected_value)
        })
    })
}

fn write_response(stream: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &[u8]) {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(body);
}

fn client_with(config: BinaryDownloadConfig) -> MeowClient {
    MeowClient::new(
        MeowConfig::builder()
            .binary_download_config(config)
            .build()
            .expect("valid meow config"),
    )
}

async fn enqueue_and_receive(
    client: &MeowClient,
    task: BinaryTask,
) -> Result<BinaryDownloadOutput, rusty_cat::api::MeowError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    client
        .try_enqueue_binary_task(task, move |_, result| {
            let _ = tx.send(result);
        })
        .expect("enqueue binary task");
    tokio::time::timeout(Duration::from_secs(4), rx)
        .await
        .expect("binary callback timeout")
        .expect("callback sender dropped")
}

#[tokio::test]
async fn downloads_image_bytes_content_type_and_custom_headers() {
    let server = BinaryTestServer::spawn();
    let client = client_with(BinaryDownloadConfig::default());
    let output = enqueue_and_receive(&client, BinaryTask::new(server.url("/image")))
        .await
        .expect("image download");
    assert_eq!(output.bytes(), &b"\xff\xd8\xff\xe0binary-jpeg"[..]);
    assert_eq!(
        output.content_type().and_then(|value| value.to_str().ok()),
        Some("image/jpeg")
    );

    let output = enqueue_and_receive(
        &client,
        BinaryTask::new(server.url("/header")).with_header(
            HeaderName::from_static("x-test-token"),
            HeaderValue::from_static("meow"),
        ),
    )
    .await
    .expect("header download");
    assert_eq!(output.bytes(), &b"header-ok"[..]);

    let redirected = enqueue_and_receive(&client, BinaryTask::new(server.url("/redirect-image")))
        .await
        .expect("redirected image download");
    assert_eq!(redirected.bytes(), &b"\xff\xd8\xff\xe0binary-jpeg"[..]);
    assert_eq!(
        redirected
            .content_type()
            .and_then(|value| value.to_str().ok()),
        Some("image/jpeg")
    );
    client.close().await.expect("close binary client");
}

#[tokio::test]
async fn downloads_multiple_binary_kinds_and_strips_cross_origin_credentials() {
    let source = BinaryTestServer::spawn();
    let target = BinaryTestServer::spawn();
    source.set_redirect_target(target.url("/reject-sensitive-headers"));
    let client = client_with(BinaryDownloadConfig::default());
    for (path, expected_type, prefix) in [
        ("/json", "application/json", b"{".as_slice()),
        ("/pdf", "application/pdf", b"%PDF".as_slice()),
        ("/octet", "application/octet-stream", b"\0\x01".as_slice()),
    ] {
        let output = enqueue_and_receive(&client, BinaryTask::new(source.url(path)))
            .await
            .expect("binary kind download");
        assert!(output.bytes().starts_with(prefix));
        assert_eq!(
            output.content_type().and_then(|value| value.to_str().ok()),
            Some(expected_type)
        );
    }

    let redirected = enqueue_and_receive(
        &client,
        BinaryTask::new(source.url("/redirect-cross"))
            .with_header(
                reqwest::header::AUTHORIZATION,
                HeaderValue::from_static("Bearer secret"),
            )
            .with_header(
                reqwest::header::COOKIE,
                HeaderValue::from_static("session=secret"),
            ),
    )
    .await
    .expect("cross-origin redirect must strip credentials");
    assert_eq!(redirected.bytes(), &b"stripped"[..]);
    client.close().await.expect("close client");
}

#[tokio::test]
async fn supports_empty_body_and_reports_http_and_size_errors() {
    let server = BinaryTestServer::spawn();
    let config = BinaryDownloadConfig::builder()
        .max_body_bytes(8)
        .retry_delays(Vec::new())
        .build()
        .expect("binary config");
    let client = client_with(config);

    let empty = enqueue_and_receive(&client, BinaryTask::new(server.url("/empty")))
        .await
        .expect("empty response is valid");
    assert!(empty.bytes().is_empty());
    assert!(empty.content_type().is_none());

    let non_utf8 = enqueue_and_receive(
        &client,
        BinaryTask::new(server.url("/non-utf8-content-type")),
    )
    .await
    .expect("non-UTF8 Content-Type remains transport metadata");
    assert!(non_utf8
        .content_type()
        .expect("content type")
        .to_str()
        .is_err());

    let exact = enqueue_and_receive(&client, BinaryTask::new(server.url("/exact-eight")))
        .await
        .expect("body exactly at limit succeeds");
    assert_eq!(exact.bytes(), &b"12345678"[..]);

    let status = enqueue_and_receive(&client, BinaryTask::new(server.url("/status")))
        .await
        .expect_err("404 must fail");
    assert_eq!(status.code(), InnerErrorCode::ResponseStatusError as i32);
    assert_eq!(status.http_status(), Some(404));

    for path in ["/large", "/chunked-large"] {
        let error = enqueue_and_receive(&client, BinaryTask::new(server.url(path)))
            .await
            .expect_err("oversized response must fail");
        assert_eq!(
            error.code(),
            InnerErrorCode::BinaryBodyTooLarge as i32,
            "unexpected error for {path}: {error:?}"
        );
    }
    let huge_length = enqueue_and_receive(&client, BinaryTask::new(server.url("/huge-length")))
        .await
        .expect_err("u64::MAX Content-Length must fail without allocation");
    assert!(matches!(
        huge_length.code(),
        code if code == InnerErrorCode::BinaryBodyTooLarge as i32
            || code == InnerErrorCode::HttpError as i32
    ));
    let conflicting =
        enqueue_and_receive(&client, BinaryTask::new(server.url("/conflicting-length")))
            .await
            .expect_err("conflicting Content-Length must fail");
    assert_eq!(conflicting.code(), InnerErrorCode::HttpError as i32);
    client.close().await.expect("close binary client");
}

#[tokio::test]
async fn retry_discards_partial_body_and_timeout_is_reported() {
    let server = BinaryTestServer::spawn();
    let retrying = client_with(
        BinaryDownloadConfig::builder()
            .request_timeout(Duration::from_secs(2))
            .retry_delays(vec![Duration::from_millis(10)])
            .build()
            .expect("retry config"),
    );
    let output = enqueue_and_receive(&retrying, BinaryTask::new(server.url("/partial-retry")))
        .await
        .expect("retry should recover");
    assert_eq!(output.bytes(), &b"abcdef"[..]);
    assert_eq!(server.partial_hits.load(Ordering::SeqCst), 2);
    retrying.close().await.expect("close retrying client");

    let timing_out = client_with(
        BinaryDownloadConfig::builder()
            .request_timeout(Duration::from_millis(80))
            .retry_delays(Vec::new())
            .build()
            .expect("timeout config"),
    );
    let error = enqueue_and_receive(&timing_out, BinaryTask::new(server.url("/slow")))
        .await
        .expect_err("slow response must time out");
    assert_eq!(error.code(), InnerErrorCode::HttpError as i32);
    timing_out.close().await.expect("close timeout client");
}

#[tokio::test]
async fn binary_defaults_inherit_meow_timeout_without_reusing_its_http_client() {
    let server = BinaryTestServer::spawn();
    let inherited = MeowClient::new(
        MeowConfig::builder()
            .http_timeout(Duration::from_millis(80))
            .build()
            .expect("meow config"),
    );
    let error = enqueue_and_receive(&inherited, BinaryTask::new(server.url("/slow")))
        .await
        .expect_err("binary default must inherit MeowConfig timeout");
    assert_eq!(error.code(), InnerErrorCode::HttpError as i32);
    inherited.close().await.expect("close inherited client");

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-pounce-client-only"),
        HeaderValue::from_static("present"),
    );
    let pounce_http_client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("custom pounce HTTP client");
    let isolated = MeowClient::new(
        MeowConfig::builder()
            .http_client(pounce_http_client)
            .binary_download_config(BinaryDownloadConfig::default())
            .build()
            .expect("isolated client config"),
    );
    let output = enqueue_and_receive(
        &isolated,
        BinaryTask::new(server.url("/reject-pounce-client-header")),
    )
    .await
    .expect("binary executor must use an isolated reqwest client");
    assert_eq!(output.bytes(), &b"isolated-client"[..]);
    isolated.close().await.expect("close isolated client");
}

#[tokio::test]
async fn cancel_interrupts_retry_backoff_without_waiting_for_the_delay() {
    let server = BinaryTestServer::spawn();
    let client = client_with(
        BinaryDownloadConfig::builder()
            .request_timeout(Duration::from_secs(2))
            .retry_delays(vec![Duration::from_secs(1)])
            .build()
            .expect("backoff config"),
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task_id = client
        .try_enqueue_binary_task(
            BinaryTask::new(server.url("/always-partial")),
            move |_, result| {
                let _ = tx.send(result);
            },
        )
        .expect("enqueue retrying task");
    tokio::time::timeout(Duration::from_secs(1), async {
        while server.partial_hits.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first partial attempt");
    tokio::time::sleep(Duration::from_millis(30)).await;

    let started = std::time::Instant::now();
    client.cancel(task_id).await.expect("cancel during backoff");
    let result = tokio::time::timeout(Duration::from_millis(300), rx)
        .await
        .expect("backoff cancellation callback must be prompt")
        .expect("callback sender");
    assert!(started.elapsed() < Duration::from_millis(300));
    assert_eq!(
        result.expect_err("canceled backoff task").code(),
        InnerErrorCode::TaskCanceled as i32
    );
    assert_eq!(server.partial_hits.load(Ordering::SeqCst), 1);
    client.close().await.expect("close backoff client");
}

#[tokio::test]
async fn redirect_limit_zero_rejects_redirect_without_retrying_status() {
    let server = BinaryTestServer::spawn();
    let client = client_with(
        BinaryDownloadConfig::builder()
            .redirect_limit(0)
            .retry_delays(Vec::new())
            .build()
            .expect("no redirect config"),
    );
    let error = enqueue_and_receive(&client, BinaryTask::new(server.url("/redirect-image")))
        .await
        .expect_err("redirect must be rejected");
    assert_eq!(error.code(), InnerErrorCode::HttpError as i32);
    client.close().await.expect("close client");
}

#[tokio::test]
async fn positive_redirect_limit_counts_followed_redirects_exactly() {
    let server = BinaryTestServer::spawn();
    let one_redirect = client_with(
        BinaryDownloadConfig::builder()
            .redirect_limit(1)
            .retry_delays(Vec::new())
            .build()
            .expect("one redirect config"),
    );
    let output = enqueue_and_receive(
        &one_redirect,
        BinaryTask::new(server.url("/redirect-image")),
    )
    .await
    .expect("one configured redirect must be followed");
    assert_eq!(output.bytes(), &b"\xff\xd8\xff\xe0binary-jpeg"[..]);
    let error = enqueue_and_receive(
        &one_redirect,
        BinaryTask::new(server.url("/redirect-twice")),
    )
    .await
    .expect_err("a second redirect must exceed limit one");
    assert_eq!(error.code(), InnerErrorCode::HttpError as i32);
    one_redirect.close().await.expect("close limit-one client");

    let two_redirects = client_with(
        BinaryDownloadConfig::builder()
            .redirect_limit(2)
            .retry_delays(Vec::new())
            .build()
            .expect("two redirect config"),
    );
    let output = enqueue_and_receive(
        &two_redirects,
        BinaryTask::new(server.url("/redirect-twice")),
    )
    .await
    .expect("two configured redirects must be followed");
    assert_eq!(output.bytes(), &b"\xff\xd8\xff\xe0binary-jpeg"[..]);
    two_redirects.close().await.expect("close limit-two client");
}

#[tokio::test]
async fn cancel_only_controls_binary_tasks_and_snapshot_excludes_them() {
    let server = BinaryTestServer::spawn();
    let client = client_with(
        BinaryDownloadConfig::builder()
            .request_timeout(Duration::from_secs(2))
            .retry_delays(Vec::new())
            .build()
            .expect("config"),
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task_id = client
        .try_enqueue_binary_task(BinaryTask::new(server.url("/slow")), move |_, result| {
            let _ = tx.send(result);
        })
        .expect("enqueue slow task");
    assert_eq!(
        client
            .pause(task_id)
            .await
            .expect_err("pause unsupported")
            .code(),
        InnerErrorCode::InvalidTaskState as i32
    );
    assert_eq!(
        client
            .resume(task_id)
            .await
            .expect_err("resume unsupported")
            .code(),
        InnerErrorCode::InvalidTaskState as i32
    );
    let snapshot = client.snapshot().await.expect("pounce snapshot");
    assert_eq!(snapshot.active_groups, 0);
    assert_eq!(snapshot.queued_groups, 0);
    client.cancel(task_id).await.expect("cancel binary task");
    let result = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("cancel callback timeout")
        .expect("cancel callback sender");
    assert_eq!(
        result.expect_err("canceled task must fail").code(),
        InnerErrorCode::TaskCanceled as i32
    );
    client.close().await.expect("close mixed client");
}

#[tokio::test]
async fn close_cancels_active_binary_task_and_rejects_new_work() {
    let server = BinaryTestServer::spawn();
    let client = client_with(BinaryDownloadConfig::default());
    let (tx, rx) = tokio::sync::oneshot::channel();
    client
        .try_enqueue_binary_task(BinaryTask::new(server.url("/slow")), move |_, result| {
            let _ = tx.send(result);
        })
        .expect("enqueue slow task");
    client.close().await.expect("close active binary client");
    let result = rx.await.expect("close callback sender");
    assert_eq!(
        result.expect_err("closed task must fail").code(),
        InnerErrorCode::ClientClosed as i32
    );
    let error = client
        .try_enqueue_binary_task(BinaryTask::new(server.url("/image")), |_, _| {})
        .expect_err("closed client must reject binary task");
    assert_eq!(error.code(), InnerErrorCode::ClientClosed as i32);

    let invalid_callback_called = Arc::new(AtomicBool::new(false));
    let callback_flag = Arc::clone(&invalid_callback_called);
    let error = client
        .try_enqueue_binary_task(BinaryTask::new("not-a-url"), move |_, _| {
            callback_flag.store(true, Ordering::SeqCst);
        })
        .expect_err("closed state must take precedence over invalid task input");
    assert_eq!(error.code(), InnerErrorCode::ClientClosed as i32);
    assert!(!invalid_callback_called.load(Ordering::SeqCst));
}

#[test]
fn mixed_close_uses_sdk_runtime_outside_tokio() {
    let server = BinaryTestServer::spawn();
    let client = client_with(BinaryDownloadConfig::default());
    let (tx, rx) = std::sync::mpsc::channel();
    client
        .try_enqueue_binary_task(BinaryTask::new(server.url("/slow")), move |_, result| {
            let _ = tx.send(result);
        })
        .expect("enqueue binary task");

    block_on_without_tokio(client.close()).expect("mixed close outside Tokio runtime");
    let result = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("terminal callback");
    assert_eq!(
        result.expect_err("closed binary task").code(),
        InnerErrorCode::ClientClosed as i32
    );
}

#[test]
fn dropping_binary_client_without_close_is_non_blocking() {
    let server = BinaryTestServer::spawn();
    let client = client_with(BinaryDownloadConfig::default());
    let (tx, rx) = std::sync::mpsc::channel();
    client
        .try_enqueue_binary_task(BinaryTask::new(server.url("/slow")), move |_, result| {
            let _ = tx.send(result);
        })
        .expect("enqueue slow task");

    let started = std::time::Instant::now();
    drop(client);
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "Drop must hand joining to the reaper"
    );
    let result = rx
        .recv_timeout(Duration::from_secs(3))
        .expect("Drop terminal callback");
    assert_eq!(
        result.expect_err("Drop closes active task").code(),
        InnerErrorCode::ClientClosed as i32
    );
}

#[test]
fn last_client_arc_can_be_released_on_binary_callback_thread() {
    struct DropProbe {
        client: Option<Arc<MeowClient>>,
        done: std::sync::mpsc::Sender<()>,
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            drop(self.client.take());
            let _ = self.done.send(());
        }
    }

    let server = BinaryTestServer::spawn();
    let client = Arc::new(client_with(BinaryDownloadConfig::default()));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let probe = DropProbe {
        client: Some(Arc::clone(&client)),
        done: done_tx,
    };
    client
        .try_enqueue_binary_task(BinaryTask::new(server.url("/image")), move |_, result| {
            assert!(result.is_ok());
            drop(probe);
        })
        .expect("enqueue callback-thread drop task");
    drop(client);

    done_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("callback-thread client Drop must not self-join");
}

#[tokio::test]
async fn binary_concurrency_is_two_and_callback_panic_is_isolated() {
    let server = BinaryTestServer::spawn();
    let client = client_with(BinaryDownloadConfig::default());
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
    for index in 0..6 {
        let done_tx = done_tx.clone();
        client
            .try_enqueue_binary_task(
                BinaryTask::new(server.url("/concurrency")),
                move |_, result| {
                    assert!(result.is_ok());
                    if index == 0 {
                        panic!("intentional callback panic");
                    }
                    let _ = done_tx.send(index);
                },
            )
            .expect("enqueue concurrency task");
    }
    drop(done_tx);
    let mut completed = Vec::new();
    while completed.len() < 5 {
        completed.push(
            tokio::time::timeout(Duration::from_secs(3), done_rx.recv())
                .await
                .expect("callback timeout")
                .expect("callback channel closed"),
        );
    }
    assert!(server.peak.load(Ordering::SeqCst) <= 2);
    client.close().await.expect("close binary client");
}

#[tokio::test]
async fn separate_clients_do_not_share_binary_concurrency_slots() {
    let server = BinaryTestServer::spawn();
    let first = client_with(BinaryDownloadConfig::default());
    let second = client_with(BinaryDownloadConfig::default());
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
    for client in [&first, &second] {
        for _ in 0..4 {
            let done_tx = done_tx.clone();
            client
                .try_enqueue_binary_task(
                    BinaryTask::new(server.url("/concurrency")),
                    move |_, result| {
                        assert!(result.is_ok());
                        let _ = done_tx.send(());
                    },
                )
                .expect("enqueue isolated concurrency task");
        }
    }
    drop(done_tx);
    for _ in 0..8 {
        tokio::time::timeout(Duration::from_secs(3), done_rx.recv())
            .await
            .expect("callback timeout")
            .expect("callback channel");
    }
    assert!(
        server.peak.load(Ordering::SeqCst) >= 3,
        "two clients must provide more than one client's two slots"
    );
    assert!(server.peak.load(Ordering::SeqCst) <= 4);
    first.close().await.expect("close first client");
    second.close().await.expect("close second client");
}

#[tokio::test]
async fn binary_outstanding_capacity_rejects_the_1025th_task() {
    let server = BinaryTestServer::spawn();
    let client = client_with(BinaryDownloadConfig::default());
    let callback_count = Arc::new(AtomicUsize::new(0));

    for _ in 0..1024 {
        let callback_count = Arc::clone(&callback_count);
        client
            .try_enqueue_binary_task(BinaryTask::new(server.url("/slow")), move |_, _| {
                callback_count.fetch_add(1, Ordering::SeqCst);
            })
            .expect("the first 1024 outstanding tasks must be accepted");
    }

    let rejected_callback_called = Arc::new(AtomicBool::new(false));
    let callback_flag = Arc::clone(&rejected_callback_called);
    let error = client
        .try_enqueue_binary_task(BinaryTask::new(server.url("/slow")), move |_, _| {
            callback_flag.store(true, Ordering::SeqCst);
        })
        .expect_err("the 1025th outstanding task must be rejected");
    assert_eq!(error.code(), InnerErrorCode::BinaryTaskQueueFull as i32);
    assert!(!rejected_callback_called.load(Ordering::SeqCst));

    client
        .close()
        .await
        .expect("close drains accepted callbacks");
    assert_eq!(callback_count.load(Ordering::SeqCst), 1024);
}

#[tokio::test]
async fn full_callback_path_does_not_block_cancel_control_plane() {
    let server = BinaryTestServer::spawn();
    let client = client_with(BinaryDownloadConfig::default());
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    client
        .try_enqueue_binary_task(BinaryTask::new(server.url("/image")), move |_, result| {
            assert!(result.is_ok());
            let _ = started_tx.send(());
            let _ = release_rx.recv();
        })
        .expect("enqueue blocking callback");
    tokio::time::timeout(Duration::from_secs(2), started_rx)
        .await
        .expect("blocking callback did not start")
        .expect("started sender dropped");

    let (second_tx, second_rx) = tokio::sync::oneshot::channel();
    client
        .try_enqueue_binary_task(BinaryTask::new(server.url("/image")), move |_, result| {
            let _ = second_tx.send(result);
        })
        .expect("enqueue second task");
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    let cancel_id = client
        .try_enqueue_binary_task(BinaryTask::new(server.url("/slow")), move |_, result| {
            let _ = cancel_tx.send(result);
        })
        .expect("enqueue pending task");
    tokio::time::timeout(Duration::from_millis(300), client.cancel(cancel_id))
        .await
        .expect("cancel must not wait for callback queue")
        .expect("cancel pending task");

    release_tx.send(()).expect("release callback");
    second_rx
        .await
        .expect("second callback sender")
        .expect("second result");
    let canceled = cancel_rx.await.expect("cancel callback sender");
    assert_eq!(
        canceled.expect_err("cancel result").code(),
        InnerErrorCode::TaskCanceled as i32
    );
    client.close().await.expect("close client");
}

#[tokio::test]
async fn close_waits_for_binary_callback_drain() {
    let server = BinaryTestServer::spawn();
    let client = Arc::new(client_with(BinaryDownloadConfig::default()));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    client
        .try_enqueue_binary_task(BinaryTask::new(server.url("/image")), move |_, result| {
            assert!(result.is_ok());
            let _ = started_tx.send(());
            let _ = release_rx.recv();
        })
        .expect("enqueue task");
    started_rx.await.expect("callback started");
    let close_client = Arc::clone(&client);
    let mut close = tokio::spawn(async move { close_client.close().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(80), &mut close)
            .await
            .is_err(),
        "close must wait while callback is still running"
    );
    release_tx.send(()).expect("release callback");
    close
        .await
        .expect("close join")
        .expect("close after callback drain");
}

#[tokio::test]
async fn callback_pending_task_keeps_binary_control_routing() {
    let server = BinaryTestServer::spawn();
    let client = client_with(BinaryDownloadConfig::default());
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let task_id = client
        .try_enqueue_binary_task(BinaryTask::new(server.url("/image")), move |_, result| {
            assert!(result.is_ok());
            let _ = started_tx.send(());
            let _ = release_rx.recv();
        })
        .expect("enqueue callback-pending task");
    started_rx.await.expect("callback started");

    assert_eq!(
        client
            .pause(task_id)
            .await
            .expect_err("callback-pending binary task cannot pause")
            .code(),
        InnerErrorCode::InvalidTaskState as i32
    );
    assert_eq!(
        client
            .resume(task_id)
            .await
            .expect_err("callback-pending binary task cannot resume")
            .code(),
        InnerErrorCode::InvalidTaskState as i32
    );
    assert_eq!(
        client
            .cancel(task_id)
            .await
            .expect_err("callback-pending task is already terminal")
            .code(),
        InnerErrorCode::TaskNotFound as i32
    );

    release_tx.send(()).expect("release callback");
    client.close().await.expect("close callback-routing client");
}

#[tokio::test]
async fn aborted_close_caller_does_not_strand_the_close_barrier() {
    let server = BinaryTestServer::spawn();
    let client = Arc::new(client_with(BinaryDownloadConfig::default()));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    client
        .try_enqueue_binary_task(BinaryTask::new(server.url("/image")), move |_, result| {
            assert!(result.is_ok());
            let _ = started_tx.send(());
            let _ = release_rx.recv();
        })
        .expect("enqueue blocking callback");
    started_rx.await.expect("callback started");

    let first_client = Arc::clone(&client);
    let first_close = tokio::spawn(async move { first_client.close().await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !client.is_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first close reached lifecycle barrier");
    first_close.abort();
    let _ = first_close.await;

    let second_client = Arc::clone(&client);
    let second_close = tokio::spawn(async move { second_client.close().await });
    release_tx.send(()).expect("release callback");
    let error = tokio::time::timeout(Duration::from_secs(2), second_close)
        .await
        .expect("second close must not hang after first caller is aborted")
        .expect("second close task")
        .expect_err("the waiting close keeps existing ClientClosed semantics");
    assert_eq!(error.code(), InnerErrorCode::ClientClosed as i32);
}

#[test]
fn invalid_tasks_fail_synchronously_without_callback() {
    let client = client_with(BinaryDownloadConfig::default());
    let callback_called = Arc::new(AtomicBool::new(false));
    for task in [
        BinaryTask::new("not-a-url"),
        BinaryTask::new("file:///tmp/data"),
        BinaryTask::new("https://user@example.com/data"),
        BinaryTask::new("https://example.com").with_max_body_bytes(0),
    ] {
        let callback_called = Arc::clone(&callback_called);
        let error = client
            .try_enqueue_binary_task(task, move |_, _| {
                callback_called.store(true, Ordering::SeqCst);
            })
            .expect_err("invalid task must fail synchronously");
        assert_eq!(error.code(), InnerErrorCode::ParameterEmpty as i32);
    }
    assert!(!callback_called.load(Ordering::SeqCst));
}

#[test]
fn configuration_rejects_exceptional_values() {
    assert!(BinaryDownloadConfig::builder()
        .max_body_bytes(0)
        .build()
        .is_err());
    assert!(BinaryDownloadConfig::builder()
        .request_timeout(Duration::ZERO)
        .build()
        .is_err());
    assert!(BinaryDownloadConfig::builder()
        .redirect_limit(usize::MAX)
        .build()
        .is_err());
    assert!(BinaryDownloadConfig::builder()
        .retry_delays(vec![Duration::from_millis(1); 9])
        .build()
        .is_err());
}
