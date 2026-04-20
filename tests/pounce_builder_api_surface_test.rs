use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Method;
use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::http_breakpoint::{
    BreakpointDownloadHttpConfig, DefaultStyleUpload, StandardRangeDownload,
};
use rusty_cat::up_pounce_builder::UploadPounceBuilder;

fn temp_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_builder_surface_{case}_{ts}.bin"));
    p
}

#[test]
fn download_builder_all_chain_methods_take_effect_in_built_task() {
    // 场景说明：
    // 1) 连续调用 download builder 的所有 with_* 链式方法；
    // 2) build 后通过 Debug 输出断言关键字段已被带入；
    // 3) 覆盖 down_pounce_builder 的 API surface 路径，避免后续重构漏字段。
    let path = temp_path("download_surface_a");
    let moved_path = temp_path("download_surface_b");
    let mut headers = HeaderMap::new();
    headers.insert("x-case", HeaderValue::from_static("download-surface"));

    let task =
        DownloadPounceBuilder::new("origin.bin", &path, 2048, "http://127.0.0.1/a", Method::GET)
            .with_url("http://127.0.0.1/b")
            .with_file_path(&moved_path)
            .with_method(Method::POST)
            .with_headers(headers)
            .with_client_file_sign("client-sign-123")
            .with_breakpoint_download(Arc::new(StandardRangeDownload))
            .with_breakpoint_download_http(BreakpointDownloadHttpConfig {
                range_accept: "application/custom-surface".to_string(),
            })
            .with_max_chunk_retries(7)
            .build();

    let text = format!("{task:?}");
    assert!(text.contains("file_name: \"origin.bin\""));
    assert!(text.contains("url: \"http://127.0.0.1/b\""));
    assert!(text.contains("method: POST"));
    assert!(text.contains("client_file_sign: Some(\"client-sign-123\")"));
    assert!(text.contains("range_accept: \"application/custom-surface\""));
    assert!(text.contains("max_chunk_retries: 7"));
    assert!(text.contains("max_upload_prepare_retries: 3"));
}

#[test]
fn upload_builder_all_chain_methods_take_effect_and_build_success() {
    // 场景说明：
    // 1) 构造真实上传源文件并调用 upload builder 的链式 API；
    // 2) 覆盖 with_file_path/with_method/with_headers/with_breakpoint_upload/with_max_chunk_retries/with_max_upload_prepare_retries；
    // 3) build 成功后验证关键字段，确保 API 表面行为稳定。
    let src_a = temp_path("upload_surface_a");
    let src_b = temp_path("upload_surface_b");
    fs::write(&src_a, b"upload-surface-source-a").expect("write source a");
    fs::write(&src_b, b"upload-surface-source-b").expect("write source b");

    let mut headers = HeaderMap::new();
    headers.insert("x-case", HeaderValue::from_static("upload-surface"));
    let task = UploadPounceBuilder::new("up.bin", &src_a, 4096)
        .with_url("http://127.0.0.1/upload")
        .with_file_path(&src_b)
        .with_method(Method::PUT)
        .with_headers(headers)
        .with_breakpoint_upload(Arc::new(DefaultStyleUpload {
            category: "surface-cat".to_string(),
        }))
        .with_max_chunk_retries(9)
        .with_max_upload_prepare_retries(11)
        .build()
        .expect("upload builder build success");

    let text = format!("{task:?}");
    assert!(text.contains("file_name: \"up.bin\""));
    assert!(text.contains("url: \"http://127.0.0.1/upload\""));
    assert!(text.contains("method: PUT"));
    assert!(text.contains("max_chunk_retries: 9"));
    assert!(text.contains("max_upload_prepare_retries: 11"));
    assert!(text.contains("breakpoint_upload: Some("));

    fs::remove_file(&src_a).expect("cleanup source a");
    fs::remove_file(&src_b).expect("cleanup source b");
}

#[test]
fn upload_builder_from_bytes_avoids_file_metadata_lookup_and_keeps_size() {
    // 场景说明：
    // 1) 使用 from_bytes 构造上传任务，不依赖本地文件元数据；
    // 2) build 后 total_size 应等于 bytes 长度；
    // 3) Debug 输出中 upload_source 应是 Bytes(len=...) 形态。
    let payload = b"upload-builder-from-bytes".repeat(1024);
    let payload_len = payload.len();
    let task = UploadPounceBuilder::from_bytes("mem.bin", payload, 2048)
        .with_url("http://127.0.0.1/upload-bytes")
        .with_method(Method::PUT)
        .build()
        .expect("upload bytes builder build success");

    let text = format!("{task:?}");
    assert!(
        text.contains(&format!("total_size: {}", payload_len)),
        "total_size should match bytes payload length"
    );
    assert!(
        text.contains("upload_source: Some(Bytes"),
        "upload_source should be bytes for from_bytes builder"
    );
}
