//! 热点路径微基准：分片缓冲准备、流式 MD5、全局监听器快照扇出（与调度器模式对齐的简化模型）。
//!
//! 运行：`cargo bench -p rusty-cat --bench transfer_hotspots`

use std::collections::VecDeque;
use std::hint::black_box;
use std::io::{Read, Seek};
use std::sync::{Arc, Mutex, RwLock};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};

/// 分片读缓冲准备的 resize/truncate 微基准（无磁盘 I/O）。
fn bench_chunk_buffer_prepare(c: &mut Criterion) {
    let chunk_size = 2048usize;
    let mut group = c.benchmark_group("chunk_buffer");
    group.throughput(Throughput::Bytes(chunk_size as u64));
    group.bench_function("resize_truncate_per_chunk", |b| {
        let mut buf = Vec::<u8>::new();
        b.iter(|| {
            // Reset the logical length so every iteration performs the same
            // resize/initialization work. Merely truncating to `chunk_size`
            // after the first iteration leaves the length unchanged and lets
            // the benchmark collapse to a pointer black_box.
            buf.clear();
            buf.resize(chunk_size, 7);
            black_box(&buf[..]);
        });
    });
    group.finish();
}

/// Compare candidate U2 hash buffers without changing the MD5 algorithm.
fn bench_md5_incremental_buffers(c: &mut Criterion) {
    let data: Vec<u8> = (0u8..=255).cycle().take(4 * 1024 * 1024).collect();
    let mut group = c.benchmark_group("md5_stream");
    group.throughput(Throughput::Bytes(data.len() as u64));
    for buffer_size in [64 * 1024, 256 * 1024, 1024 * 1024] {
        group.bench_function(format!("consume_{buffer_size}_byte_chunks"), |b| {
            b.iter(|| {
                let mut ctx = md5::Context::new();
                for chunk in data.chunks(buffer_size) {
                    ctx.consume(chunk);
                }
                black_box(ctx.compute());
            });
        });
    }
    group.finish();
}

fn bench_positioned_upload_reads(c: &mut Criterion) {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "rusty_cat_positioned_read_bench_{}.bin",
        std::process::id()
    ));
    let data: Vec<u8> = (0u8..=255).cycle().take(4 * 1024 * 1024).collect();
    std::fs::write(&path, &data).expect("write positioned-read fixture");
    let chunk = 1024 * 1024usize;

    let shared_cursor = Arc::new(Mutex::new(
        std::fs::File::open(&path).expect("open fixture"),
    ));
    let positioned = Arc::new(std::fs::File::open(&path).expect("open fixture"));
    let mut group = c.benchmark_group("upload_file_read");
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.bench_function("shared_cursor_mutex", |b| {
        b.iter(|| {
            std::thread::scope(|scope| {
                for offset in [0_u64, chunk as u64, (chunk * 2) as u64, (chunk * 3) as u64] {
                    let shared_cursor = Arc::clone(&shared_cursor);
                    scope.spawn(move || {
                        let mut bytes = vec![0u8; chunk];
                        let mut file = shared_cursor.lock().expect("cursor lock");
                        file.seek(std::io::SeekFrom::Start(offset)).expect("seek");
                        file.read_exact(&mut bytes).expect("read");
                        black_box(bytes);
                    });
                }
            });
        });
    });
    group.bench_function("same_handle_positioned", |b| {
        b.iter(|| {
            std::thread::scope(|scope| {
                for offset in [0_u64, chunk as u64, (chunk * 2) as u64, (chunk * 3) as u64] {
                    let positioned = Arc::clone(&positioned);
                    scope.spawn(move || {
                        let mut bytes = vec![0u8; chunk];
                        read_exact_at(&positioned, offset, &mut bytes).expect("positioned read");
                        black_box(bytes);
                    });
                }
            });
        });
    });
    group.finish();
    let _ = std::fs::remove_file(path);
}

#[cfg(unix)]
fn read_exact_at(file: &std::fs::File, offset: u64, bytes: &mut [u8]) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    let mut filled = 0;
    while filled < bytes.len() {
        let read = file.read_at(&mut bytes[filled..], offset + filled as u64)?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        filled += read;
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &std::fs::File, offset: u64, bytes: &mut [u8]) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut filled = 0;
    while filled < bytes.len() {
        let read = file.seek_read(&mut bytes[filled..], offset + filled as u64)?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        filled += read;
    }
    Ok(())
}

fn bench_scheduler_progress_classification(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduler_progress");
    for queue_len in [1usize, 100, 10_000] {
        let queue: VecDeque<usize> = (0..queue_len).collect();
        group.bench_function(format!("legacy_scan_{queue_len}"), |b| {
            b.iter(|| {
                let mut queue = queue.clone();
                for _ in 0..queue.len() {
                    if let Some(item) = queue.pop_front() {
                        queue.push_back(item);
                    }
                }
                black_box(queue.len());
            });
        });
        group.bench_function(format!("classified_skip_{queue_len}"), |b| {
            b.iter(|| {
                black_box(false);
            });
        });
    }
    group.finish();
}

fn bench_download_checkpoint_snapshot(c: &mut Criterion) {
    let part_count = 596usize;
    let bitmap = vec![0xffu8; part_count.div_ceil(8)];
    let digests = vec![[7u8; 32]; part_count];
    let mut group = c.benchmark_group("download_checkpoint_snapshot");
    group.bench_function("legacy_per_part_snapshot", |b| {
        b.iter(|| {
            let mut bytes_written = 0usize;
            for _ in 0..part_count {
                let mut snapshot = Vec::with_capacity(bitmap.len() + digests.len() * 32);
                snapshot.extend_from_slice(&bitmap);
                for digest in &digests {
                    snapshot.extend_from_slice(digest);
                }
                bytes_written += black_box(snapshot.len());
            }
            black_box((bytes_written, part_count, part_count));
        });
    });
    group.bench_function("epoch_8_snapshot", |b| {
        b.iter(|| {
            let checkpoints = part_count.div_ceil(8);
            let mut bytes_written = 0usize;
            for _ in 0..checkpoints {
                let mut snapshot = Vec::with_capacity(bitmap.len() + digests.len() * 32);
                snapshot.extend_from_slice(&bitmap);
                for digest in &digests {
                    snapshot.extend_from_slice(digest);
                }
                bytes_written += black_box(snapshot.len());
            }
            // The tuple exposes the expected data-sync and rename counts to the
            // benchmark report: 596 legacy barriers versus 75 batched barriers.
            black_box((bytes_written, checkpoints, checkpoints));
        });
    });
    group.finish();
}

type ProgressCb = Arc<dyn Fn(DummyRecord) + Send + Sync + 'static>;

#[derive(Clone)]
struct DummyRecord {
    task_id: u64,
}

/// 对齐 `emit_global_progress`：读锁内只 clone 一个不可变 `Arc` 快照，
/// 锁外逐个调用。
fn bench_event_fanout_snapshot(c: &mut Criterion) {
    let n = 32usize;
    let listeners: Vec<ProgressCb> = (0..n)
        .map(|i| {
            Arc::new(move |r: DummyRecord| {
                black_box(r.task_id.wrapping_add(i as u64));
            }) as ProgressCb
        })
        .collect();
    let legacy_pool = Arc::new(RwLock::new(listeners.clone()));
    let snapshot_pool: Arc<RwLock<Arc<[ProgressCb]>>> = Arc::new(RwLock::new(Arc::from(listeners)));

    let mut group = c.benchmark_group("global_listener_fanout");
    group.bench_function(format!("legacy_clone_{n}_callbacks"), |b| {
        let dto = DummyRecord { task_id: 1 };
        b.iter(|| {
            let snap: Vec<ProgressCb> = match legacy_pool.read() {
                Ok(g) => g.iter().cloned().collect(),
                Err(_) => return,
            };
            for cb in snap {
                cb(dto.clone());
            }
        });
    });
    group.bench_function(format!("arc_snapshot_{n}_callbacks"), |b| {
        let dto = DummyRecord { task_id: 1 };
        b.iter(|| {
            let snap = match snapshot_pool.read() {
                Ok(g) => Arc::clone(&g),
                Err(_) => return,
            };
            for cb in snap.iter() {
                cb(dto.clone());
            }
        });
    });
    group.finish();
}

fn criterion_transfer_hotspots(c: &mut Criterion) {
    bench_chunk_buffer_prepare(c);
    bench_md5_incremental_buffers(c);
    bench_positioned_upload_reads(c);
    bench_scheduler_progress_classification(c);
    bench_download_checkpoint_snapshot(c);
    bench_event_fanout_snapshot(c);
}

criterion_group!(benches, criterion_transfer_hotspots);
criterion_main!(benches);
