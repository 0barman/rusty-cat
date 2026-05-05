use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use rusty_cat::api::{
    FileTransferRecord, MeowClient, MeowConfig, PounceTask, TaskId, TransferStatus,
};

pub const FIVE_MB: usize = 5 * 1024 * 1024;
pub const ONE_MB: u64 = 1024 * 1024;

pub type AnyResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[allow(dead_code)]
pub fn placeholder(value: &str) -> bool {
    value.is_empty() || value.contains("CHANGE_ME") || value.contains("example.com")
}

pub fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(name)
}

pub fn make_file(path: &Path, size: usize) -> AnyResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut data = Vec::with_capacity(size);
    for i in 0..size {
        data.push((i % 251) as u8);
    }
    std::fs::write(path, data)?;
    Ok(())
}

pub fn remove_if_exists(path: &Path) -> AnyResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Box::new(e)),
    }
}

pub fn progress_bar(label: &str, total: u64) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "{msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} {percent}% {bytes_per_sec}",
        )
        .unwrap()
        .progress_chars("=>-"),
    );
    pb.set_message(label.to_string());
    pb
}

pub async fn run_task(client: &MeowClient, task: PounceTask, label: &str) -> AnyResult<TaskId> {
    let pb = progress_bar(label, 0);
    let done_pb = pb.clone();
    let label = label.to_string();
    let done_label = label.clone();
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let tx = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
    let progress_tx = std::sync::Arc::clone(&tx);
    let progress_label = label.clone();
    let complete_tx = std::sync::Arc::clone(&tx);
    let task_id = client
        .try_enqueue(
            task,
            move |record: FileTransferRecord| {
                pb.set_length(record.total_size());
                let pos = (record.progress().clamp(0.0, 1.0) * record.total_size() as f32) as u64;
                pb.set_position(pos.min(record.total_size()));
                match record.status() {
                    TransferStatus::Failed(e) => {
                        pb.finish_with_message(format!("{progress_label} failed"));
                        if let Ok(mut guard) = progress_tx.lock() {
                            if let Some(tx) = guard.take() {
                                let _ = tx.send(Err(format!("{progress_label} failed: {e}")));
                            }
                        }
                    }
                    TransferStatus::Canceled => {
                        pb.finish_with_message(format!("{progress_label} canceled"));
                        if let Ok(mut guard) = progress_tx.lock() {
                            if let Some(tx) = guard.take() {
                                let _ = tx.send(Err(format!("{progress_label} canceled")));
                            }
                        }
                    }
                    _ => {}
                }
            },
            move |_task_id, payload| {
                done_pb.finish_with_message(format!("{done_label} complete {payload:?}"));
                if let Ok(mut guard) = complete_tx.lock() {
                    if let Some(tx) = guard.take() {
                        let _ = tx.send(Ok(()));
                    }
                }
            },
        )
        .await?;
    match rx.recv_timeout(Duration::from_secs(180)) {
        Ok(Ok(())) => Ok(task_id),
        Ok(Err(e)) => Err(e.into()),
        Err(e) => Err(format!("wait transfer finished without completion callback: {e}").into()),
    }
}

pub fn config() -> AnyResult<MeowConfig> {
    Ok(MeowConfig::builder()
        .max_upload_concurrency(1)
        .max_download_concurrency(1)
        .build()?)
}
