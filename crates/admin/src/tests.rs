use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use hubdouble_git::{
    Commit, GitBuilder, GitError, Object, ObjectData, ObjectId, Ref, Tag, TreeEntry,
};
use hubdouble_model::{
    Access, AccountKind, ApiError, Clock, Config, ManualClock, OrgRole, RepositorySelection,
    Timestamp, Visibility,
};
use hubdouble_store::{
    Accounts, Apps, Counters, Installations, Memberships, Repos, Sequence, Store, StoreBuilder,
    Tokens, Tx, TxMut,
};
use serde_json::{Value, json};

use crate::history::Vcs;
use crate::kinds::apply_repository;
use crate::{Admin, Registrar, SeedCtx, Services};

#[derive(Default)]
struct FakeGit {
    refs: RefCell<HashMap<(i64, String), ObjectId>>,
    objects: RefCell<HashMap<ObjectId, ObjectData>>,
}

impl FakeGit {
    fn put(&self, data: ObjectData) -> ObjectId {
        let mut h = DefaultHasher::new();
        format!("{data:?}").hash(&mut h);
        let mut id = [0u8; 20];
        id[..8].copy_from_slice(&h.finish().to_be_bytes());
        let id = ObjectId(id);
        self.objects.borrow_mut().insert(id, data);
        id
    }

    fn commit_at(&self, repo: i64, name: &str) -> Arc<Commit> {
        let id = self.refs.borrow()[&(repo, name.to_owned())];
        match &self.objects.borrow()[&id] {
            ObjectData::Commit(c) => c.clone(),
            other => panic!("{name} is {other:?}"),
        }
    }

    fn files(&self, commit: &Commit) -> Vec<(String, String)> {
        let mut out = Vec::new();
        self.walk(commit.tree, "", &mut out);
        out
    }

    fn walk(&self, tree: ObjectId, prefix: &str, out: &mut Vec<(String, String)>) {
        let ObjectData::Tree(entries) = self.objects.borrow()[&tree].clone() else {
            panic!("not a tree")
        };
        for e in entries.iter() {
            let path = format!("{prefix}{}", e.name);
            match self.objects.borrow()[&e.id].clone() {
                ObjectData::Blob(b) => out.push((path, String::from_utf8_lossy(&b).into_owned())),
                ObjectData::Tree(_) => self.walk(e.id, &format!("{path}/"), out),
                _ => panic!("odd entry"),
            }
        }
    }
}

impl Vcs for FakeGit {
    fn resolve(&self, _: &Tx, repo: i64, name: &str) -> Result<ObjectId, GitError> {
        self.refs
            .borrow()
            .get(&(repo, name.to_owned()))
            .copied()
            .ok_or(GitError::NotFound)
    }

    fn refs(&self, _: &Tx, repo: i64, prefix: &str) -> Vec<Ref> {
        let mut refs: Vec<Ref> = self
            .refs
            .borrow()
            .iter()
            .filter(|((r, n), _)| *r == repo && n.starts_with(prefix))
            .map(|((_, n), id)| Ref {
                name: n.clone(),
                target: *id,
            })
            .collect();
        refs.sort_by(|a, b| a.name.cmp(&b.name));
        refs
    }

    fn update_ref(
        &self,
        _: &mut TxMut<'_>,
        repo: i64,
        name: &str,
        old: Option<ObjectId>,
        new: ObjectId,
    ) -> Result<(), GitError> {
        let mut refs = self.refs.borrow_mut();
        let key = (repo, name.to_owned());
        if refs.get(&key).copied() != old {
            return Err(GitError::Stale);
        }
        refs.insert(key, new);
        Ok(())
    }

    fn object(&self, _: &Tx, _: i64, id: ObjectId) -> Result<Object, GitError> {
        let data = self
            .objects
            .borrow()
            .get(&id)
            .cloned()
            .ok_or(GitError::NotFound)?;
        Ok(Object { id, data })
    }

    fn write_blob(&self, _: &Tx, _: i64, data: &[u8]) -> Result<ObjectId, GitError> {
        Ok(self.put(ObjectData::Blob(data.into())))
    }

    fn write_tree(&self, _: &Tx, _: i64, entries: &[TreeEntry]) -> Result<ObjectId, GitError> {
        Ok(self.put(ObjectData::Tree(entries.into())))
    }

    fn write_commit(&self, _: &Tx, _: i64, commit: &Commit) -> Result<ObjectId, GitError> {
        Ok(self.put(ObjectData::Commit(Arc::new(commit.clone()))))
    }

    fn write_tag(&self, _: &Tx, _: i64, tag: &Tag) -> Result<ObjectId, GitError> {
        Ok(self.put(ObjectData::Tag(Arc::new(tag.clone()))))
    }
}

struct World {
    store: Store,
    git: hubdouble_git::Git,
    clock: Arc<dyn Clock>,
    admin: Admin,
}

fn world() -> World {
    let mut store = StoreBuilder::new();
    let mut git = GitBuilder::new(&mut store);
    let mut rest = hubdouble_rest::Registry::new();
    let mut gql = hubdouble_gql::Registry::new();
    let mut admin = Admin::new();
    crate::register(&mut Registrar {
        store: &mut store,
        rest: &mut rest,
        gql: &mut gql,
        git: &mut git,
        admin: &mut admin,
    });
    World {
        store: store.build(),
        git: git.build(),
        clock: Arc::new(ManualClock::new(Timestamp::SEED)),
        admin,
    }
}

impl World {
    fn seed(&self, values: Vec<Value>) -> Result<(), ApiError> {
        self.seed_with(values, &|_| None)
    }

    fn seed_with(
        &self,
        values: Vec<Value>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), ApiError> {
        let entries = crate::seed::prepare(values, &self.admin.kinds, env, None)?;
        let ctx = crate::seed::Env {
            git: &self.git,
            clock: &self.clock,
            lookup: env,
        };
        self.store
            .update_quiet(|tx| crate::seed::apply(tx, entries, &ctx))
    }

    fn repo(&self, fake: &FakeGit, v: Value) -> Result<(), ApiError> {
        self.store.update_quiet(|tx| {
            apply_repository(
                &mut SeedCtx {
                    tx: tx.reborrow(),
                    git: &self.git,
                    clock: self.clock.as_ref(),
                    env: &|_| None,
                },
                fake,
                v,
            )
        })
    }
}

const KEY: &str = "-----BEGIN PUBLIC KEY-----\nMFkw\n-----END PUBLIC KEY-----\n";

fn base() -> Vec<Value> {
    vec![
        json!({"kind": "installation", "id": 30, "app": "ci", "account": "acme", "repositories": ["web"],
               "permissions": {"contents": "read"}}),
        json!({"kind": "app", "id": 1_000_001, "slug": "ci", "owner": "acme", "public_key": KEY,
               "permissions": {"contents": "write", "metadata": "read"}, "events": ["push"]}),
        json!({"kind": "repository", "owner": "acme", "name": "web", "private": true}),
        json!({"kind": "org", "id": 962_416, "login": "acme", "sso_enforced": true,
               "members": [{"login": "alice", "role": "admin"}, {"login": "bob"}]}),
        json!({"kind": "user", "id": 10, "login": "alice", "tokens": [{"token": "ghp_alice"}]}),
        json!({"kind": "user", "login": "bob"}),
        json!({"kind": "token", "user": "bob", "scopes": ["repo"]}),
    ]
}

#[test]
fn the_seed_kinds_build_accounts_orgs_repos_apps_installations_and_tokens() {
    // Given
    let w = world();
    // When
    w.seed(base()).unwrap();
    // Then
    w.store
        .view(|tx| {
            let accounts = tx.table::<Accounts>();
            let alice = accounts.by_login("alice").unwrap();
            assert_eq!((alice.id, alice.kind), (10, AccountKind::User));
            let bob = accounts.by_login("bob").unwrap();
            assert!(
                bob.id > 1_000_001,
                "generated IDs start above the largest given ID"
            );
            let acme = accounts.by_login("acme").unwrap();
            assert_eq!(
                (acme.id, acme.kind, acme.sso_enforced),
                (962_416, AccountKind::Organization, true)
            );
            let roles: Vec<_> = tx
                .table::<Memberships>()
                .children(acme.id)
                .map(|m| (m.user_id, m.role))
                .collect();
            assert_eq!(roles, [(10, OrgRole::Admin), (bob.id, OrgRole::Member)]);
            let web = tx.table::<Repos>().by_name(acme.id, "web").unwrap();
            assert_eq!(web.visibility, Visibility::Private);
            assert_eq!(web.default_branch, "main");
            let app = tx.table::<Apps>().get(1_000_001).unwrap();
            let bot = accounts.get(app.bot_id).unwrap();
            assert_eq!(
                (bot.login.as_str(), bot.kind, bot.app_id),
                ("ci[bot]", AccountKind::Bot, Some(app.id))
            );
            let inst = tx.table::<Installations>().get(30).unwrap();
            assert_eq!(inst.repository_selection, RepositorySelection::Selected);
            assert_eq!(inst.repo_ids, [web.id]);
            assert_eq!(inst.permissions.get("contents"), Some(&Access::Read));
            assert_eq!(inst.events, ["push"]);
            let tokens = tx.table::<Tokens>();
            let alice_token = tokens.by_value("ghp_alice").unwrap();
            assert_eq!(alice_token.scopes, ["repo", "workflow", "read:org", "user"]);
            assert!(alice_token.owner_seeded);
            let bob_token = tokens.children(bob.id).next().unwrap();
            assert_eq!(bob_token.scopes, ["repo"]);
            assert!(bob_token.token.starts_with("ghp_"));
            assert_eq!(bob_token.token.len(), 40);
            assert!(bob_token.token[4..].bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(tx.table::<Counters>().is_seeded(bob.id));
            Ok(())
        })
        .unwrap();
}

#[test]
fn no_account_takes_the_id_or_login_of_the_ghost_user() {
    for taken in [
        json!({"kind": "user", "login": "ghost"}),
        json!({"kind": "user", "login": "Ghost"}),
        json!({"kind": "user", "id": 10137, "login": "casper"}),
        json!({"kind": "org", "id": 10137, "login": "spooks"}),
    ] {
        // Given
        let w = world();
        // When
        let e = w.seed(vec![taken.clone()]).unwrap_err();
        // Then
        assert_eq!(e.status, 422, "{taken}");
    }

    // Given
    let w = world();
    // When
    w.seed(vec![
        json!({"kind": "user", "id": 10136, "login": "alice"}),
        json!({"kind": "user", "login": "bob"}),
    ])
    .unwrap();
    // Then
    w.store
        .view(|tx| {
            assert_eq!(tx.table::<Accounts>().by_login("bob").unwrap().id, 10138);
            Ok(())
        })
        .unwrap();
}

#[test]
fn an_app_takes_only_a_positive_id() {
    let org = json!({"kind": "org", "id": 962_416, "login": "acme"});
    for id in [0, -1] {
        // Given
        let w = world();
        // When
        let e = w
            .seed(vec![
                org.clone(),
                json!({"kind": "app", "id": id, "slug": "ci", "owner": "acme", "public_key": KEY}),
            ])
            .unwrap_err();
        // Then
        assert_eq!(e.status, 422, "{id}");
    }

    // Given
    let w = world();
    // When
    w.seed(vec![
        org,
        json!({"kind": "app", "id": 1, "slug": "ci", "owner": "acme", "public_key": KEY}),
    ])
    .unwrap();
    // Then
    w.store
        .view(|tx| {
            assert!(tx.table::<Apps>().get(1).is_some());
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_failing_entry_changes_nothing_and_names_its_position() {
    // Given
    let w = world();
    let mut values = base();
    values.push(
        json!({"kind": "installation", "app": "ci", "account": "acme",
                       "permissions": {"administration": "write"}}),
    );
    // When
    let e = w.seed(values).unwrap_err();
    // Then
    assert_eq!(e.status, 422);
    assert!(
        e.message.starts_with("seed entry 8 (installation)"),
        "{}",
        e.message
    );
    w.store
        .view(|tx| {
            assert!(tx.table::<Accounts>().is_empty());
            assert_eq!(tx.table::<Counters>().current(Sequence::Id), 0);
            Ok(())
        })
        .unwrap();
}

#[test]
fn env_templates_fill_any_string_and_an_unset_variable_fails() {
    let w = world();
    let env = |n: &str| (n == "LOGIN").then(|| "carol".to_owned());
    w.seed_with(
        vec![json!({"kind": "user", "login": "{{env \"LOGIN\"}}"})],
        &env,
    )
    .unwrap();
    assert!(
        w.store
            .view(|tx| Ok(tx.table::<Accounts>().by_login("carol").is_some()))
            .unwrap()
    );
    let e = world()
        .seed(vec![json!({"kind": "user", "login": "{{env \"LOGIN\"}}"})])
        .unwrap_err();
    assert!(e.message.starts_with("seed entry 1 (user)"));
}

#[test]
fn a_repository_history_writes_files_branches_tags_and_ordered_commits() {
    // Given
    let w = world();
    w.seed(vec![json!({"kind": "user", "login": "dev"})])
        .unwrap();
    let fake = FakeGit::default();
    w.repo(&fake, json!({
        "owner": "dev", "name": "runtime",
        "files": {"b.txt": "b", "a/x.txt": "x"},
        "commits": [
            {"files": {"a/y.txt": "y"}, "message": "add y", "tags": [{"name": "v1.0.0", "message": "v1"}]},
            {"branch": "feature", "files": {"b.txt": null}, "message": "drop b"},
            {"files": {"c.txt": "c"}, "message": "after the tag",
             "author": {"name": "Dev", "email": "dev@example.com"}},
        ],
    }))
    .unwrap();
    // When
    w.repo(
        &fake,
        json!({
            "owner": "dev", "name": "caller",
            "files": {"pin.yml": "uses: dev/runtime@{{sha \"dev/runtime\" \"v1.0.0\"}} # v1.0.0"},
        }),
    )
    .unwrap();
    // Then
    let runtime = w
        .store
        .view(|tx| Ok(tx.table::<Repos>().iter().next().unwrap().clone()))
        .unwrap();
    let main = fake.commit_at(runtime.id, "refs/heads/main");
    assert_eq!(main.message, "after the tag");
    assert_eq!(main.author.email, "dev@example.com");
    assert_eq!(main.author.time, Timestamp::SEED);
    let tagged = fake.refs.borrow()[&(runtime.id, "refs/tags/v1.0.0".to_owned())];
    let ObjectData::Tag(tag) = fake.objects.borrow()[&tagged].clone() else {
        panic!("v1.0.0 is not annotated")
    };
    assert_eq!(main.parents, [tag.target]);
    let ObjectData::Commit(at_tag) = fake.objects.borrow()[&tag.target].clone() else {
        panic!("the tag does not name a commit")
    };
    assert_eq!(at_tag.message, "add y");
    assert_eq!(
        fake.files(&at_tag),
        [("a/x.txt", "x"), ("a/y.txt", "y"), ("b.txt", "b")]
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
    );
    let first = &at_tag.parents;
    assert_eq!(first.len(), 1);
    let feature = fake.commit_at(runtime.id, "refs/heads/feature");
    assert_eq!(feature.parents, [tag.target]);
    assert_eq!(fake.files(&feature).len(), 2);
    let caller = w
        .store
        .view(|tx| Ok(tx.table::<Repos>().iter().last().unwrap().clone()))
        .unwrap();
    let pin = fake.files(&fake.commit_at(caller.id, "refs/heads/main"));
    assert_eq!(
        pin[0].1,
        format!("uses: dev/runtime@{} # v1.0.0", tag.target)
    );
    assert!(caller.pushed_at.is_some());
}

#[test]
fn a_sha_template_needs_a_ref_from_an_earlier_entry() {
    let w = world();
    w.seed(vec![json!({"kind": "user", "login": "dev"})])
        .unwrap();
    let fake = FakeGit::default();
    let e = w
        .repo(
            &fake,
            json!({"owner": "dev", "name": "c", "files": {"f": "{{sha \"dev/later\" \"v1\"}}"}}),
        )
        .unwrap_err();
    assert_eq!(e.status, 422);
    assert!(
        w.store
            .view(|tx| Ok(tx.table::<Repos>().is_empty()))
            .unwrap()
    );
}

#[test]
fn a_fork_starts_with_its_parents_refs() {
    let w = world();
    w.seed(vec![json!({"kind": "user", "login": "dev"})])
        .unwrap();
    let fake = FakeGit::default();
    w.repo(
        &fake,
        json!({"owner": "dev", "name": "up", "default_branch": "trunk", "files": {"f": "1"},
                         "tags": [{"name": "light"}]}),
    )
    .unwrap();
    w.repo(
        &fake,
        json!({"owner": "dev", "name": "fork", "fork_of": "dev/up"}),
    )
    .unwrap();
    let (up, fork) = w
        .store
        .view(|tx| {
            let repos = tx.table::<Repos>();
            let mut it = repos.iter();
            Ok((it.next().unwrap().clone(), it.next().unwrap().clone()))
        })
        .unwrap();
    assert_eq!(fork.fork_of, Some(up.id));
    assert_eq!(fork.default_branch, "trunk");
    let refs = fake.refs.borrow();
    for name in ["refs/heads/trunk", "refs/tags/light"] {
        assert_eq!(
            refs[&(fork.id, name.to_owned())],
            refs[&(up.id, name.to_owned())],
            "{name}"
        );
    }
}

fn services(w: World, seed: Option<std::path::PathBuf>) -> (Admin, Services) {
    let config = Arc::new(Config {
        seed,
        ..Config::default()
    });
    let hooks = hubdouble_hooks::Handle::start(w.store.clone(), w.clock.clone()).unwrap();
    (
        w.admin,
        Services {
            store: w.store,
            git: w.git,
            hooks,
            clock: w.clock,
            config,
        },
    )
}

#[tokio::test]
async fn reset_restores_the_seed_but_sequences_and_seeded_tokens_carry_on() {
    // Given
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("seed.yaml");
    std::fs::write(
        &path,
        "- kind: user\n  login: alice\n  tokens: [{token: ghp_seeded}]\n- kind: org\n  login: acme\n",
    )
    .unwrap();
    let (admin, s) = services(world(), Some(path));
    admin.load_seed(&s, &|_| None).unwrap();
    let (alice, before) = s
        .store
        .view(|tx| {
            Ok((
                tx.table::<Accounts>().by_login("alice").unwrap().id,
                tx.table::<Counters>().current(Sequence::Id),
            ))
        })
        .unwrap();
    s.store
        .update(|tx| {
            let mut ctx = SeedCtx {
                tx: tx.reborrow(),
                git: &s.git,
                clock: s.clock.as_ref(),
                env: &|_| None,
            };
            crate::kinds::user_kind(&mut ctx, json!({"login": "later"}))?;
            crate::kinds::token_kind(&mut ctx, json!({"user": "alice", "token": "ghp_minted"}))?;
            crate::kinds::token_kind(&mut ctx, json!({"user": "later", "token": "ghp_gone"}))
        })
        .unwrap();
    // When
    admin.reset_state(&s).await.unwrap();
    // Then
    s.store
        .view(|tx| {
            let accounts = tx.table::<Accounts>();
            assert_eq!(accounts.by_login("alice").unwrap().id, alice);
            assert!(accounts.by_login("acme").is_some());
            assert!(accounts.by_login("later").is_none());
            let tokens = tx.table::<Tokens>();
            assert!(tokens.by_value("ghp_seeded").is_some());
            assert!(
                tokens.by_value("ghp_minted").is_some(),
                "a token of a seeded user survives"
            );
            assert!(tokens.by_value("ghp_gone").is_none());
            assert!(tx.table::<Counters>().current(Sequence::Id) > before + 2);
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn a_missing_seed_file_fails_to_load() {
    let dir = tempfile::TempDir::new().unwrap();
    let (admin, s) = services(world(), Some(dir.path().join("absent.yaml")));
    assert_eq!(admin.load_seed(&s, &|_| None).unwrap_err().status, 422);
}
