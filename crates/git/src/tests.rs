use std::sync::Arc;

use hubdouble_model::{
    Account, AccountKind, ApiError, Config, ManualClock, Repo, Timestamp, Visibility, node_id,
};
use hubdouble_store::{Accounts, Repos, Store, StoreBuilder, Table, TxMut};
use pgp::composed::{Deserializable, DetachedSignature, SignedPublicKey};
use serde_json::{Value, json};

use crate::{
    Commit, EntryMode, FileChange, FileStatus, Git, GitBuilder, GitError, ObjectData, ObjectId,
    ObjectKind, RefChange, RefSubscriber, Signature, SignatureState, Tag, TreeEntry,
};

const ORG: i64 = 1;
const ALICE: i64 = 2;
const REPO: i64 = 10;
const FORK: i64 = 11;
const OTHER: i64 = 12;
const MERGE_TIME: Timestamp = Timestamp(1_767_229_200);

#[derive(Clone, Default)]
struct Seen(Vec<RefChange>);

impl Table for Seen {}

#[expect(
    clippy::unnecessary_wraps,
    reason = "a ref subscriber returns a Result"
)]
fn record(tx: &mut TxMut<'_>, _: &Git, change: &RefChange) -> Result<(), ApiError> {
    tx.table_mut::<Seen>().get_mut().0.push(change.clone());
    Ok(())
}

fn refuse_blocked(_: &mut TxMut<'_>, _: &Git, change: &RefChange) -> Result<(), ApiError> {
    if change.name.contains("blocked") {
        return Err(ApiError::forbidden("blocked"));
    }
    Ok(())
}

fn account(id: i64, login: &str, kind: AccountKind, email: Option<&str>) -> Account {
    Account {
        id,
        node_id: node_id::entity(node_id::USER, id),
        login: login.to_owned(),
        kind,
        name: None,
        email: email.map(str::to_owned),
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

fn repo(id: i64, owner_id: i64, name: &str, fork_of: Option<i64>) -> Repo {
    Repo {
        id,
        node_id: node_id::entity(node_id::REPOSITORY, id),
        owner_id,
        name: name.to_owned(),
        description: Some("the repo".to_owned()),
        homepage: None,
        language: None,
        topics: Vec::new(),
        visibility: Visibility::Private,
        fork_of,
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

struct Fx {
    store: Store,
    git: Git,
}

fn fixture_with(subscribers: &[RefSubscriber]) -> Fx {
    let mut builder = StoreBuilder::new();
    builder.register_table::<Seen>();
    let mut git = GitBuilder::new(&mut builder);
    git.on_ref_update(record);
    for s in subscribers {
        git.on_ref_update(*s);
    }
    git.clock(Arc::new(ManualClock::new(MERGE_TIME)));
    let config = Config {
        web_url: "https://github.com".to_owned(),
        ..Config::default()
    };
    let fx = Fx {
        store: builder.build_with(Arc::new(config)),
        git: git.build(),
    };
    fx.store
        .update(|tx| {
            let mut accounts = tx.table_mut::<Accounts>();
            accounts.insert(account(ORG, "octo-org", AccountKind::Organization, None))?;
            accounts.insert(account(
                ALICE,
                "alice",
                AccountKind::User,
                Some("alice@example.com"),
            ))?;
            let mut repos = tx.table_mut::<Repos>();
            repos.insert(repo(REPO, ORG, "r", None))?;
            repos.insert(repo(FORK, ALICE, "r", Some(REPO)))?;
            repos.insert(repo(OTHER, ORG, "other", None))?;
            Ok(())
        })
        .unwrap();
    fx
}

fn fixture() -> Fx {
    fixture_with(&[])
}

fn alice() -> Signature {
    Signature {
        name: "Alice".to_owned(),
        email: "alice@example.com".to_owned(),
        ..Signature::seed()
    }
}

fn write(path: &str, text: &str) -> FileChange {
    FileChange::Write {
        path: path.to_owned(),
        data: text.as_bytes().into(),
        mode: EntryMode::File,
    }
}

fn delete(path: &str) -> FileChange {
    FileChange::Delete {
        path: path.to_owned(),
    }
}

impl Fx {
    fn commit_as(
        &self,
        repo: i64,
        branch: &str,
        changes: &[FileChange],
        message: &str,
        author: &Signature,
    ) -> Result<ObjectId, ApiError> {
        self.store.update(|tx| {
            let parent = self
                .git
                .resolve_ref(tx, repo, &format!("refs/heads/{branch}"))
                .ok();
            Ok(self.git.commit(
                tx, repo, ALICE, branch, parent, changes, message, author, author, false,
            )?)
        })
    }

    fn commit(&self, branch: &str, changes: &[FileChange], message: &str) -> ObjectId {
        self.commit_as(REPO, branch, changes, message, &Signature::seed())
            .unwrap()
    }

    fn branch(&self, repo: i64, name: &str, at: ObjectId) {
        self.store
            .update(|tx| {
                Ok(self
                    .git
                    .update_ref(tx, repo, ALICE, name, None, Some(at), false)?)
            })
            .unwrap();
    }

    fn head(&self, repo: i64, name: &str) -> Result<ObjectId, GitError> {
        self.store
            .view(|tx| Ok(self.git.resolve_ref(tx, repo, name)))
            .unwrap()
    }

    fn commit_object(&self, repo: i64, id: ObjectId) -> Arc<Commit> {
        self.store
            .view(|tx| match self.git.object(tx, repo, id)?.data {
                ObjectData::Commit(c) => Ok(c),
                _ => Err(ApiError::internal("not a commit")),
            })
            .unwrap()
    }

    fn seen(&self) -> Vec<RefChange> {
        self.store
            .view(|tx| Ok(tx.table::<Seen>().0.clone()))
            .unwrap()
    }

    fn update_ref(
        &self,
        name: &str,
        old: Option<ObjectId>,
        new: Option<ObjectId>,
        force: bool,
    ) -> Result<(), ApiError> {
        self.store.update(|tx| {
            Ok(self
                .git
                .update_ref(tx, REPO, ALICE, name, old, new, force)?)
        })
    }
}

fn oid(hex: &str) -> ObjectId {
    hex.parse().unwrap()
}

#[test]
fn object_ids_round_trip_as_lowercase_hex() {
    let text = "0123456789abcdef0123456789ABCDEF01234567";
    let id: ObjectId = text.parse().unwrap();
    assert_eq!(id.to_string(), text.to_ascii_lowercase());
    assert!("0123".parse::<ObjectId>().is_err());
    assert!("g".repeat(40).parse::<ObjectId>().is_err());
}

#[test]
fn objects_hash_to_the_ids_git_gives_them() {
    // Given the SHAs `git hash-object`, `git write-tree` and `git commit-tree` print for the same content
    let fx = fixture();

    // When the same blobs, trees and commits are written
    let first = fx.commit(
        "main",
        &[
            write("c.txt", "c\n"),
            write("a/b.txt", "b\n"),
            FileChange::Write {
                path: "run.sh".to_owned(),
                data: b"#!/bin/sh\n".as_slice().into(),
                mode: EntryMode::Executable,
            },
        ],
        "init\n",
    );
    let second = fx.commit(
        "main",
        &[delete("c.txt"), write("a/b.txt", "B\n")],
        "second\n",
    );

    // Then every ID is git's
    assert_eq!(first, oid("e9af0c6524aed1477408dd8f16ba36e3f7a5856a"));
    assert_eq!(
        fx.commit_object(REPO, first).tree,
        oid("9e993d2018de5c9f3ce713459fcfe1f12891bbd8")
    );
    assert_eq!(second, oid("fbcdb62fab6f4673232f9c63fd8c7af4384232c2"));
    let tree2 = fx.commit_object(REPO, second).tree;
    assert_eq!(tree2, oid("e604897117905cf16ce99110956e7e39cd317b57"));
    fx.store
        .view(|tx| {
            assert_eq!(
                fx.git.write_blob(tx, REPO, b"hello\n")?,
                oid("ce013625030ba8dba906f756967f9e9ca394464a")
            );
            assert_eq!(
                fx.git.write_tree(tx, REPO, &[])?,
                oid("4b825dc642cb6eb9a060e54bf8d69288fbee4904")
            );
            let zoned = Commit {
                tree: tree2,
                parents: Vec::new(),
                author: Signature::seed(),
                committer: Signature {
                    time: Timestamp(1_767_229_200),
                    offset_minutes: 60,
                    ..Signature::seed()
                },
                message: "tz\n".to_owned(),
                gpg_signature: None,
            };
            assert_eq!(
                fx.git.write_commit(tx, REPO, &zoned)?,
                oid("daedf811fe96cb22ae2610906d7f40b1590a2842")
            );
            Ok(())
        })
        .unwrap();
}

fn seed_history(fx: &Fx) -> Vec<(String, ObjectId)> {
    let base = fx.commit("main", &[write("b.md", "b"), write("a.md", "a")], "one\n");
    fx.store
        .update(|tx| {
            let tag = fx.git.write_tag(
                tx,
                REPO,
                &Tag {
                    target: base,
                    target_kind: ObjectKind::Commit,
                    name: "v1.0.0".to_owned(),
                    tagger: Some(Signature::seed()),
                    message: "v1.0.0\n".to_owned(),
                    gpg_signature: None,
                },
            )?;
            fx.git
                .update_ref(tx, REPO, ALICE, "refs/tags/v1.0.0", None, Some(tag), false)?;
            Ok(())
        })
        .unwrap();
    fx.commit("main", &[write("docs/c.md", "c")], "two\n");
    fx.store
        .view(|tx| {
            Ok(fx
                .git
                .refs(tx, REPO, "")
                .into_iter()
                .map(|r| (r.name, r.target))
                .collect())
        })
        .unwrap()
}

#[test]
fn the_same_seed_gives_the_same_shas() {
    // Given two fresh servers
    let (one, two) = (fixture(), fixture());

    // When each applies the same seeded history
    let (refs_one, refs_two) = (seed_history(&one), seed_history(&two));

    // Then every ref and the tag's peeled commit are the same
    assert_eq!(refs_one, refs_two);
    assert_eq!(refs_one.len(), 2);
    let peel = |fx: &Fx| {
        fx.store
            .view(|tx| {
                let tag = fx.git.resolve_ref(tx, REPO, "v1.0.0")?;
                Ok(fx.git.peel_to_commit(tx, REPO, tag)?)
            })
            .unwrap()
    };
    assert_eq!(peel(&one), peel(&two));
}

#[test]
fn update_ref_checks_the_expected_value_and_fast_forwards() {
    let fx = fixture();
    let c1 = fx.commit("main", &[write("a", "1")], "1");
    let c2 = fx.commit("main", &[write("a", "2")], "2");

    // Given a branch at c2, when it is created again, moved from a stale value or rewound
    fx.update_ref("refs/heads/topic", None, Some(c1), false)
        .unwrap();
    let exists = fx.update_ref("refs/heads/topic", None, Some(c2), false);
    let stale = fx.update_ref("refs/heads/topic", Some(c2), Some(c1), false);
    let rewind = fx.update_ref("refs/heads/main", Some(c2), Some(c1), false);

    // Then each is refused with GitHub's status and the refs stay
    assert_eq!(exists.unwrap_err().status, 422);
    assert_eq!(stale.unwrap_err().status, 422);
    assert_eq!(rewind.unwrap_err().status, 422);
    assert_eq!(fx.head(REPO, "refs/heads/main"), Ok(c2));
    assert_eq!(fx.head(REPO, "refs/heads/topic"), Ok(c1));

    // When forced, a fast-forward and a delete go through
    fx.update_ref("refs/heads/main", Some(c2), Some(c1), true)
        .unwrap();
    fx.update_ref("refs/heads/topic", Some(c1), Some(c2), false)
        .unwrap();
    fx.update_ref("refs/heads/topic", Some(c2), None, false)
        .unwrap();

    // Then the subscriber saw each change with its forced flag
    let seen = fx.seen();
    let last: Vec<_> = seen.iter().rev().take(3).rev().cloned().collect();
    assert_eq!(
        last,
        vec![
            RefChange {
                repo_id: REPO,
                sender: ALICE,
                name: "refs/heads/main".to_owned(),
                old: Some(c2),
                new: Some(c1),
                forced: true,
            },
            RefChange {
                repo_id: REPO,
                sender: ALICE,
                name: "refs/heads/topic".to_owned(),
                old: Some(c1),
                new: Some(c2),
                forced: false,
            },
            RefChange {
                repo_id: REPO,
                sender: ALICE,
                name: "refs/heads/topic".to_owned(),
                old: Some(c2),
                new: None,
                forced: false,
            },
        ]
    );
    assert_eq!(fx.head(REPO, "refs/heads/topic"), Err(GitError::NotFound));
}

#[test]
fn update_ref_refuses_bad_names_and_targets() {
    let fx = fixture();
    let c1 = fx.commit("main", &[write("a", "1")], "1");
    let errors = |name: &str, new: Option<ObjectId>| {
        fx.store
            .update(|tx| Ok(fx.git.update_ref(tx, REPO, ALICE, name, None, new, false)))
            .unwrap()
            .unwrap_err()
    };
    for name in [
        "main",
        "refs/",
        "refs/heads/a..b",
        "refs/heads/a b",
        "refs/heads/x.lock",
        "refs/heads/.x",
        "refs/heads//x",
    ] {
        assert!(
            matches!(errors(name, Some(c1)), GitError::Invalid(_)),
            "{name}"
        );
    }
    let blob = fx
        .store
        .view(|tx| Ok(fx.git.write_blob(tx, REPO, b"x")?))
        .unwrap();
    assert_eq!(errors("refs/heads/blob", Some(blob)), GitError::WrongKind);
    assert_eq!(
        errors("refs/heads/none", Some(ObjectId([7; 20]))),
        GitError::NotFound
    );
    assert_eq!(errors("refs/heads/none", None), GitError::NotFound);
    let before = fx.seen().len();
    fx.update_ref("refs/heads/main", Some(c1), Some(c1), false)
        .unwrap();
    assert_eq!(fx.seen().len(), before);
}

#[test]
fn a_refused_or_failed_update_leaves_the_refs_as_they_were() {
    let fx = fixture_with(&[refuse_blocked]);
    let c1 = fx.commit("main", &[write("a", "1")], "1");

    // When a subscriber refuses one update, and another update fails after moving a ref
    let refused = fx.update_ref("refs/heads/blocked", None, Some(c1), false);
    let failed: Result<(), ApiError> = fx.store.update(|tx| {
        fx.git
            .update_ref(tx, REPO, ALICE, "refs/heads/later", None, Some(c1), false)?;
        Err(ApiError::internal("later step failed"))
    });

    // Then the subscriber's error comes back and neither ref exists
    assert_eq!(refused.unwrap_err().status, 403);
    assert!(failed.is_err());
    assert_eq!(fx.head(REPO, "refs/heads/blocked"), Err(GitError::NotFound));
    assert_eq!(fx.head(REPO, "refs/heads/later"), Err(GitError::NotFound));
    assert_eq!(fx.seen().len(), 1);
}

#[test]
fn reset_brings_the_seeded_refs_back() {
    let fx = fixture();
    let c1 = fx.commit("main", &[write("a", "1")], "1");
    let snapshot = fx.store.snapshot();
    fx.commit("main", &[write("a", "2")], "2");
    fx.update_ref("refs/heads/extra", None, Some(c1), false)
        .unwrap();

    fx.store.restore(&snapshot);

    assert_eq!(fx.head(REPO, "main"), Ok(c1));
    assert_eq!(fx.head(REPO, "extra"), Err(GitError::NotFound));
}

#[test]
fn refs_resolve_short_names_and_list_by_prefix() {
    let fx = fixture();
    let c1 = fx.commit("main", &[write("a", "1")], "1");
    fx.branch(REPO, "refs/heads/prompts/b", c1);
    fx.branch(REPO, "refs/heads/prompts/a", c1);
    fx.branch(REPO, "refs/heads/promptsx", c1);
    fx.branch(REPO, "refs/tags/main", c1);

    fx.store
        .view(|tx| {
            let names = |prefix: &str| -> Vec<String> {
                fx.git
                    .refs(tx, REPO, prefix)
                    .into_iter()
                    .map(|r| r.name)
                    .collect()
            };
            assert_eq!(
                names("refs/heads/prompts/"),
                ["refs/heads/prompts/a", "refs/heads/prompts/b"]
            );
            assert_eq!(names("tags/"), ["refs/tags/main"]);
            assert_eq!(names("refs/heads/none/"), Vec::<String>::new());
            assert_eq!(fx.git.refs(tx, OTHER, ""), []);
            assert_eq!(fx.git.resolve_ref(tx, REPO, "HEAD"), Ok(c1));
            assert_eq!(fx.git.resolve_ref(tx, REPO, "heads/prompts/a"), Ok(c1));
            assert_eq!(fx.git.resolve_ref(tx, REPO, "prompts/a"), Ok(c1));
            assert_eq!(
                fx.git.resolve_ref(tx, REPO, "nope"),
                Err(GitError::NotFound)
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn commits_refuse_bad_paths_and_a_stale_parent() {
    let fx = fixture();
    let c1 = fx.commit("main", &[write("c.txt", "c"), write("d/e.txt", "e")], "1");
    for change in [
        write("../x", "x"),
        write("a//b", "x"),
        write("", "x"),
        write(".git/config", "x"),
        write("c.txt/x", "x"),
        write("d", "x"),
        delete("missing.txt"),
        delete("missing/x"),
        delete("d"),
    ] {
        let result = fx.commit_as(
            REPO,
            "main",
            std::slice::from_ref(&change),
            "x",
            &Signature::seed(),
        );
        assert_eq!(result.unwrap_err().status, 422, "{change:?}");
    }
    let stale = fx.store.update(|tx| {
        Ok(fx.git.commit(
            tx,
            REPO,
            ALICE,
            "main",
            None,
            &[write("x", "x")],
            "x",
            &Signature::seed(),
            &Signature::seed(),
            false,
        )?)
    });
    assert_eq!(stale.unwrap_err().status, 422);
    assert_eq!(fx.head(REPO, "main"), Ok(c1));
}

#[test]
fn expressions_name_blobs_trees_commits_and_tags() {
    let fx = fixture();
    let c1 = fx.commit("main", &[write("docs/a.md", "a\n")], "1");
    let tag = fx
        .store
        .update(|tx| {
            let tag = fx.git.write_tag(
                tx,
                REPO,
                &Tag {
                    target: c1,
                    target_kind: ObjectKind::Commit,
                    name: "v1".to_owned(),
                    tagger: None,
                    message: String::new(),
                    gpg_signature: None,
                },
            )?;
            fx.git
                .update_ref(tx, REPO, ALICE, "refs/tags/v1", None, Some(tag), false)?;
            Ok(tag)
        })
        .unwrap();
    let tree = fx.commit_object(REPO, c1).tree;
    fx.store
        .view(|tx| {
            let eval = |e: &str| fx.git.expression(tx, REPO, e);
            assert!(matches!(&eval("main:docs/a.md")?.data, ObjectData::Blob(b) if &**b == b"a\n"));
            assert!(matches!(
                &eval(&format!("{c1}:docs/a.md"))?.data,
                ObjectData::Blob(_)
            ));
            assert_eq!(eval("HEAD:")?.id, tree);
            assert_eq!(eval("v1:")?.id, tree);
            assert!(matches!(eval("main:docs")?.data, ObjectData::Tree(_)));
            assert_eq!(eval("main")?.id, c1);
            assert_eq!(eval(&c1.to_string())?.id, c1);
            assert_eq!(eval("v1")?.id, tag);
            assert!(matches!(eval("v1")?.data, ObjectData::Tag(_)));
            for missing in ["main:docs/b.md", "main:docs/a.md/x", "nope:", "nope", ""] {
                assert_eq!(eval(missing), Err(GitError::NotFound), "{missing}");
            }
            Ok(())
        })
        .unwrap();
}

fn divergent(fx: &Fx) -> (ObjectId, ObjectId, ObjectId) {
    let base = fx.commit(
        "main",
        &[
            write("a.txt", "1\n"),
            write("b.txt", "1\n"),
            write("c.txt", "same\n"),
        ],
        "base",
    );
    fx.branch(REPO, "refs/heads/feature", base);
    let main = fx.commit("main", &[write("a.txt", "2\n")], "main change");
    fx.commit("feature", &[write("b.txt", "2\n")], "feature one");
    let feature = fx.commit(
        "feature",
        &[
            delete("c.txt"),
            write("e.txt", "same\n"),
            write("d.txt", "d\n"),
        ],
        "feature two",
    );
    (base, main, feature)
}

type FileRow<'a> = (
    &'a str,
    FileStatus,
    Option<&'a str>,
    u64,
    u64,
    Option<&'a str>,
);

#[test]
fn compare_counts_divergent_branches_from_their_merge_base() {
    let fx = fixture();
    let (base, main, feature) = divergent(&fx);

    let cmp = fx
        .store
        .view(|tx| Ok(fx.git.compare(tx, REPO, main, feature)?))
        .unwrap();

    assert_eq!(cmp.merge_base, Some(base));
    assert_eq!((cmp.ahead_by, cmp.behind_by), (2, 1));
    let messages: Vec<&str> = cmp
        .commits
        .iter()
        .map(|(_, c)| c.message.as_str())
        .collect();
    assert_eq!(messages, ["feature one", "feature two"]);
    assert_eq!(cmp.commits[1].0, feature);
    let files: Vec<FileRow<'_>> = cmp
        .files
        .iter()
        .map(|f| {
            (
                f.path.as_str(),
                f.status,
                f.previous_path.as_deref(),
                f.additions,
                f.deletions,
                f.patch.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        files,
        [
            (
                "b.txt",
                FileStatus::Modified,
                None,
                1,
                1,
                Some("@@ -1 +1 @@\n-1\n+2")
            ),
            (
                "d.txt",
                FileStatus::Added,
                None,
                1,
                0,
                Some("@@ -0,0 +1 @@\n+d")
            ),
            ("e.txt", FileStatus::Renamed, Some("c.txt"), 0, 0, None),
        ]
    );
}

#[test]
fn patches_match_git_diff_hunks_and_missing_newlines() {
    // Given the hunks `git diff -U3` prints for the same two versions
    let before: String = (1..=20).map(|n| n.to_string() + "\n").collect();
    let after = before
        .replace("\n2\n", "\ntwo\n")
        .replace("\n8\n", "\neight\n")
        .replace("\n19\n", "\nnineteen\n");
    let expected = "@@ -1,11 +1,11 @@\n 1\n-2\n+two\n 3\n 4\n 5\n 6\n 7\n-8\n+eight\n 9\n 10\n 11\n\
                    @@ -16,5 +16,5 @@\n 16\n 17\n 18\n-19\n+nineteen\n 20";

    // Then the patch text and counts are git's
    assert_eq!(
        crate::diff::text_patch(&before, &after),
        (Some(expected.to_owned()), 3, 3)
    );
    assert_eq!(
        crate::diff::text_patch("x\ny", "x\nz\n"),
        (
            Some("@@ -1,2 +1,2 @@\n x\n-y\n\\ No newline at end of file\n+z".to_owned()),
            1,
            1
        )
    );
}

#[test]
fn compare_gives_binary_files_no_patch() {
    let fx = fixture();
    let c1 = fx.commit("main", &[write("text.txt", "x\n")], "1");
    let c2 = fx.commit(
        "main",
        &[FileChange::Write {
            path: "bin.dat".to_owned(),
            data: b"\x00\x01\x02".as_slice().into(),
            mode: EntryMode::File,
        }],
        "2",
    );
    let cmp = fx
        .store
        .view(|tx| Ok(fx.git.compare(tx, REPO, c1, c2)?))
        .unwrap();
    assert_eq!((cmp.ahead_by, cmp.behind_by), (1, 0));
    assert_eq!(cmp.files.len(), 1);
    assert_eq!(cmp.files[0].status, FileStatus::Added);
    assert_eq!(cmp.files[0].patch, None);
}

#[test]
fn merge_joins_divergent_branches_with_a_signed_web_flow_commit() {
    let fx = fixture();
    let (_, main, feature) = divergent(&fx);

    let merged = fx
        .store
        .update(|tx| {
            Ok(fx
                .git
                .merge(tx, REPO, ALICE, "main", "feature", "Merge feature")?)
        })
        .unwrap();

    assert_eq!(fx.head(REPO, "refs/heads/main"), Ok(merged.commit));
    let commit = fx.commit_object(REPO, merged.commit);
    assert_eq!(commit.parents, [main, feature]);
    assert_eq!(commit.message, "Merge feature");
    assert_eq!(
        commit.author,
        Signature {
            name: "alice".to_owned(),
            email: "alice@example.com".to_owned(),
            time: MERGE_TIME,
            offset_minutes: 0,
        }
    );
    assert_eq!(commit.committer, Signature::web_flow(MERGE_TIME));
    assert_eq!(fx.git.verification(&commit), SignatureState::Valid);
    fx.store
        .view(|tx| {
            let text = |path: &str| match fx.git.expression(tx, REPO, &format!("main:{path}")) {
                Ok(o) => match o.data {
                    ObjectData::Blob(b) => Some(String::from_utf8_lossy(&b).into_owned()),
                    _ => None,
                },
                Err(_) => None,
            };
            assert_eq!(text("a.txt").as_deref(), Some("2\n"));
            assert_eq!(text("b.txt").as_deref(), Some("2\n"));
            assert_eq!(text("c.txt"), None);
            assert_eq!(text("d.txt").as_deref(), Some("d\n"));
            assert_eq!(text("e.txt").as_deref(), Some("same\n"));
            Ok(())
        })
        .unwrap();
    let last = fx.seen().pop().unwrap();
    assert_eq!(
        (last.sender, last.old, last.new, last.forced),
        (ALICE, Some(main), Some(merged.commit), false)
    );

    let again = fx.store.update(|tx| {
        Ok(fx
            .git
            .merge(tx, REPO, ALICE, "main", &feature.to_string(), "again")?)
    });
    assert_eq!(again.unwrap_err(), ApiError::from(GitError::NothingToMerge));
}

#[test]
fn merge_combines_new_files_in_the_same_directory() {
    let fx = fixture();
    let base = fx.commit("main", &[write("dir/keep", "k")], "base");
    fx.branch(REPO, "refs/heads/feature", base);
    fx.commit("main", &[write("dir/x", "x")], "x");
    fx.commit("feature", &[write("dir/y", "y")], "y");
    let bob = 3;

    let merged = fx
        .store
        .update(|tx| {
            tx.table_mut::<Accounts>()
                .insert(account(bob, "bob", AccountKind::User, None))?;
            Ok(fx.git.merge(tx, REPO, bob, "main", "feature", "m")?)
        })
        .unwrap();

    assert_eq!(
        fx.commit_object(REPO, merged.commit).author.email,
        "3+bob@users.noreply.github.com"
    );
    let names = fx
        .store
        .view(|tx| match fx.git.expression(tx, REPO, "main:dir")?.data {
            ObjectData::Tree(t) => Ok(t.iter().map(|e| e.name.clone()).collect::<Vec<_>>()),
            _ => Ok(Vec::new()),
        })
        .unwrap();
    assert_eq!(names, ["keep", "x", "y"]);
}

#[test]
fn merge_reports_a_conflict_and_changes_nothing() {
    let fx = fixture();
    let base = fx.commit(
        "main",
        &[write("a.txt", "1\n"), write("b.txt", "1\n")],
        "base",
    );
    fx.branch(REPO, "refs/heads/feature", base);
    fx.commit("main", &[write("a.txt", "main\n")], "main");
    let feature = fx.commit(
        "feature",
        &[write("a.txt", "feature\n"), delete("b.txt")],
        "feature",
    );
    let main = fx.commit("main", &[write("b.txt", "changed\n")], "main again");

    let result =
        fx.store.update(
            |tx| match fx.git.merge(tx, REPO, ALICE, "main", "feature", "m") {
                Err(GitError::Conflict { paths }) => Ok(paths),
                other => Err(ApiError::internal(format!("{other:?}"))),
            },
        );

    assert_eq!(result.unwrap(), ["a.txt", "b.txt"]);
    assert_eq!(fx.head(REPO, "main"), Ok(main));
    let mergeable = fx
        .store
        .view(|tx| {
            Ok((
                fx.git.mergeable(tx, REPO, main, feature)?,
                fx.git.mergeable(tx, REPO, main, base)?,
            ))
        })
        .unwrap();
    assert_eq!(mergeable, (false, true));
}

#[test]
fn log_follows_first_parents_or_committer_time() {
    let fx = fixture();
    let (base, main, feature) = divergent(&fx);
    let merged = fx
        .store
        .update(|tx| Ok(fx.git.merge(tx, REPO, ALICE, "main", "feature", "m")?))
        .unwrap()
        .commit;

    let (first, all) = fx
        .store
        .view(|tx| {
            let ids = |first_parent| -> Result<Vec<ObjectId>, GitError> {
                Ok(fx
                    .git
                    .log(tx, REPO, merged, first_parent, 100)?
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect())
            };
            Ok((ids(true)?, ids(false)?))
        })
        .unwrap();

    assert_eq!(first, [merged, main, base]);
    assert_eq!(all.len(), 5);
    assert_eq!(all[0], merged);
    assert_eq!(all.last(), Some(&base));
    assert!(all.contains(&feature));
}

#[test]
fn web_flow_signatures_verify_against_the_published_key() {
    let fx = fixture();
    let signed = fx
        .store
        .update(|tx| {
            Ok(fx.git.commit(
                tx,
                REPO,
                ALICE,
                "main",
                None,
                &[write("a", "1")],
                "signed",
                &alice(),
                &Signature::web_flow(MERGE_TIME),
                true,
            )?)
        })
        .unwrap();
    let commit = fx.commit_object(REPO, signed);

    let node = fx.git.signature(REPO, signed, &commit).unwrap();
    assert_eq!(node.state, SignatureState::Valid);
    assert!(node.was_signed_by_github);
    assert!(!node.payload.contains("gpgsig"));
    let (key, _) = SignedPublicKey::from_string(&fx.git.web_flow_public_key().unwrap()).unwrap();
    let (detached, _) = DetachedSignature::from_string(&node.signature).unwrap();
    assert!(detached.verify(&key, node.payload.as_bytes()).is_ok());

    let unsigned = Commit {
        gpg_signature: None,
        ..(*commit).clone()
    };
    assert_eq!(fx.git.verification(&unsigned), SignatureState::Unsigned);
    assert_eq!(fx.git.signature(REPO, signed, &unsigned), None);
    let tampered = Commit {
        message: "changed".to_owned(),
        ..(*commit).clone()
    };
    assert_eq!(fx.git.verification(&tampered), SignatureState::Invalid);
    let foreign = crate::sign::WebFlow::from_seed(7).unwrap();
    let payload = crate::encode::commit_bytes(&unsigned).unwrap();
    let by_other_key = Commit {
        gpg_signature: Some(foreign.sign(&payload, MERGE_TIME).unwrap()),
        ..unsigned.clone()
    };
    assert_eq!(
        fx.git.verification(&by_other_key),
        SignatureState::UnknownKey
    );
    let garbage = Commit {
        gpg_signature: Some("not a signature".to_owned()),
        ..unsigned
    };
    assert_eq!(fx.git.verification(&garbage), SignatureState::Invalid);
}

#[test]
fn a_fork_shares_objects_with_its_parent_and_keeps_its_own_refs() {
    let fx = fixture();
    let main = fx.commit("main", &[write("a", "1")], "1");
    fx.branch(FORK, "refs/heads/feat", main);

    let pushed = fx
        .commit_as(FORK, "feat", &[write("b", "2")], "fork only", &alice())
        .unwrap();

    fx.store
        .view(|tx| {
            assert!(fx.git.object(tx, REPO, pushed).is_ok());
            let cmp = fx.git.compare(tx, REPO, main, pushed)?;
            assert_eq!(
                (cmp.ahead_by, cmp.behind_by, cmp.merge_base),
                (1, 0, Some(main))
            );
            assert_eq!(cmp.files.len(), 1);
            assert_eq!(
                fx.git.resolve_ref(tx, REPO, "refs/heads/feat"),
                Err(GitError::NotFound)
            );
            assert_eq!(
                fx.git.resolve_ref(tx, FORK, "refs/heads/main"),
                Err(GitError::NotFound)
            );
            assert_eq!(fx.git.object(tx, OTHER, pushed), Err(GitError::NotFound));
            assert!(fx.git.expression(tx, REPO, &format!("{pushed}:b")).is_ok());
            Ok(())
        })
        .unwrap();
}

fn events(fx: &Fx, change: &RefChange) -> Vec<(&'static str, i64, Option<i64>, Value)> {
    fx.store
        .view(|tx| {
            let odb = fx.git.odb(tx, change.repo_id);
            Ok(crate::events::for_change(tx, &odb, change)?
                .into_iter()
                .map(|e| {
                    assert_eq!(e.repo_id, Some(change.repo_id));
                    assert_eq!((e.app_id, e.installation_id, e.action), (None, None, None));
                    (e.name, e.sender_id, e.org_id, Value::Object(e.payload))
                })
                .collect())
        })
        .unwrap()
}

fn commit_ids(payload: &Value) -> Vec<String> {
    payload["commits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| c["id"].as_str().unwrap_or("").to_owned())
        .collect()
}

#[test]
fn a_new_branch_sends_push_and_create() {
    let fx = fixture();
    let c1 = fx
        .commit_as(
            REPO,
            "main",
            &[write("a.txt", "1"), write("b.txt", "1")],
            "first\n",
            &alice(),
        )
        .unwrap();
    let change = fx.seen().pop().unwrap();

    let sent = events(&fx, &change);

    assert_eq!(sent.len(), 2);
    let (name, sender, org, push) = &sent[0];
    assert_eq!((*name, *sender, *org), ("push", ALICE, Some(ORG)));
    assert_eq!(push["ref"], "refs/heads/main");
    assert_eq!(push["before"], ObjectId::NULL.to_string());
    assert_eq!(push["after"], c1.to_string());
    assert_eq!(
        (
            push["created"].clone(),
            push["deleted"].clone(),
            push["forced"].clone()
        ),
        (json!(true), json!(false), json!(false))
    );
    assert_eq!(push["base_ref"], Value::Null);
    assert_eq!(push["compare"], "https://github.com/octo-org/r/compare/main");
    assert_eq!(
        push["pusher"],
        json!({"name": "alice", "email": "alice@example.com"})
    );
    assert_eq!(commit_ids(push), [c1.to_string()]);
    let head = &push["head_commit"];
    assert_eq!(head["id"], c1.to_string());
    assert_eq!(head["message"], "first\n");
    assert_eq!(head["timestamp"], "2026-01-01T00:00:00Z");
    assert_eq!(
        head["url"],
        format!("https://github.com/octo-org/r/commit/{c1}")
    );
    assert_eq!(
        head["author"],
        json!({"name": "Alice", "email": "alice@example.com", "username": "alice"})
    );
    assert_eq!(head["added"], json!(["a.txt", "b.txt"]));
    assert_eq!(
        (head["removed"].clone(), head["modified"].clone()),
        (json!([]), json!([]))
    );
    assert_eq!(head["distinct"], true);
    let (name, sender, _, create) = &sent[1];
    assert_eq!((*name, *sender), ("create", ALICE));
    assert_eq!(
        create,
        &json!({"ref": "main", "ref_type": "branch", "master_branch": "main", "description": "the repo", "pusher_type": "user"})
    );
}

#[test]
fn a_push_lists_only_the_new_commits_with_their_file_changes() {
    let fx = fixture();
    let c1 = fx.commit("main", &[write("a.txt", "1"), write("gone.txt", "x")], "1");
    let c2 = fx.commit("main", &[write("a.txt", "2"), write("new.txt", "n")], "2");
    let c3 = fx.commit("main", &[delete("gone.txt")], "3");
    let change = RefChange {
        repo_id: REPO,
        sender: ALICE,
        name: "refs/heads/main".to_owned(),
        old: Some(c1),
        new: Some(c3),
        forced: false,
    };

    let sent = events(&fx, &change);

    assert_eq!(sent.len(), 1);
    let (name, sender, _, push) = &sent[0];
    assert_eq!((*name, *sender), ("push", ALICE));
    assert_eq!(
        push["pusher"],
        json!({"name": "alice", "email": "alice@example.com"})
    );
    assert_eq!(
        push["head_commit"]["author"]["email"],
        "seed@hubdouble.invalid"
    );
    assert_eq!(commit_ids(push), [c2.to_string(), c3.to_string()]);
    assert_eq!(push["commits"][0]["added"], json!(["new.txt"]));
    assert_eq!(push["commits"][0]["modified"], json!(["a.txt"]));
    assert_eq!(push["commits"][1]["removed"], json!(["gone.txt"]));
    assert_eq!(
        push["compare"],
        format!(
            "https://github.com/octo-org/r/compare/{}...{}",
            &c1.to_string()[..12],
            &c3.to_string()[..12]
        )
    );
}

#[test]
fn a_forced_push_a_delete_and_a_tag_carry_their_flags() {
    let fx = fixture();
    let c1 = fx.commit("main", &[write("a", "1")], "1");
    let c2 = fx.commit("main", &[write("a", "2")], "2");
    fx.update_ref("refs/heads/main", Some(c2), Some(c1), true)
        .unwrap();
    let forced = fx.seen().pop().unwrap();
    fx.update_ref("refs/heads/main", Some(c1), None, false)
        .unwrap();
    let deleted = fx.seen().pop().unwrap();
    fx.update_ref("refs/heads/keep", None, Some(c2), false)
        .unwrap();
    fx.update_ref("refs/tags/v1", None, Some(c2), false)
        .unwrap();
    let tagged = fx.seen().pop().unwrap();
    fx.update_ref("refs/pull/1/head", None, Some(c2), false)
        .unwrap();
    let pull = fx.seen().pop().unwrap();

    let forced = &events(&fx, &forced)[0].3;
    assert_eq!(
        (forced["forced"].clone(), commit_ids(forced).len()),
        (json!(true), 0)
    );
    assert_eq!(forced["head_commit"]["id"], c1.to_string());

    let deleted = events(&fx, &deleted);
    assert_eq!(deleted.len(), 1);
    let deleted = &deleted[0].3;
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted["after"], ObjectId::NULL.to_string());
    assert_eq!(deleted["head_commit"], Value::Null);
    assert_eq!(deleted["commits"], json!([]));

    let tagged = events(&fx, &tagged);
    assert_eq!(tagged.len(), 2);
    assert_eq!(tagged[0].3["ref"], "refs/tags/v1");
    assert_eq!(tagged[0].3["commits"], json!([]));
    assert_eq!(tagged[1].3["ref_type"], "tag");
    assert_eq!(tagged[1].3["ref"], "v1");

    assert_eq!(events(&fx, &pull).len(), 0);
}

#[test]
fn a_commit_in_another_zone_has_its_local_timestamp() {
    let fx = fixture();
    let zoned = Signature {
        time: Timestamp::SEED,
        offset_minutes: -270,
        ..Signature::seed()
    };
    let c1 = fx
        .commit_as(REPO, "main", &[write("a", "1")], "1", &zoned)
        .unwrap();
    let change = fx.seen().pop().unwrap();
    assert_eq!(change.new, Some(c1));
    assert_eq!(
        events(&fx, &change)[0].3["head_commit"]["timestamp"],
        "2025-12-31T19:30:00-04:30"
    );
}

#[test]
fn write_tree_and_write_commit_refuse_missing_objects() {
    let fx = fixture();
    fx.store
        .view(|tx| {
            let missing = ObjectId([9; 20]);
            let entry = |mode| TreeEntry {
                name: "x".to_owned(),
                mode,
                id: missing,
            };
            assert_eq!(
                fx.git.write_tree(tx, REPO, &[entry(EntryMode::File)]),
                Err(GitError::NotFound)
            );
            assert!(
                fx.git
                    .write_tree(tx, REPO, &[entry(EntryMode::Submodule)])
                    .is_ok()
            );
            let blob = fx.git.write_blob(tx, REPO, b"x")?;
            let as_tree = TreeEntry {
                name: "x".to_owned(),
                mode: EntryMode::Tree,
                id: blob,
            };
            assert_eq!(
                fx.git.write_tree(tx, REPO, &[as_tree]),
                Err(GitError::WrongKind)
            );
            let tree = fx.git.write_tree(tx, REPO, &[])?;
            let commit = Commit {
                tree,
                parents: vec![missing],
                author: Signature::seed(),
                committer: Signature::seed(),
                message: String::new(),
                gpg_signature: None,
            };
            assert_eq!(
                fx.git.write_commit(tx, REPO, &commit),
                Err(GitError::NotFound)
            );
            Ok(())
        })
        .unwrap();
}
