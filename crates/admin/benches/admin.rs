use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use hubdouble_admin::{Admin, FaultQuery, LogEntry};

fn per_request_hooks(c: &mut Criterion) {
    let admin = Admin::new();
    let entry = LogEntry {
        method: "GET",
        path: "/repos/octo-org/e2e/pulls/1",
        operation_id: "pulls/get",
        actor: Some("agent-local[bot]"),
        status: 200,
    };
    c.bench_function("request log record", |b| {
        b.iter(|| admin.record(black_box(&entry)));
    });
    let query = FaultQuery {
        method: "GET",
        path: "/repos/octo-org/e2e/pulls/1",
        operation: "pulls/get",
    };
    c.bench_function("fault lookup without faults", |b| {
        b.iter(|| black_box(admin.fault(black_box(&query))));
    });
}

criterion_group!(benches, per_request_hooks);
criterion_main!(benches);
