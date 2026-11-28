use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use bytes::Bytes;
use hubdouble_model::{
    Account, AccountKind, ApiError, Delivery, DeliveryRequest, DeliveryResponse, Environment, Repo,
    Timestamp, Visibility, node_id,
};

use crate::{
    Accounts, Blob, Blobs, Committed, Counters, Deleted, DeletionSubscriber, Deliveries,
    Environments, OutboxEntry, OutboxUse, Repos, Sequence, Store, StoreBuilder, Table, TxMut,
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
        squash_merge_commit_title: hubdouble_model::SquashMergeCommitTitle::CommitOrPrTitle,
        squash_merge_commit_message: hubdouble_model::SquashMergeCommitMessage::CommitMessages,
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
            payload: Bytes::from_static(b"null"),
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
        hook_id: None,
        installation_id: None,
        repo_id: None,
        event: event.into(),
        action: None,
        payload: Bytes::from_static(b"{}"),
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
            link_key: None,
            cdn: false,
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
    let events: Vec<_> = taken.iter().map(|c| &*c.entry.event).collect();
    assert_eq!(events, ["create", "push", "issues"]);
    assert!(taken.windows(2).all(|w| w[0].seq < w[1].seq));
    assert_eq!(s.take_outbox(), Vec::new());
}

#[test]
fn an_entry_past_the_outbox_limits_commits_over_budget_until_space_is_released() {
    // Given an outbox bounded to two entries and five payload bytes
    let s = store();
    s.limit_outbox(OutboxUse {
        events: 2,
        bytes: 5,
        refused_per_app: usize::MAX,
        ..OutboxUse::default()
    });
    let sized = |n: usize| OutboxEntry {
        payload: Bytes::from(vec![b'1'; n]),
        ..entry("push")
    };
    let commit = |n: usize| {
        s.update(|tx| {
            tx.push_outbox(sized(n));
            Ok(())
        })
        .unwrap();
    };

    // When entries of 2, 4, 3, 1 and 1 bytes commit, the third after one release
    commit(2);
    commit(4);
    commit(3);
    s.release_outbox(2);
    commit(1);
    commit(1);
    let _ = s.update(|tx| -> Result<(), ApiError> {
        tx.push_outbox(sized(1));
        Err(ApiError::not_found())
    });

    // Then each entry fits only while both limits hold, and a rolled-back one holds nothing
    let over: Vec<_> = s.take_outbox().iter().map(|c| c.over_budget).collect();
    assert_eq!(over, [false, true, false, false, true]);
    assert_eq!(
        s.outbox_use(),
        OutboxUse {
            events: 2,
            bytes: 4,
            ..OutboxUse::default()
        }
    );
}

#[test]
fn over_budget_entries_keep_only_the_newest_payloads_within_the_refused_limit() {
    // Given an outbox with room for one byte and five bytes of refused payload
    let s = store();
    s.limit_outbox(OutboxUse {
        events: 10,
        bytes: 1,
        refused_bytes: 5,
        refused_per_app: usize::MAX,
    });
    let commit = |n: usize| {
        s.update(|tx| {
            tx.push_outbox(OutboxEntry {
                payload: Bytes::from(vec![b'1'; n]),
                ..entry("push")
            });
            Ok(())
        })
        .unwrap();
    };

    // When one entry fits and over-budget entries of 2, 2, 3 and 2 bytes follow
    for n in [1, 2, 2, 3, 2] {
        commit(n);
    }

    // Then the oldest over-budget entries lost their payloads and the rest stay within the limit
    assert_eq!(s.outbox_use().refused_bytes, 5);
    let kept: Vec<_> = s
        .take_outbox()
        .iter()
        .map(|c| (c.over_budget, c.entry.payload.len()))
        .collect();
    assert_eq!(
        kept,
        [(false, 1), (true, 0), (true, 0), (true, 3), (true, 2)]
    );
    assert_eq!(s.outbox_use().refused_bytes, 5);
}

#[test]
fn a_taken_over_budget_entry_holds_its_refused_share_until_it_is_released() {
    // Given an outbox with room for one byte and five bytes of refused payload
    let s = store();
    s.limit_outbox(OutboxUse {
        events: 10,
        bytes: 1,
        refused_bytes: 5,
        refused_per_app: usize::MAX,
    });
    let commit = |n: usize| {
        s.update(|tx| {
            tx.push_outbox(OutboxEntry {
                payload: Bytes::from(vec![b'1'; n]),
                ..entry("push")
            });
            Ok(())
        })
        .unwrap();
    };
    let kept = |taken: Vec<Committed>| -> Vec<usize> {
        taken
            .iter()
            .filter(|c| c.over_budget)
            .map(|c| c.entry.payload.len())
            .collect()
    };
    commit(1);
    commit(3);
    assert_eq!(kept(s.take_outbox()), [3]);

    // When another over-budget entry of 3 bytes commits while the taken one is not yet logged
    commit(3);

    // Then it loses its payload, since the taken one still holds 3 of the 5 bytes
    assert_eq!(kept(s.take_outbox()), [0]);
    assert_eq!(s.outbox_use().refused_bytes, 3);

    // When the taken one is released and another over-budget entry of 3 bytes commits
    s.release_refused(3);
    commit(3);

    // Then that entry keeps its payload
    assert_eq!(kept(s.take_outbox()), [3]);
}

#[test]
fn an_app_keeps_only_its_newest_over_budget_entries_within_the_refused_count() {
    // Given an outbox with no room, which keeps two over-budget entries per App
    let s = store();
    s.limit_outbox(OutboxUse {
        refused_bytes: usize::MAX,
        refused_per_app: 2,
        ..OutboxUse::default()
    });
    let commit = |app_id: i64, n: usize| {
        s.update(|tx| {
            tx.push_outbox(OutboxEntry {
                app_id,
                payload: Bytes::from(vec![b'1'; n]),
                ..entry("push")
            });
            Ok(())
        })
        .unwrap();
    };

    // When App 1 commits entries of 1, 2, 3 and 4 bytes around one of 5 bytes for App 2
    commit(1, 1);
    commit(1, 2);
    commit(2, 5);
    commit(1, 3);
    commit(1, 4);

    // Then App 1 keeps its two newest, App 2 keeps its one, in commit order, and only they hold bytes
    assert_eq!(s.outbox_use().refused_per_app, 2);
    assert_eq!(s.outbox_use().refused_bytes, 12);
    let kept: Vec<_> = s
        .take_outbox()
        .iter()
        .map(|c| (c.entry.app_id, c.entry.payload.len()))
        .collect();
    assert_eq!(kept, [(2, 5), (1, 3), (1, 4)]);
}

#[test]
fn restore_releases_what_the_dropped_outbox_entries_held() {
    // Given committed entries that hold a share of the outbox limits
    let s = store();
    s.limit_outbox(OutboxUse {
        events: 10,
        bytes: 100,
        ..OutboxUse::default()
    });
    let snapshot = s.snapshot();
    s.update(|tx| {
        tx.push_outbox(entry("push"));
        tx.push_outbox(entry("issues"));
        Ok(())
    })
    .unwrap();

    // When the store is restored before the publisher takes them
    s.restore(&snapshot);

    // Then nothing holds the limits
    assert_eq!(s.outbox_use(), OutboxUse::default());
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

#[expect(
    clippy::unnecessary_wraps,
    reason = "a deletion subscriber returns a Result"
)]
fn note_deletion(tx: &mut TxMut<'_>, deleted: &Deleted) -> Result<(), ApiError> {
    let name = match deleted {
        Deleted::Repo(r) => format!("repo {}", r.id),
        Deleted::Team(t) => format!("team {}", t.id),
        Deleted::Account(a) => format!("account {}", a.id),
        Deleted::Environment(e) => format!("environment {}", e.id),
    };
    let still_there = match deleted {
        Deleted::Repo(r) => tx.table::<Repos>().get(r.id).is_some(),
        Deleted::Team(_) => true,
        Deleted::Account(a) => tx.table::<Accounts>().get(a.id).is_some(),
        Deleted::Environment(e) => tx.table::<Environments>().get(e.id).is_some(),
    };
    tx.table_mut::<Notes>()
        .get_mut()
        .0
        .push(format!("{name} {still_there}"));
    Ok(())
}

fn refuse_deletion(_: &mut TxMut<'_>, _: &Deleted) -> Result<(), ApiError> {
    Err(ApiError::unprocessable("kept"))
}

fn deleting(subscriber: DeletionSubscriber) -> Store {
    let mut b = StoreBuilder::new();
    b.register_table::<Notes>().on_delete(subscriber);
    let s = b.build();
    s.update(|tx| {
        tx.table_mut::<Accounts>().insert(account(1, "octo"))?;
        tx.table_mut::<Accounts>().insert(account(2, "other"))?;
        tx.table_mut::<Repos>().insert(repo(10, 1, "first"))?;
        tx.table_mut::<Repos>().insert(repo(11, 1, "second"))?;
        tx.table_mut::<Repos>().insert(repo(12, 2, "kept"))?;
        Ok(())
    })
    .unwrap();
    s
}

#[test]
fn deleting_an_account_deletes_its_repositories_first_and_subscribers_see_the_rows() {
    // Given an account with two repositories and another account with one
    let s = deleting(note_deletion);

    // When the first account is deleted
    s.update(|tx| {
        let octo = tx.table::<Accounts>().get(1).cloned().unwrap();
        tx.delete(&Deleted::Account(octo))
    })
    .unwrap();

    // Then each repository went as its own deletion before the account, the subscriber saw
    // each row before the store dropped it, and the other account's repository stays
    s.view(|tx| {
        assert_eq!(
            tx.table::<Notes>().0,
            ["repo 10 true", "repo 11 true", "account 1 true"]
        );
        assert!(tx.table::<Accounts>().get(1).is_none());
        let repos: Vec<i64> = tx.table::<Repos>().iter().map(|r| r.id).collect();
        assert_eq!(repos, [12]);
        Ok(())
    })
    .unwrap();
}

fn environment(id: i64, repo_id: i64, name: &str) -> Environment {
    Environment {
        id,
        node_id: node_id::entity(node_id::ENVIRONMENT, id),
        repo_id,
        name: name.into(),
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
    }
}

#[test]
fn deleting_an_environment_drops_its_row_and_keeps_other_environments() {
    // Given two environments of a repository
    let s = deleting(note_deletion);
    s.update(|tx| {
        tx.table_mut::<Environments>()
            .insert(environment(20, 10, "staging"))?;
        tx.table_mut::<Environments>()
            .insert(environment(21, 10, "production"))?;
        Ok(())
    })
    .unwrap();

    // When one is deleted
    s.update(|tx| {
        let staging = tx
            .table::<Environments>()
            .by_name(10, "STAGING")
            .cloned()
            .unwrap();
        tx.delete(&Deleted::Environment(staging))
    })
    .unwrap();

    // Then the subscriber saw it before it went, and the other environment stays
    s.view(|tx| {
        assert_eq!(tx.table::<Notes>().0, ["environment 20 true"]);
        let kept: Vec<i64> = tx.table::<Environments>().iter().map(|e| e.id).collect();
        assert_eq!(kept, [21]);
        Ok(())
    })
    .unwrap();
}

#[test]
fn deleting_a_repository_deletes_its_environments_first() {
    // Given an environment in each of two repositories
    let s = deleting(note_deletion);
    s.update(|tx| {
        tx.table_mut::<Environments>()
            .insert(environment(20, 10, "staging"))?;
        tx.table_mut::<Environments>()
            .insert(environment(21, 12, "staging"))?;
        Ok(())
    })
    .unwrap();

    // When the first repository is deleted
    s.update(|tx| {
        let repo = tx.table::<Repos>().get(10).cloned().unwrap();
        tx.delete(&Deleted::Repo(repo))
    })
    .unwrap();

    // Then its environment went as its own deletion, and the other repository keeps its own
    s.view(|tx| {
        assert_eq!(
            tx.table::<Notes>().0,
            ["environment 20 true", "repo 10 true"]
        );
        let kept: Vec<i64> = tx.table::<Environments>().iter().map(|e| e.id).collect();
        assert_eq!(kept, [21]);
        Ok(())
    })
    .unwrap();
}

#[test]
fn a_subscriber_that_refuses_a_deletion_rolls_the_transaction_back() {
    // Given a subscriber that refuses every deletion
    let s = deleting(refuse_deletion);

    // When a repository is deleted
    let refused = s.update(|tx| {
        let repo = tx.table::<Repos>().get(10).cloned().unwrap();
        tx.delete(&Deleted::Repo(repo))
    });

    // Then the deletion fails and the repository stays
    assert_eq!(refused.unwrap_err().status, 422);
    assert!(
        s.view(|tx| Ok(tx.table::<Repos>().get(10).is_some()))
            .unwrap()
    );
}
