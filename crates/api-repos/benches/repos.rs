use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use hubdouble_model::{glob, ref_matches};
use serde_json::json;

fn branch_rule_matching(c: &mut Criterion) {
    c.bench_function("ruleset glob on a nested branch", |b| {
        b.iter(|| {
            black_box(glob(
                black_box("refs/heads/releases/**/*"),
                black_box("refs/heads/releases/v5/v5.1/hotfix"),
            ))
        });
    });
    let stars = format!("{}c*b", "a*".repeat(15));
    let deep = format!("{}c/*b", "**/".repeat(7));
    let long_ref = format!("{}b", "a".repeat(254));
    let long_path = format!("{}ab", "a/".repeat(126));
    c.bench_function("ruleset glob that backtracks on stars and fails", |b| {
        b.iter(|| {
            black_box(glob(black_box(&stars), black_box(&long_ref)))
                || black_box(glob(black_box(&deep), black_box(&long_path)))
        });
    });
    let conditions = json!({"ref_name": {
        "include": ["~DEFAULT_BRANCH", "refs/heads/release/*", "refs/heads/prompts/**"],
        "exclude": ["refs/heads/release/old-*"],
    }});
    c.bench_function("ruleset ref_name condition", |b| {
        b.iter(|| {
            black_box(ref_matches(
                Some(black_box(&conditions)),
                black_box("prompts/proposal-1"),
                "main",
            ))
        });
    });
}

criterion_group!(benches, branch_rule_matching);
criterion_main!(benches);
