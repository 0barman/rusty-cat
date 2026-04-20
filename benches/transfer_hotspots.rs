//! 热点路径微基准：分片缓冲准备、流式 MD5、全局监听器快照扇出（与调度器模式对齐的简化模型）。
//!
//! 运行：`cargo bench -p rusty-cat --bench transfer_hotspots`

use std::hint::black_box;
use std::sync::{Arc, RwLock};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};

/// 对齐 `upload_one_chunk` 中对 `upload_chunk_buf` 的 resize/truncate 模式（无磁盘 I/O）。
fn bench_chunk_buffer_prepare(c: &mut Criterion) {
    let chunk_size = 2048usize;
    let mut group = c.benchmark_group("chunk_buffer");
    group.throughput(Throughput::Bytes(chunk_size as u64));
    group.bench_function("resize_truncate_per_chunk", |b| {
        let mut buf = Vec::<u8>::new();
        b.iter(|| {
            if buf.len() < chunk_size {
                buf.resize(chunk_size, 7);
            } else {
                buf.truncate(chunk_size);
            }
            black_box(buf.as_ptr());
        });
    });
    group.finish();
}

/// 对齐 `inner/sign.rs` 中按 64KiB 读取累加 MD5 的 CPU 形态（内存数据源）。
fn bench_md5_incremental_64k(c: &mut Criterion) {
    let data: Vec<u8> = (0u8..=255).cycle().take(4 * 1024 * 1024).collect();
    let mut group = c.benchmark_group("md5_stream");
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.bench_function("consume_64k_chunks", |b| {
        b.iter(|| {
            let mut ctx = md5::Context::new();
            for chunk in data.chunks(65536) {
                ctx.consume(chunk);
            }
            black_box(ctx.compute());
        });
    });
    group.finish();
}

type ProgressCb = Arc<dyn Fn(DummyRecord) + Send + Sync + 'static>;

#[derive(Clone)]
struct DummyRecord {
    task_id: u64,
}

/// 对齐 `emit_global_progress`：读锁内 clone 回调句柄，锁外逐个调用。
fn bench_event_fanout_snapshot(c: &mut Criterion) {
    let n = 32usize;
    let listeners: Vec<ProgressCb> = (0..n)
        .map(|i| {
            Arc::new(move |r: DummyRecord| {
                black_box(r.task_id.wrapping_add(i as u64));
            }) as ProgressCb
        })
        .collect();
    let pool = Arc::new(RwLock::new(listeners));

    let mut group = c.benchmark_group("global_listener_fanout");
    group.bench_function(format!("snapshot_and_invoke_{n}_callbacks"), |b| {
        let dto = DummyRecord { task_id: 1 };
        b.iter(|| {
            let snap: Vec<ProgressCb> = match pool.read() {
                Ok(g) => g.iter().cloned().collect(),
                Err(_) => return,
            };
            for cb in snap {
                cb(dto.clone());
            }
        });
    });
    group.finish();
}

fn criterion_transfer_hotspots(c: &mut Criterion) {
    bench_chunk_buffer_prepare(c);
    bench_md5_incremental_64k(c);
    bench_event_fanout_snapshot(c);
}

criterion_group!(benches, criterion_transfer_hotspots);
criterion_main!(benches);
