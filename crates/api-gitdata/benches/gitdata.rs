use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use hubdouble_api_gitdata::{decode_base64, wrapped_base64};

fn file_contents(c: &mut Criterion) {
    let data: Vec<u8> = (0..64 * 1024)
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect();
    let encoded = wrapped_base64(&data);
    let mut group = c.benchmark_group("contents");
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.bench_function("encode a 64 KiB file", |b| {
        b.iter(|| wrapped_base64(black_box(&data)));
    });
    group.bench_function("decode a 64 KiB blob", |b| {
        b.iter(|| decode_base64(black_box(&encoded)));
    });
    group.finish();
}

criterion_group!(benches, file_contents);
criterion_main!(benches);
