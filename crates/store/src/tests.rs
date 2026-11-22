use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use bytes::Bytes;
use hubdouble_model::{
    Account, AccountKind, ApiError, Delivery, DeliveryRequest, DeliveryResponse, Repo, Timestamp,
    Visibility, node_id,
};

use crate::{
    Accounts, Blob, Blobs, Counters, Deliveries, OutboxEntry, Repos, Sequence, Store, StoreBuilder,
    Table, TxMut,
};

#[derive(Clone, Default, Debug, PartialEq)]
struct Notes(Vec<String>);

impl Table for Notes {}

fn account(id: i64, login: &str) -> Account {
    Account {
        id,
        node_id: node_id::entity(node_id::USER, id),
        login: login.to_owned(),
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

fn repo(id: i64, owner_id: i64, name: &str) -> Repo {
    Repo {
        id,
        node_id: node_id::entity(node_id::REPOSITORY, id),
        owner_id,
        name: name.to_owned(),
        description: None,
        homepage: None,
        language: None,
        topics: Vec::new(),
        visibility: Visibility::Private,
        fork_of: None,
        default_branch: "main".to_owned(),
        archived: false,
        disabled: false,
        is_template: false,
        has_issues: true,
        has_projects: true,
        has_wiki: true,
        has_discussions: false,
        allow_merge_commit: true,
        allow_squash_merge: true,
        allow_rebase_merge: true,
        allow_auto_merge: false,
        delete_branch_on_merge: false,
        collaborators: Vec::new(),
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
        pushed_at: None,
    }
}

fn delivery(id: i64) -> Delivery {
    Delivery {
        id,
        guid: format!("00000000-0000-4000-8000-{id:012}"),
        app_id: 1,
        hook_id: 1,
        installation_id: None,
        repo_id: None,
        event: "ping".to_owned(),
        action: None,
        url: "http://receiver/".to_owned(),
        delivered_at: Timestamp::SEED,
        redelivery: false,
        duration: 0.1,
        status: "OK".to_owned(),
        status_code: 200,
        request: DeliveryRequest {
            headers: BTreeMap::new(),
            payload: serde_json::Value::Null,
        },
        response: DeliveryResponse {
            headers: BTreeMap::new(),
            payload: None,
        },
    }
}

fn entry(event: &'static str) -> OutboxEntry {
    OutboxEntry {
        app_id: 1,
        installation_id: None,
        repo_id: None,
        event,
        action: None,
        payload: serde_json::json!({}),
        redelivery_of: None,
    }
}

fn store() -> Store {
    let mut b = StoreBuilder::new();
    b.register_table::<Notes>();
    b.build()
}

fn seed(s: &Store) {
    s.update_quiet(|tx| {
        let id = tx.next_id();
        tx.table_mut::<Accounts>().insert(account(id, "Octo"))?;
        let rid = tx.next_id();
        tx.table_mut::<Repos>().insert(repo(rid, id, "Hello"))?;
        tx.table_mut::<Notes>()
            .get_mut()
            .0
            .push("seeded".to_owned());
        Ok(())
    })
    .unwrap();
}

#[derive(Debug, PartialEq)]
struct Seen {
    logins: Vec<String>,
    repo_found: bool,
    notes: Notes,
    next_id: i64,
    issue_number: i64,
    blob: bool,
}

fn seen(s: &Store) -> Seen {
    s.view(|tx| {
        Ok(Seen {
            logins: tx
                .table::<Accounts>()
                .iter()
                .map(|a| a.login.clone())
                .collect(),
            repo_found: tx
                .table::<Accounts>()
                .by_login("octo")
                .is_some_and(|a| tx.table::<Repos>().by_name(a.id, "hello").is_some()),
            notes: tx.table::<Notes>().clone(),
            next_id: tx.table::<Counters>().current(Sequence::Id),
            issue_number: tx.table::<Counters>().number(2),
            blob: tx.table::<Blobs>().get("log").is_some(),
        })
    })
    .unwrap()
}

fn change_everything(tx: &mut TxMut<'_>) -> Result<(), ApiError> {
    let id = tx.next_id();
    tx.table_mut::<Accounts>().insert(account(id, "hubot"))?;
    tx.table_mut::<Accounts>()
        .replace(account(1, "octo-renamed"))?;
    tx.table_mut::<Repos>().remove(2);
    tx.table_mut::<Notes>()
        .get_mut()
        .0
        .push("changed".to_owned());
    tx.table_mut::<Counters>().next_number(2);
    tx.table_mut::<Blobs>().insert(
        "log".to_owned(),
        Blob {
            content_type: "text/plain".to_owned(),
            data: Bytes::from_static(b"x"),
        },
    );
    tx.push_outbox(entry("push"));
    Ok(())
}

#[test]
fn a_failed_update_rolls_back_every_table_and_keeps_no_outbox_entry() {
    // Given a seeded store
    let s = store();
    seed(&s);
    let before = seen(&s);

    // When an update changes every kind of table and then fails
    let result = s.update(|tx| {
        change_everything(tx)?;
        Err::<(), _>(ApiError::unprocessable("no"))
    });

    // Then the error comes back, nothing changed and the outbox is empty
    assert_eq!(result.unwrap_err().status, 422);
    assert_eq!(seen(&s), before);
    assert_eq!(s.take_outbox(), Vec::new());
}

#[test]
fn a_panicking_update_rolls_back_every_table_and_keeps_no_outbox_entry() {
    let s = store();
    seed(&s);
    let before = seen(&s);

    let result = catch_unwind(AssertUnwindSafe(|| {
        s.update(|tx| -> Result<(), ApiError> {
            change_everything(tx)?;
            panic!("handler bug")
        })
    }));

    assert!(result.is_err());
    assert_eq!(seen(&s), before);
    assert_eq!(s.take_outbox(), Vec::new());
    s.update(change_everything).unwrap();
    assert_eq!(seen(&s).logins, ["octo-renamed", "hubot"]);
}

#[test]
fn committed_outbox_entries_keep_commit_order_and_quiet_updates_deliver_nothing() {
    let s = store();
    s.update(|tx| {
        tx.push_outbox(entry("create"));
        tx.push_outbox(entry("push"));
        Ok(())
    })
    .unwrap();
    s.update_quiet(|tx| {
        tx.push_outbox(entry("ping"));
        Ok(())
    })
    .unwrap();
    s.update(|tx| {
        tx.push_outbox(entry("issues"));
        Ok(())
    })
    .unwrap();

    let taken = s.take_outbox();
    let events: Vec<_> = taken.iter().map(|c| c.entry.event).collect();
    assert_eq!(events, ["create", "push", "issues"]);
    assert!(taken.windows(2).all(|w| w[0].seq < w[1].seq));
    assert_eq!(s.take_outbox(), Vec::new());
}

#[test]
fn restore_brings_back_the_seed_but_keeps_counters_and_the_delivery_log() {
    // Given a seeded store, its snapshot, and later changes
    let s = store();
    seed(&s);
    let snapshot = s.snapshot();
    let ids_at_seed = seen(&s).next_id;
    s.update(|tx| {
        change_everything(tx)?;
        tx.table_mut::<Deliveries>().insert(delivery(7))?;
        Ok(())
    })
    .unwrap();
    let epoch = s.epoch();

    // When the store is restored
    s.restore(&snapshot);

    // Then seeded rows and tables come back, sequences keep counting up, the delivery log stays, pending outbox entries are gone and the epoch moved on
    let after = seen(&s);
    assert_eq!(after.logins, ["Octo"]);
    assert!(after.repo_found);
    assert_eq!(after.notes, Notes(vec!["seeded".to_owned()]));
    assert!(!after.blob);
    assert_eq!(after.next_id, ids_at_seed + 1);
    assert_eq!(after.issue_number, 1);
    assert!(
        s.view(|tx| Ok(tx.table::<Deliveries>().get(7).is_some()))
            .unwrap()
    );
    assert_eq!(s.take_outbox(), Vec::new());
    assert!(s.epoch() > epoch);
    assert_eq!(s.update(|tx| Ok(tx.next_id())).unwrap(), ids_at_seed + 2);
}

#[test]
fn an_index_key_held_by_another_row_is_refused() {
    let s = store();
    seed(&s);

    let taken = s.update(|tx| {
        let id = tx.next_id();
        tx.table_mut::<Accounts>().insert(account(id, "OCTO"))?;
        Ok(())
    });

    assert_eq!(taken.unwrap_err().status, 422);
    assert_eq!(seen(&s).logins, ["Octo"]);
}
