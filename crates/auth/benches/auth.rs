#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a benchmark fails loudly on a broken fixture"
)]

#[path = "../tests/common/mod.rs"]
#[expect(dead_code, reason = "the benchmark uses part of the test fixture")]
mod common;

use std::hint::black_box;

use common::{NOW, PRIVATE, app_jwt, bearer, store};
use criterion::{Criterion, criterion_group, criterion_main};
use hubdouble_auth::{Target, authenticate, authorize};
use hubdouble_model::{Grant, ManualClock, Requirement, Scope};
use hubdouble_store::Repos;

const CREATE_COMMENT: Requirement = Requirement {
    any_of: &[Grant::write("issues"), Grant::write("pull_requests")],
    scope: Scope::Repo,
    scopes: &["repo", "public_repo"],
};

fn bench(c: &mut Criterion) {
    let store = store();
    let clock = ManualClock::new(NOW);
    let token = bearer("ghs_full");
    let jwt = bearer(&app_jwt());
    let installation = store.view(|tx| authenticate(tx, &clock, &token)).unwrap();
    let carol = store
        .view(|tx| authenticate(tx, &clock, &bearer("ghp_carol")))
        .unwrap();
    let repo = store
        .view(|tx| Ok(tx.table::<Repos>().get(PRIVATE).cloned()))
        .unwrap()
        .expect("repo");

    c.bench_function("authenticate an installation token", |b| {
        b.iter(|| store.view(|tx| authenticate(tx, &clock, black_box(&token))));
    });
    c.bench_function("authenticate an App JWT", |b| {
        b.iter(|| store.view(|tx| authenticate(tx, &clock, black_box(&jwt))));
    });
    c.bench_function("authorize an installation on a repository", |b| {
        b.iter(|| {
            store.view(|tx| {
                authorize(
                    tx,
                    &installation,
                    &CREATE_COMMENT,
                    Target::Repo(black_box(&repo)),
                )
            })
        });
    });
    c.bench_function("authorize a user through a parent team", |b| {
        b.iter(|| {
            store.view(|tx| authorize(tx, &carol, &CREATE_COMMENT, Target::Repo(black_box(&repo))))
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
