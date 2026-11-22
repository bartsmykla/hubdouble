use criterion::{Criterion, criterion_group, criterion_main};
use hubdouble_gql::Schema;

fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("schema");
    g.sample_size(10);
    g.bench_function("load", |b| b.iter(Schema::load));
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
