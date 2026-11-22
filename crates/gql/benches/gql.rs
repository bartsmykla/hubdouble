#[path = "../tests/fake/mod.rs"]
mod fake;

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use hubdouble_gql::Schema;
use serde_json::json;

const LARGEST: &str = include_str!("../../../docs/inventory/graphql/implementPull.graphql");

fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("schema");
    g.sample_size(10);
    g.bench_function("load", |b| b.iter(Schema::load));
    g.finish();

    let mut g = c.benchmark_group("implementPull");
    g.bench_function("parse", |b| {
        b.iter(|| apollo_compiler::parser::Parser::new().parse_ast(black_box(LARGEST), "q"));
    });
    g.bench_function("validate", |b| {
        b.iter(|| fake::SCHEMA.check(black_box(LARGEST)));
    });
    let world = fake::World::new(fake::registry());
    let vars = json!({"owner": "o", "name": "one", "number": 1});
    g.bench_function("execute", |b| {
        b.iter(|| world.run(black_box(LARGEST), &vars));
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
