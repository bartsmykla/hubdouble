use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use hubdouble_model::{ApiError, Requirement};
use hubdouble_rest::{Ctx, Registry, Reply};
use hubdouble_spec::OPERATIONS;

#[expect(clippy::unnecessary_wraps, reason = "a handler returns a Result")]
fn ok(_: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    Ok(Reply::empty())
}

fn bench(c: &mut Criterion) {
    let mut r = Registry::new();
    for op in &OPERATIONS {
        r.handle(op.id, Requirement::public(), ok);
    }
    let get = http::Method::GET;
    for path in [
        "/repos/octo-org/e2e/pulls/1",
        "/repos/octo-org/e2e/commits/heads/main/check-runs",
        "/repos/octo-org/e2e/contents/docs/a/b/c.md",
        "/app/installations/962416",
    ] {
        c.bench_function(&format!("route {path}"), |b| {
            b.iter(|| {
                r.route(black_box(&get), black_box(path))
                    .map(|x| x.operation_id)
            });
        });
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
