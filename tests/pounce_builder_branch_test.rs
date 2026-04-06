use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Method;
use rusty_cat::down_pounce_builder::DownloadPounceBuilder;
use rusty_cat::up_pounce_builder::UploadPounceBuilder;

fn temp_file_path(case: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    p.push(format!("rusty_cat_builder_branch_{case}_{ts}.bin"));
    p
}

#[test]
fn upload_builder_build_nonexistent_file_hits_io_error_branch() {
    // 场景说明：
    // 1) 上传 builder 在 build 阶段会读取 metadata 获取 total_size；
    // 2) 当源文件不存在时，build 应返回 io::Error(NotFound)；
    // 3) 该用例覆盖 UploadPounceBuilder::build 的错误分支。
    let missing = temp_file_path("missing_src");
    let task = UploadPounceBuilder::new("x.bin", &missing, 1024)
        .with_url("http://127.0.0.1:9/upload")
        .build();
    let err = task.expect_err("missing source file should fail build");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "build should fail with NotFound for missing upload source"
    );
}

#[test]
fn upload_builder_chunk_size_zero_is_normalized_to_default() {
    // 场景说明：
    // 1) 传入 chunk_size=0，期望走 normalized_chunk_size 分支；
    // 2) 通过 Debug 输出检查最终任务 chunk_size 是否被归一化为 1MB；
    // 3) 该用例防止后续重构误删零值归一化逻辑。
    let src = temp_file_path("upload_chunk_zero");
    fs::write(&src, vec![1u8; 16]).expect("write tiny upload source");
    let task = UploadPounceBuilder::new("x.bin", &src, 0)
        .with_url("http://127.0.0.1:9/upload")
        .build()
        .expect("build upload task");

    let text = format!("{task:?}");
    assert!(
        text.contains("chunk_size: 1048576"),
        "chunk_size=0 should normalize to 1048576 bytes"
    );
    fs::remove_file(&src).expect("remove temp source");
}

#[test]
fn download_builder_chunk_size_zero_and_retry_zero_paths_are_kept() {
    // 场景说明：
    // 1) 下载 builder 的 chunk_size=0 会走默认分片大小归一化分支；
    // 2) with_max_chunk_retries(0) 应保留为 0（禁用重试分支）；
    // 3) 通过 Debug 输出断言两个字段都符合预期。
    let path = temp_file_path("download_chunk_retry_zero");
    let task = DownloadPounceBuilder::new("d.bin", &path, 0, "http://127.0.0.1:9/d", Method::GET)
        .with_max_chunk_retries(0)
        .build();
    let text = format!("{task:?}");
    assert!(
        text.contains("chunk_size: 1048576"),
        "download chunk_size=0 should normalize to 1048576 bytes"
    );
    assert!(
        text.contains("max_chunk_retries: 0"),
        "max_chunk_retries=0 should be kept as disabled retry"
    );
}
