#![expect(clippy::unwrap_used, reason = "a benchmark stops on a setup failure")]

use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use hubdouble_git::{EntryMode, FileChange, Git, GitBuilder, ObjectId, Signature};
use hubdouble_model::{Account, AccountKind, Repo, Timestamp, Visibility, node_id};
use hubdouble_store::{Accounts, Repos, Store, StoreBuilder};

const REPO: i64 = 2;

fn account() -> Account {
    Account {
        id: 1,
        node_id: node_id::entity(node_id::USER, 1),
        login: "owner".to_owned(),
        kind: AccountKind::User,
        name: None,
        email: Some("seed@hubdouble.invalid".to_owned()),
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

fn repo() -> Repo {
    Repo {
        id: REPO,
        node_id: node_id::entity(node_id::REPOSITORY, REPO),
        owner_id: 1,
        name: "r".to_owned(),
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

fn setup() -> (Store, Git) {
    let mut builder = StoreBuilder::new();
    let git = GitBuilder::new(&mut builder).build();
    let store = builder.build();
    store
        .update(|tx| {
            tx.table_mut::<Accounts>().insert(account())?;
            tx.table_mut::<Repos>().insert(repo())?;
            Ok(())
        })
        .unwrap();
    (store, git)
}

fn files(round: usize, count: usize) -> Vec<FileChange> {
    (0..count)
        .map(|i| FileChange::Write {
            path: format!("dir{}/file{i}.txt", i % 10),
            data: format!("line one\nround {round} file {i}\n")
                .as_bytes()
                .into(),
            mode: EntryMode::File,
        })
        .collect()
}

fn commit(store: &Store, git: &Git, branch: &str, changes: &[FileChange]) -> ObjectId {
    store
        .update(|tx| {
            let parent = git
                .resolve_ref(tx, REPO, &format!("refs/heads/{branch}"))
                .ok();
            let me = Signature::seed();
            Ok(git.commit_with_message(
                tx, REPO, branch, parent, changes, "change", &me, &me, false,
            )?)
        })
        .unwrap()
}

fn bench(c: &mut Criterion) {
    let (store, git) = setup();
    let base = commit(&store, &git, "main", &files(0, 100));

    let mut round = 0;
    c.bench_function("git_commit_100_file_change", |b| {
        b.iter_batched(
            || {
                round += 1;
                files(round, 100)
            },
            |changes| commit(&store, &git, "main", &changes),
            BatchSize::SmallInput,
        );
    });

    store
        .update(|tx| Ok(git.update_ref(tx, REPO, "refs/heads/left", None, Some(base), false)?))
        .unwrap();
    store
        .update(|tx| Ok(git.update_ref(tx, REPO, "refs/heads/right", None, Some(base), false)?))
        .unwrap();
    let (mut left, mut right) = (base, base);
    for i in 0..50 {
        left = commit(&store, &git, "left", &files(1000 + i, 3));
        right = commit(&store, &git, "right", &files(2000 + i, 3));
    }
    c.bench_function("git_compare_two_50_commit_branches", |b| {
        b.iter(|| {
            store
                .view(|tx| Ok(git.compare(tx, REPO, black_box(left), black_box(right))?))
                .unwrap()
        });
    });

    c.bench_function("git_resolve_ref", |b| {
        b.iter(|| {
            store
                .view(|tx| Ok(git.resolve_ref(tx, REPO, black_box("main"))?))
                .unwrap()
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
