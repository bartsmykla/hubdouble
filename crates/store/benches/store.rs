#![expect(clippy::unwrap_used, reason = "a benchmark stops on a setup failure")]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use hubdouble_model::{Account, AccountKind, Timestamp, node_id};
use hubdouble_store::{Accounts, StoreBuilder};

fn account(id: i64) -> Account {
    Account {
        id,
        node_id: node_id::entity(node_id::USER, id),
        login: format!("user{id}"),
        kind: AccountKind::User,
        name: None,
        email: None,
        company: None,
        blog: None,
        location: None,
        bio: None,
        twitter_username: None,
        description: None,
        site_admin: false,
        verified_email: true,
        sso_enforced: false,
        app_id: None,
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
    }
}

fn bench(c: &mut Criterion) {
    let store = StoreBuilder::new().build();
    store
        .update(|tx| {
            for _ in 0..1000 {
                let id = tx.next_id();
                tx.table_mut::<Accounts>().insert(account(id))?;
            }
            Ok(())
        })
        .unwrap();

    c.bench_function("store_view_lookup_by_id", |b| {
        b.iter(|| {
            store
                .view(|tx| Ok(tx.table::<Accounts>().get(black_box(500)).map(|a| a.id)))
                .unwrap()
        });
    });
    c.bench_function("store_update_replace_one_row", |b| {
        b.iter(|| {
            store
                .update(|tx| {
                    Ok(tx
                        .table_mut::<Accounts>()
                        .replace(account(black_box(500)))?
                        .is_some())
                })
                .unwrap()
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
