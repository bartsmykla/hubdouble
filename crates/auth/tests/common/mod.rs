use std::collections::BTreeMap;
use std::sync::Arc;

use http::{HeaderMap, HeaderValue};
use hubdouble_model::{
    Access, Account, AccountKind, App, Collaborator, Config, Installation, Membership,
    MembershipState, OrgRole, Repo, RepoRole, RepositorySelection, Team, TeamMember, TeamPrivacy,
    TeamRepo, TeamRole, Timestamp, Token, TokenKind, Visibility,
};
use hubdouble_store::{
    Accounts, Apps, Installations, Memberships, Repos, Store, StoreBuilder, Teams, Tokens,
};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};

pub const NOW: Timestamp = Timestamp(Timestamp::SEED.0 + 86_400);

pub const APP_KEY: &str = include_str!("../../testdata/app.key.pem");
pub const APP_PUB: &str = include_str!("../../testdata/app.pub.pem");
pub const OTHER_KEY: &str = include_str!("../../testdata/other.key.pem");

pub const ACME: i64 = 10;
pub const SSO_ORG: i64 = 11;
pub const ALICE: i64 = 20;
pub const BOB: i64 = 21;
pub const CAROL: i64 = 22;
pub const DAVE: i64 = 23;
pub const BOT: i64 = 30;
pub const PUBLIC: i64 = 100;
pub const PRIVATE: i64 = 101;
pub const UNINSTALLED: i64 = 102;
pub const SSO_PRIVATE: i64 = 103;
pub const INTERNAL: i64 = 104;
pub const APP: i64 = 40;
pub const CLIENT_ID: &str = "Iv23liAbCdEf";
pub const INSTALLATION: i64 = 50;
pub const SSO_INSTALLATION: i64 = 51;
pub const PARENT_TEAM: i64 = 60;
pub const CHILD_TEAM: i64 = 61;

pub fn perms(list: &[(&str, Access)]) -> BTreeMap<String, Access> {
    list.iter().map(|(p, a)| ((*p).to_owned(), *a)).collect()
}

fn account(id: i64, login: &str, kind: AccountKind) -> Account {
    Account {
        id,
        node_id: format!("U_{id}"),
        login: login.to_owned(),
        kind,
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

fn repo(id: i64, owner_id: i64, name: &str, visibility: Visibility) -> Repo {
    Repo {
        id,
        node_id: format!("R_{id}"),
        owner_id,
        name: name.to_owned(),
        description: None,
        homepage: None,
        language: None,
        topics: Vec::new(),
        visibility,
        fork_of: None,
        default_branch: "main".to_owned(),
        archived: false,
        disabled: false,
        is_template: false,
        has_issues: true,
        has_projects: false,
        has_wiki: false,
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

fn installation(
    id: i64,
    account_id: i64,
    repo_ids: Vec<i64>,
    permissions: BTreeMap<String, Access>,
) -> Installation {
    Installation {
        id,
        node_id: format!("I_{id}"),
        app_id: APP,
        account_id,
        repository_selection: RepositorySelection::Selected,
        repo_ids,
        permissions,
        events: Vec::new(),
        suspended_at: None,
        suspended_by: None,
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
    }
}

pub fn token(id: i64, value: &str, kind: TokenKind) -> Token {
    Token {
        id,
        token: value.to_owned(),
        kind,
        app_id: None,
        installation_id: None,
        user_id: None,
        repo_ids: None,
        permissions: BTreeMap::new(),
        scopes: Vec::new(),
        expires_at: None,
        revoked: false,
        owner_seeded: true,
        created_at: Timestamp::SEED,
    }
}

pub fn installation_token(id: i64, value: &str, permissions: &[(&str, Access)]) -> Token {
    Token {
        installation_id: Some(INSTALLATION),
        app_id: Some(APP),
        permissions: perms(permissions),
        expires_at: Some(NOW.plus_seconds(3600)),
        ..token(id, value, TokenKind::Installation)
    }
}

pub fn personal(id: i64, value: &str, user: i64, scopes: &[&str]) -> Token {
    Token {
        user_id: Some(user),
        scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        ..token(id, value, TokenKind::Personal)
    }
}

pub fn user_token(id: i64, value: &str, user: i64) -> Token {
    Token {
        user_id: Some(user),
        app_id: Some(APP),
        expires_at: Some(NOW.plus_seconds(8 * 3600)),
        ..token(id, value, TokenKind::User)
    }
}

/// Two orgs (`sso-org` enforces SSO), four users, the App's bot, five repositories, one App with
/// two installations, two nested teams, and every token the tests use.
#[expect(
    clippy::too_many_lines,
    reason = "the fixture reads best as one list of rows"
)]
pub fn store() -> Store {
    let mut b = StoreBuilder::new();
    hubdouble_auth::register_tables(&mut b);
    let store = b.build_with(Arc::new(Config {
        base_url: "http://hubdouble:8000".to_owned(),
        ..Config::default()
    }));
    store
        .update(|tx| {
            let mut sso = account(SSO_ORG, "sso-org", AccountKind::Organization);
            sso.sso_enforced = true;
            let mut bot = account(BOT, "hubdouble-test[bot]", AccountKind::Bot);
            bot.app_id = Some(APP);
            let mut accounts = tx.table_mut::<Accounts>();
            for a in [
                account(ACME, "acme", AccountKind::Organization),
                sso,
                account(ALICE, "alice", AccountKind::User),
                account(BOB, "bob", AccountKind::User),
                account(CAROL, "carol", AccountKind::User),
                account(DAVE, "dave", AccountKind::User),
                bot,
            ] {
                accounts.insert(a)?;
            }
            let mut members = tx.table_mut::<Memberships>();
            for (id, org, user, role) in [
                (70, ACME, CAROL, OrgRole::Member),
                (71, ACME, DAVE, OrgRole::Admin),
                (72, ACME, BOB, OrgRole::Member),
                (73, SSO_ORG, ALICE, OrgRole::Member),
            ] {
                members.insert(Membership {
                    id,
                    org_id: org,
                    user_id: user,
                    role,
                    state: MembershipState::Active,
                })?;
            }
            let mut private = repo(PRIVATE, ACME, "private", Visibility::Private);
            private.collaborators.push(Collaborator {
                user_id: ALICE,
                role: RepoRole::Write,
            });
            private.collaborators.push(Collaborator {
                user_id: BOB,
                role: RepoRole::Read,
            });
            let mut sso_private = repo(SSO_PRIVATE, SSO_ORG, "private", Visibility::Private);
            sso_private.collaborators.push(Collaborator {
                user_id: ALICE,
                role: RepoRole::Write,
            });
            let mut repos = tx.table_mut::<Repos>();
            for r in [
                repo(PUBLIC, ACME, "public", Visibility::Public),
                private,
                repo(UNINSTALLED, ACME, "uninstalled", Visibility::Private),
                sso_private,
                repo(INTERNAL, ACME, "internal", Visibility::Internal),
            ] {
                repos.insert(r)?;
            }
            let team = |id: i64, slug: &str, parent_id: Option<i64>| Team {
                id,
                node_id: format!("T_{id}"),
                org_id: ACME,
                name: slug.to_owned(),
                slug: slug.to_owned(),
                description: None,
                privacy: TeamPrivacy::Closed,
                notifications_enabled: true,
                parent_id,
                members: Vec::new(),
                repos: Vec::new(),
                created_at: Timestamp::SEED,
                updated_at: Timestamp::SEED,
            };
            let mut parent = team(PARENT_TEAM, "parent", None);
            parent.repos.push(TeamRepo {
                repo_id: PRIVATE,
                role: RepoRole::Maintain,
            });
            let mut child = team(CHILD_TEAM, "child", Some(PARENT_TEAM));
            child.members.push(TeamMember {
                user_id: CAROL,
                role: TeamRole::Member,
            });
            tx.table_mut::<Teams>().insert(parent)?;
            tx.table_mut::<Teams>().insert(child)?;
            tx.table_mut::<Apps>().insert(App {
                id: APP,
                node_id: "A_40".to_owned(),
                slug: "hubdouble-test".to_owned(),
                name: "hubdouble test".to_owned(),
                owner_id: ACME,
                bot_id: BOT,
                description: None,
                external_url: "https://example.test".to_owned(),
                client_id: CLIENT_ID.to_owned(),
                client_secret: "secret".to_owned(),
                public_key_pem: APP_PUB.to_owned(),
                private_key_pem: None,
                webhook: None,
                permissions: BTreeMap::new(),
                events: Vec::new(),
                device_flow: true,
                created_at: Timestamp::SEED,
                updated_at: Timestamp::SEED,
            })?;
            let granted = perms(&[
                ("issues", Access::Write),
                ("pull_requests", Access::Write),
                ("contents", Access::Write),
                ("members", Access::Read),
            ]);
            tx.table_mut::<Installations>().insert(installation(
                INSTALLATION,
                ACME,
                vec![PUBLIC, PRIVATE, INTERNAL],
                granted.clone(),
            ))?;
            tx.table_mut::<Installations>().insert(installation(
                SSO_INSTALLATION,
                SSO_ORG,
                vec![SSO_PRIVATE],
                granted,
            ))?;
            let mut tokens = tx.table_mut::<Tokens>();
            let expired = Token {
                expires_at: Some(NOW),
                ..installation_token(206, "ghs_expired", &[])
            };
            let revoked = Token {
                revoked: true,
                ..installation_token(207, "ghs_revoked", &[])
            };
            let narrowed = Token {
                repo_ids: Some(vec![PUBLIC]),
                ..installation_token(208, "ghs_narrowed", &[])
            };
            let refresh = Token {
                user_id: Some(ALICE),
                ..token(209, "ghr_alice", TokenKind::Refresh)
            };
            for t in [
                installation_token(200, "ghs_full", &[]),
                installation_token(201, "ghs_issues", &[("issues", Access::Write)]),
                installation_token(202, "ghs_pulls", &[("pull_requests", Access::Write)]),
                installation_token(203, "ghs_contents", &[("contents", Access::Read)]),
                expired,
                revoked,
                narrowed,
                refresh,
                personal(
                    210,
                    "ghp_alice",
                    ALICE,
                    &["repo", "workflow", "read:org", "user"],
                ),
                personal(211, "ghp_alice_public", ALICE, &["public_repo"]),
                personal(212, "ghp_bob", BOB, &["repo"]),
                personal(213, "ghp_carol", CAROL, &["repo"]),
                personal(214, "ghp_dave", DAVE, &["repo", "admin:org"]),
                user_token(215, "ghu_alice", ALICE),
                user_token(216, "ghu_bob", BOB),
            ] {
                tokens.insert(t)?;
            }
            Ok(())
        })
        .unwrap_or_else(|e| panic!("the fixture does not load: {e}"));
    store
}

pub fn bearer(value: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {value}")).unwrap_or_else(|e| panic!("{e}")),
    );
    h
}

pub fn jwt(key_pem: &str, claims: &Value) -> String {
    let key = EncodingKey::from_rsa_pem(key_pem.as_bytes()).unwrap_or_else(|e| panic!("{e}"));
    jsonwebtoken::encode(&Header::new(Algorithm::RS256), claims, &key)
        .unwrap_or_else(|e| panic!("{e}"))
}

pub fn app_jwt() -> String {
    jwt(
        APP_KEY,
        &json!({ "iss": APP, "iat": NOW.unix() - 60, "exp": NOW.unix() + 540 }),
    )
}
