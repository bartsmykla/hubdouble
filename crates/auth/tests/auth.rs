mod common;

use common::*;
use http::HeaderMap;
use hubdouble_auth::{
    Actor, AuthError, NarrowRequest, Target, accepts_token, authenticate, authenticate_admin,
    authorize, can_see, narrow, record_sso, repo_role, verify_app_jwt,
};
use hubdouble_model::{
    Access, ApiError, Grant, ManualClock, RepoRole, Requirement, Scope, TeamRepo, Timestamp,
};
use hubdouble_store::{Accounts, Installations, Repos, Store, Teams, Tx};
use serde_json::json;

const READ_CONTENTS: Requirement = Requirement {
    any_of: &[Grant::read("contents")],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &[],
};
const WRITE_CONTENTS: Requirement = Requirement {
    any_of: &[Grant::write("contents")],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &["repo"],
};
const CREATE_COMMENT: Requirement = Requirement {
    any_of: &[Grant::write("issues"), Grant::write("pull_requests")],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &["repo", "public_repo"],
};
const ADMIN_REPO: Requirement = Requirement {
    any_of: &[Grant::write("administration")],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &["repo"],
};
const ORG_REPOS: Requirement = Requirement {
    any_of: &[Grant::read("metadata")],
    all_of: &[],
    scope: Scope::Org,
    scopes: &[],
};
const ORG_MEMBERS_WRITE: Requirement = Requirement {
    any_of: &[Grant::write("members")],
    all_of: &[],
    scope: Scope::Org,
    scopes: &["admin:org"],
};
const APP_ONLY: Requirement = Requirement {
    any_of: &[],
    all_of: &[],
    scope: Scope::App,
    scopes: &[],
};
const INSTALLATION_ONLY: Requirement = Requirement {
    any_of: &[],
    all_of: &[],
    scope: Scope::Installation,
    scopes: &[],
};
const READ_HOOKS: Requirement = Requirement {
    any_of: &[Grant::read("repository_hooks")],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &["read:repo_hook", "repo"],
};
const WRITE_HOOKS: Requirement = Requirement {
    any_of: &[Grant::write("repository_hooks")],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &["write:repo_hook", "repo"],
};
const DELETE_HOOKS: Requirement = Requirement {
    any_of: &[Grant::write("repository_hooks")],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &["admin:repo_hook", "repo"],
};
const ORG_EVENTS: Requirement = Requirement {
    any_of: &[Grant::read("organization_events")],
    all_of: &[],
    scope: Scope::Org,
    scopes: &[],
};
const USER_EMAILS: Requirement = Requirement {
    any_of: &[Grant::read("email_addresses")],
    all_of: &[],
    scope: Scope::User,
    scopes: &["user", "user:email"],
};

fn actor(store: &Store, credential: &str) -> Actor {
    let clock = ManualClock::new(NOW);
    store
        .view(|tx| authenticate(tx, &clock, &bearer(credential)))
        .unwrap_or_else(|e| panic!("{credential} does not authenticate: {e}"))
}

fn status_of(store: &Store, credential: &str) -> u16 {
    let clock = ManualClock::new(NOW);
    match store.view(|tx| authenticate(tx, &clock, &bearer(credential))) {
        Ok(_) => 200,
        Err(e) => e.status,
    }
}

fn on_repo(store: &Store, actor: &Actor, req: &Requirement, repo: i64) -> Result<(), ApiError> {
    store.view(|tx| {
        let repo = tx
            .table::<Repos>()
            .get(repo)
            .cloned()
            .ok_or_else(ApiError::not_found)?;
        authorize(tx, actor, req, Target::Repo(&repo))
    })
}

fn on_org(store: &Store, actor: &Actor, req: &Requirement, org: i64) -> Result<(), ApiError> {
    store.view(|tx| {
        let org = tx
            .table::<Accounts>()
            .get(org)
            .cloned()
            .ok_or_else(ApiError::not_found)?;
        authorize(tx, actor, req, Target::Org(&org))
    })
}

fn status(r: Result<(), ApiError>) -> u16 {
    r.map_or_else(|e| e.status, |()| 200)
}

fn header<'e>(e: &'e ApiError, name: &str) -> Option<&'e str> {
    e.headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn jwt_result(claims: &serde_json::Value, key: &str) -> Result<(), AuthError> {
    let store = store();
    let token = jwt(key, claims);
    store
        .view(|tx| Ok(verify_app_jwt(tx, NOW, &token).map(|_| ())))
        .unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn no_authorization_header_is_anonymous_and_public_data_passes() {
    // Given
    let store = store();
    let clock = ManualClock::new(NOW);

    // When
    let actor = store
        .view(|tx| authenticate(tx, &clock, &HeaderMap::new()))
        .expect("anonymous");

    // Then
    assert_eq!(actor, Actor::Anonymous);
    assert_eq!(
        status(on_repo(&store, &actor, &Requirement::public(), PRIVATE)),
        200
    );
    assert_eq!(status(on_repo(&store, &actor, &READ_CONTENTS, PUBLIC)), 200);
    assert_eq!(
        status(on_repo(&store, &actor, &READ_CONTENTS, PRIVATE)),
        404
    );
    assert_eq!(
        status(on_repo(&store, &actor, &CREATE_COMMENT, PUBLIC)),
        401
    );
}

#[test]
fn each_token_kind_resolves_to_its_actor() {
    let store = store();

    let Actor::Installation {
        installation, bot, ..
    } = actor(&store, "ghs_full")
    else {
        panic!("not an installation actor");
    };
    assert_eq!(installation.id, INSTALLATION);
    assert_eq!(bot.id, BOT);

    let Actor::User { user, token } = actor(&store, "ghp_alice") else {
        panic!("not a user actor");
    };
    assert_eq!(user.id, ALICE);
    assert_eq!(
        actor(&store, "ghp_alice").oauth_scopes(),
        Some(&token.scopes[..])
    );

    let Actor::User { user, .. } = actor(&store, "ghu_bob") else {
        panic!("not a user actor");
    };
    assert_eq!(user.id, BOB);

    let Actor::App { app } = actor(&store, &app_jwt()) else {
        panic!("not an App actor");
    };
    assert_eq!(app.id, APP);
}

#[test]
fn the_token_scheme_works_like_bearer() {
    let store = store();
    let clock = ManualClock::new(NOW);
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        "token ghp_alice".parse().expect("header"),
    );

    let actor = store.view(|tx| authenticate(tx, &clock, &headers));

    assert!(matches!(actor, Ok(Actor::User { .. })));
}

#[test]
fn unknown_expired_revoked_and_refresh_tokens_are_401() {
    let store = store();

    for credential in [
        "ghp_unknown",
        "ghs_expired",
        "ghs_revoked",
        "ghr_alice",
        "hubdouble-admin",
    ] {
        assert_eq!(status_of(&store, credential), 401, "{credential}");
    }
}

#[test]
fn an_installation_token_expires_with_the_clock() {
    // Given a token that expires an hour after NOW
    let store = store();
    let clock = ManualClock::new(NOW);
    let headers = bearer("ghs_full");
    let at = |t: Timestamp| {
        clock.set(t);
        store
            .view(|tx| authenticate(tx, &clock, &headers))
            .map_or_else(|e| e.status, |_| 200)
    };

    // Then
    assert_eq!(at(NOW.plus_seconds(3599)), 200);
    assert_eq!(at(NOW.plus_seconds(3600)), 401);
}

#[test]
fn a_suspended_installation_token_is_refused() {
    let store = store();
    store
        .update(|tx| {
            let mut i = (**tx.table::<Installations>().get(INSTALLATION).expect("i")).clone();
            i.suspended_at = Some(NOW);
            tx.table_mut::<Installations>().replace(i)?;
            Ok(())
        })
        .expect("suspend");

    assert_eq!(status_of(&store, "ghs_full"), 403);
}

#[test]
fn an_app_jwt_names_the_app_by_id_number_id_string_or_client_id() {
    for iss in [json!(APP), json!(APP.to_string()), json!(CLIENT_ID)] {
        let claims = json!({ "iss": iss, "iat": NOW.unix(), "exp": NOW.unix() + 600 });
        assert_eq!(jwt_result(&claims, APP_KEY), Ok(()), "iss {iss}");
    }
}

#[test]
fn an_app_jwt_breaking_a_clock_rule_is_refused() {
    let now = NOW.unix();
    let cases = [
        (
            json!({ "iss": APP, "iat": now - 600, "exp": now - 1 }),
            Err(AuthError::JwtExpired),
        ),
        (
            json!({ "iss": APP, "iat": now - 600, "exp": now }),
            Err(AuthError::JwtExpired),
        ),
        (
            json!({ "iss": APP, "iat": now, "exp": now + 601 }),
            Err(AuthError::JwtExpTooFar),
        ),
        (
            json!({ "iss": APP, "iat": now + 61, "exp": now + 300 }),
            Err(AuthError::JwtIatInFuture),
        ),
        (
            json!({ "iss": APP, "iat": now + 60, "exp": now + 600 }),
            Ok(()),
        ),
        (
            json!({ "iss": APP, "exp": now + 300 }),
            Err(AuthError::JwtMissingIat),
        ),
        (
            json!({ "iss": APP, "iat": now }),
            Err(AuthError::JwtMissingExp),
        ),
    ];
    for (claims, want) in cases {
        assert_eq!(jwt_result(&claims, APP_KEY), want, "{claims}");
    }
}

#[test]
fn an_app_jwt_with_a_bad_issuer_key_or_algorithm_is_refused() {
    let now = NOW.unix();
    let good = json!({ "iss": APP, "iat": now, "exp": now + 300 });
    assert_eq!(jwt_result(&good, OTHER_KEY), Err(AuthError::JwtSignature));
    assert_eq!(
        jwt_result(
            &json!({ "iss": 999, "iat": now, "exp": now + 300 }),
            APP_KEY
        ),
        Err(AuthError::JwtUnknownIssuer)
    );
    let store = store();
    let hs256 = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &good,
        &jsonwebtoken::EncodingKey::from_secret(APP_PUB.as_bytes()).expect("secret"),
    )
    .expect("hs256");
    let got = store
        .view(|tx| Ok(verify_app_jwt(tx, NOW, &hs256).map(|_| ())))
        .expect("view");
    assert_eq!(got, Err(AuthError::JwtAlgorithm));
    assert_eq!(status_of(&store, &jwt(OTHER_KEY, &good)), 401);
    assert_eq!(status_of(&store, "a.b.c"), 401);
}

const COMMENT_WITH_CONTENTS: Requirement = Requirement {
    any_of: &[Grant::write("issues"), Grant::write("pull_requests")],
    all_of: &[Grant::write("contents")],
    scope: Scope::Repo,
    scopes: &["repo"],
};

#[test]
fn an_all_of_grant_is_needed_beside_one_any_of_alternative() {
    // Given an installation token that holds every grant and one that holds issues:write only
    let store = store();
    let full = actor(&store, "ghs_full");
    let issues = actor(&store, "ghs_issues");

    // When each calls an operation that needs issues or pull_requests, and contents, for writing
    let allowed = on_repo(&store, &full, &COMMENT_WITH_CONTENTS, PRIVATE);
    let denied = on_repo(&store, &issues, &COMMENT_WITH_CONTENTS, PRIVATE).expect_err("denied");

    // Then the full token passes, and the refusal lists each alternative together with contents
    assert_eq!(status(allowed), 200);
    assert_eq!(denied.status, 403);
    assert_eq!(
        header(&denied, "X-Accepted-GitHub-Permissions"),
        Some("issues=write,contents=write; pull_requests=write,contents=write")
    );
}

#[test]
fn a_user_token_needs_the_highest_role_of_its_all_of_grants() {
    // Given user-to-server tokens of alice, a writer, and bob, a reader of the private repository
    let store = store();
    let alice = actor(&store, "ghu_alice");
    let bob = actor(&store, "ghu_bob");

    // When each calls an operation whose alternatives need a reader and whose joint grant a writer
    let writer = on_repo(&store, &alice, &COMMENT_WITH_CONTENTS, PRIVATE);
    let reader = on_repo(&store, &bob, &COMMENT_WITH_CONTENTS, PRIVATE);

    // Then the writer passes and the reader is refused
    assert_eq!(status(writer), 200);
    assert_eq!(status(reader), 403);
}

#[test]
fn a_token_family_that_the_operation_does_not_accept_is_403() {
    // Given an installation token, a GitHub App user token and a classic token
    let store = store();
    let installation = actor(&store, "ghs_full");
    let user_to_server = actor(&store, "ghu_alice");
    let classic = actor(&store, "ghp_alice");

    // Then each family is refused only where GitHub's docs leave it out, and a classic token never
    assert_eq!(status(accepts_token(&installation, false, true)), 403);
    assert_eq!(status(accepts_token(&installation, true, false)), 200);
    assert_eq!(status(accepts_token(&user_to_server, true, false)), 403);
    assert_eq!(status(accepts_token(&user_to_server, false, true)), 200);
    assert_eq!(status(accepts_token(&classic, false, false)), 200);
}

#[test]
fn either_any_of_alternative_suffices_and_neither_is_403_naming_both() {
    // Given installation tokens narrowed to issues:write, to pull_requests:write, and to neither
    let store = store();

    // Then
    for credential in ["ghs_issues", "ghs_pulls", "ghs_full"] {
        let a = actor(&store, credential);
        assert_eq!(
            status(on_repo(&store, &a, &CREATE_COMMENT, PRIVATE)),
            200,
            "{credential}"
        );
    }
    let neither = actor(&store, "ghs_contents");
    let e = on_repo(&store, &neither, &CREATE_COMMENT, PRIVATE).expect_err("denied");
    assert_eq!(e.status, 403);
    assert_eq!(
        header(&e, "X-Accepted-GitHub-Permissions"),
        Some("issues=write; pull_requests=write")
    );
    assert_eq!(
        status(on_repo(&store, &neither, &READ_CONTENTS, PRIVATE)),
        200
    );
    assert_eq!(
        status(on_repo(&store, &neither, &WRITE_CONTENTS, PRIVATE)),
        403
    );
}

#[test]
fn an_installation_sees_only_the_repositories_it_and_its_token_cover() {
    let store = store();
    let full = actor(&store, "ghs_full");
    let narrowed = actor(&store, "ghs_narrowed");

    assert_eq!(status(on_repo(&store, &full, &READ_CONTENTS, PRIVATE)), 200);
    assert_eq!(
        status(on_repo(&store, &full, &READ_CONTENTS, UNINSTALLED)),
        404
    );
    assert_eq!(
        status(on_repo(&store, &full, &READ_CONTENTS, SSO_PRIVATE)),
        404
    );
    assert_eq!(
        status(on_repo(&store, &narrowed, &READ_CONTENTS, PUBLIC)),
        200
    );
    assert_eq!(
        status(on_repo(&store, &narrowed, &READ_CONTENTS, PRIVATE)),
        404
    );
    assert_eq!(
        status(on_repo(&store, &narrowed, &WRITE_CONTENTS, PUBLIC)),
        200
    );
    store
        .view(|tx| {
            let repo = |id| tx.table::<Repos>().get(id).cloned().expect("repo");
            assert!(can_see(tx, &full, &repo(PRIVATE)));
            assert!(!can_see(tx, &full, &repo(UNINSTALLED)));
            assert!(!can_see(tx, &narrowed, &repo(PRIVATE)));
            assert!(can_see(tx, &narrowed, &repo(PUBLIC)));
            Ok(())
        })
        .expect("view");
}

#[test]
fn a_token_outside_the_installation_reads_a_public_repository_but_cannot_write() {
    let store = store();
    store
        .update(|tx| {
            let mut r = (**tx.table::<Repos>().get(UNINSTALLED).expect("r")).clone();
            r.visibility = hubdouble_model::Visibility::Public;
            tx.table_mut::<Repos>().replace(r)?;
            Ok(())
        })
        .expect("publish");
    let full = actor(&store, "ghs_full");

    assert_eq!(
        status(on_repo(&store, &full, &READ_CONTENTS, UNINSTALLED)),
        200
    );
    assert_eq!(
        status(on_repo(&store, &full, &WRITE_CONTENTS, UNINSTALLED)),
        403
    );
}

#[test]
fn a_user_reaches_a_private_repository_only_with_a_role_and_the_repo_scope() {
    let store = store();
    let alice = actor(&store, "ghp_alice");
    let alice_public = actor(&store, "ghp_alice_public");
    let bob = actor(&store, "ghp_bob");
    let carol = actor(&store, "ghp_carol");
    let dave = actor(&store, "ghp_dave");

    assert_eq!(
        status(on_repo(&store, &alice, &WRITE_CONTENTS, PRIVATE)),
        200
    );
    assert_eq!(status(on_repo(&store, &alice, &ADMIN_REPO, PRIVATE)), 403);
    assert_eq!(
        status(on_repo(&store, &alice, &READ_CONTENTS, UNINSTALLED)),
        404
    );
    assert_eq!(
        status(on_repo(&store, &alice_public, &READ_CONTENTS, PRIVATE)),
        404
    );
    assert_eq!(
        status(on_repo(&store, &alice_public, &CREATE_COMMENT, PUBLIC)),
        200
    );
    assert_eq!(status(on_repo(&store, &bob, &READ_CONTENTS, PRIVATE)), 200);
    assert_eq!(status(on_repo(&store, &bob, &CREATE_COMMENT, PRIVATE)), 200);
    assert_eq!(status(on_repo(&store, &bob, &WRITE_CONTENTS, PRIVATE)), 403);
    assert_eq!(status(on_repo(&store, &bob, &WRITE_CONTENTS, PUBLIC)), 403);
    assert_eq!(
        status(on_repo(&store, &carol, &WRITE_CONTENTS, PRIVATE)),
        200
    );
    assert_eq!(
        status(on_repo(&store, &dave, &ADMIN_REPO, UNINSTALLED)),
        200
    );
    assert_eq!(status(on_repo(&store, &bob, &READ_CONTENTS, INTERNAL)), 200);
    assert_eq!(
        status(on_repo(&store, &alice, &READ_CONTENTS, INTERNAL)),
        404
    );
}

#[test]
fn a_classic_token_without_a_required_scope_is_403_naming_the_scopes() {
    let store = store();
    let alice_public = actor(&store, "ghp_alice_public");

    let e = on_repo(&store, &alice_public, &WRITE_CONTENTS, PUBLIC).expect_err("denied");

    assert_eq!(e.status, 403);
    assert_eq!(header(&e, "X-Accepted-OAuth-Scopes"), Some("repo"));
}

#[test]
fn repo_role_takes_the_highest_of_owner_collaborator_team_and_org_role() {
    let store = store();
    store
        .view(|tx: &Tx| {
            let repo = |id| tx.table::<Repos>().get(id).cloned().expect("repo");
            assert_eq!(repo_role(tx, ALICE, &repo(PRIVATE)), Some(RepoRole::Write));
            assert_eq!(repo_role(tx, BOB, &repo(PRIVATE)), Some(RepoRole::Read));
            assert_eq!(
                repo_role(tx, CAROL, &repo(PRIVATE)),
                Some(RepoRole::Maintain)
            );
            assert_eq!(repo_role(tx, DAVE, &repo(PRIVATE)), Some(RepoRole::Admin));
            assert_eq!(repo_role(tx, ALICE, &repo(UNINSTALLED)), None);
            assert_eq!(repo_role(tx, ALICE, &repo(PUBLIC)), None);
            assert_eq!(repo_role(tx, CAROL, &repo(INTERNAL)), Some(RepoRole::Read));
            Ok(())
        })
        .expect("view");
}

#[test]
fn a_team_grant_reaches_a_fork_of_its_orgs_repository_owned_by_a_user() {
    // Given a fork of acme's private repository owned by alice, granted to carol's parent team
    let store = store();
    let fork_id = 105;
    store
        .update(|tx| {
            let mut fork = (**tx.table::<Repos>().get(PRIVATE).expect("repo")).clone();
            fork.id = fork_id;
            fork.node_id = format!("R_{fork_id}");
            fork.owner_id = ALICE;
            fork.fork_of = Some(PRIVATE);
            fork.collaborators.clear();
            tx.table_mut::<Repos>().insert(fork)?;
            let mut team = (**tx.table::<Teams>().get(PARENT_TEAM).expect("team")).clone();
            team.repos.push(TeamRepo {
                repo_id: fork_id,
                role: RepoRole::Write,
            });
            tx.table_mut::<Teams>().replace(team)?;
            Ok(())
        })
        .expect("update");
    // When
    let role = store
        .view(|tx| {
            let fork = tx.table::<Repos>().get(fork_id).cloned().expect("fork");
            Ok((repo_role(tx, CAROL, &fork), repo_role(tx, BOB, &fork)))
        })
        .expect("view");
    // Then
    assert_eq!(role, (Some(RepoRole::Write), None));
}

#[test]
fn a_user_token_reaches_a_private_repository_only_where_its_app_is_installed() {
    let store = store();
    let ghu = actor(&store, "ghu_alice");

    assert_eq!(status(on_repo(&store, &ghu, &WRITE_CONTENTS, PRIVATE)), 200);
    assert_eq!(
        status(on_repo(&store, &ghu, &READ_CONTENTS, UNINSTALLED)),
        404
    );
    let e = on_repo(&store, &ghu, &ADMIN_REPO, PRIVATE).expect_err("denied");
    assert_eq!(e.status, 403);
    assert_eq!(
        header(&e, "X-Accepted-GitHub-Permissions"),
        Some("administration=write")
    );
}

#[test]
fn a_user_token_without_sso_gets_403_with_the_sso_link_until_sso_is_recorded() {
    // Given alice's user token and the SSO-enforced org
    let store = store();
    let ghu = actor(&store, "ghu_alice");

    // When
    let org = on_org(&store, &ghu, &ORG_REPOS, SSO_ORG).expect_err("sso");
    let repo = store
        .view(|tx| {
            let repo = tx.table::<Repos>().get(SSO_PRIVATE).cloned().expect("repo");
            Ok(authorize(tx, &ghu, &READ_CONTENTS, Target::Repo(&repo)))
        })
        .expect("view")
        .expect_err("sso");

    // Then
    for e in [&org, &repo] {
        assert_eq!(e.status, 403);
        assert_eq!(
            header(e, "X-GitHub-SSO"),
            Some("required; url=http://hubdouble:8000/orgs/sso-org/sso")
        );
    }
    let personal = actor(&store, "ghp_alice");
    assert_eq!(status(on_org(&store, &personal, &ORG_REPOS, SSO_ORG)), 200);

    // When SSO is recorded
    store
        .update(|tx| record_sso(tx, ALICE, SSO_ORG))
        .expect("record");

    // Then
    assert_eq!(status(on_org(&store, &ghu, &ORG_REPOS, SSO_ORG)), 200);
    assert_eq!(
        status(on_repo(&store, &ghu, &READ_CONTENTS, SSO_PRIVATE)),
        200
    );
}

#[test]
fn org_writes_need_the_grant_and_for_users_the_owner_role() {
    let store = store();

    let members_read = actor(&store, "ghs_full");
    assert_eq!(status(on_org(&store, &members_read, &ORG_REPOS, ACME)), 200);
    let e = on_org(&store, &members_read, &ORG_MEMBERS_WRITE, ACME).expect_err("denied");
    assert_eq!(
        header(&e, "X-Accepted-GitHub-Permissions"),
        Some("members=write")
    );
    assert_eq!(
        status(on_org(
            &store,
            &actor(&store, "ghp_dave"),
            &ORG_MEMBERS_WRITE,
            ACME
        )),
        200
    );
    assert_eq!(
        status(on_org(
            &store,
            &actor(&store, "ghp_carol"),
            &ORG_MEMBERS_WRITE,
            ACME
        )),
        403
    );
    assert_eq!(
        status(on_org(&store, &Actor::Anonymous, &ORG_MEMBERS_WRITE, ACME)),
        401
    );
}

#[test]
fn app_installation_and_user_scopes_admit_only_their_actor() {
    let store = store();
    let app = actor(&store, &app_jwt());
    let inst = actor(&store, "ghs_full");
    let user = actor(&store, "ghp_alice");
    let none = Target::None;
    let check = |a: &Actor, req: &Requirement| status(store.view(|tx| authorize(tx, a, req, none)));

    assert_eq!(check(&app, &APP_ONLY), 200);
    assert_eq!(check(&inst, &APP_ONLY), 401);
    assert_eq!(check(&user, &APP_ONLY), 401);
    assert_eq!(check(&Actor::Anonymous, &APP_ONLY), 401);
    assert_eq!(check(&inst, &INSTALLATION_ONLY), 200);
    assert_eq!(check(&user, &INSTALLATION_ONLY), 403);
    assert_eq!(check(&Actor::Anonymous, &INSTALLATION_ONLY), 401);
    assert_eq!(check(&user, &USER_EMAILS), 200);
    assert_eq!(check(&actor(&store, "ghp_bob"), &USER_EMAILS), 403);
    assert_eq!(check(&inst, &USER_EMAILS), 403);
}

#[test]
fn narrowing_keeps_within_the_installation_grant() {
    let store = store();
    store
        .view(|tx| {
            let i = tx
                .table::<Installations>()
                .get(INSTALLATION)
                .cloned()
                .expect("i");
            let names = ["private".to_owned()];
            let asked = perms(&[("contents", Access::Read)]);
            let got = narrow(
                tx,
                &i,
                &NarrowRequest {
                    repositories: &names,
                    repository_ids: &[PUBLIC, PRIVATE],
                    permissions: Some(&asked),
                },
            )
            .expect("narrowed");
            assert_eq!(got.repo_ids, Some(vec![PRIVATE, PUBLIC]));
            assert_eq!(got.permissions, asked);

            let all = narrow(tx, &i, &NarrowRequest::default()).expect("all");
            assert_eq!(all.repo_ids, None);
            assert_eq!(all.permissions, i.permissions);

            assert!(!i.permissions.contains_key("metadata"));
            let with_metadata = perms(&[("contents", Access::Read), ("metadata", Access::Read)]);
            let got = narrow(
                tx,
                &i,
                &NarrowRequest {
                    permissions: Some(&with_metadata),
                    ..NarrowRequest::default()
                },
            )
            .expect("metadata read is implicit");
            assert_eq!(got.permissions, with_metadata);

            let beyond = perms(&[("contents", Access::Admin)]);
            let unknown = perms(&[("actions", Access::Read)]);
            for p in [&beyond, &unknown] {
                let r = narrow(
                    tx,
                    &i,
                    &NarrowRequest {
                        permissions: Some(p),
                        ..NarrowRequest::default()
                    },
                );
                assert_eq!(r, Err(AuthError::PermissionNotGranted));
            }
            for names in [&["uninstalled".to_owned()][..], &["missing".to_owned()][..]] {
                let r = narrow(
                    tx,
                    &i,
                    &NarrowRequest {
                        repositories: names,
                        ..NarrowRequest::default()
                    },
                );
                assert_eq!(r, Err(AuthError::RepositoryNotAccessible));
            }
            let r = narrow(
                tx,
                &i,
                &NarrowRequest {
                    repository_ids: &[SSO_PRIVATE],
                    ..NarrowRequest::default()
                },
            );
            assert_eq!(r, Err(AuthError::RepositoryNotAccessible));
            assert_eq!(ApiError::from(AuthError::PermissionNotGranted).status, 422);
            assert_eq!(
                ApiError::from(AuthError::RepositoryNotAccessible).status,
                422
            );
            Ok(())
        })
        .expect("view");
}

#[test]
fn the_admin_token_opens_only_the_admin_api() {
    let admin = "s3cret-admin";

    assert_eq!(authenticate_admin(&bearer(admin), admin), Ok(()));
    assert_eq!(
        authenticate_admin(&bearer("s3cret-admiN"), admin).map_err(|e| e.status),
        Err(401)
    );
    assert_eq!(
        authenticate_admin(&HeaderMap::new(), admin).map_err(|e| e.status),
        Err(401)
    );
    assert_eq!(status_of(&store(), admin), 401);
}

#[test]
fn the_permission_list_holds_every_app_permission_of_the_openapi_bundle() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../spec/openapi/api.github.com.json"
    );
    let bundle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).expect("bundle")).expect("json");
    let props = bundle["components"]["schemas"]["app-permissions"]["properties"]
        .as_object()
        .expect("app-permissions");
    let missing: Vec<&String> = props
        .keys()
        .filter(|k| !hubdouble_auth::APP_PERMISSIONS.contains(&k.as_str()))
        .collect();

    assert!(missing.is_empty(), "missing {missing:?}");
}

#[test]
fn the_public_read_permissions_are_those_github_serves_publicly_on_a_repository_or_org() {
    // Given GitHub's permission table
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/inventory/rest-permissions.tsv"
    );
    let table = std::fs::read_to_string(path).expect("table");

    // When the permissions of the public repository and org reads are collected
    let public: std::collections::BTreeSet<&str> = public_reads(&table)
        .into_iter()
        .flat_map(|(_, grants)| grants.into_iter().map(|(permission, _)| permission))
        .collect();

    // Then they are exactly the public read permissions
    let ours: std::collections::BTreeSet<&str> = hubdouble_auth::PUBLIC_READ_PERMISSIONS
        .iter()
        .copied()
        .collect();
    assert_eq!(ours, public);
}

/// The repository and org routes GitHub marks `public_read=yes`, with the grants of each
/// all-read alternative (`|` between alternatives, `+` inside one) that installations and
/// user-to-server tokens take.
fn public_reads(table: &str) -> Vec<(&str, Vec<(&str, &str)>)> {
    let mut reads = Vec::new();
    for line in table.lines().skip(1) {
        let cells: Vec<&str> = line.split('\t').collect();
        let (route, public_read) = (cells[1], cells[5]);
        let targeted = route.contains("{owner}") && route.contains("{repo}")
            || route.contains("{org}")
            || route.contains("{repository_id}");
        if !targeted || public_read != "yes" {
            continue;
        }
        for column in [cells[2], cells[3]] {
            if matches!(column, "-" | "?" | "x") {
                continue;
            }
            for alternative in column.split('|') {
                let grants: Vec<(&str, &str)> = alternative
                    .split('+')
                    .filter_map(|g| g.split_once('='))
                    .collect();
                if !grants.is_empty() && grants.iter().all(|(_, access)| *access == "read") {
                    reads.push((route, grants));
                }
            }
        }
    }
    reads
}

#[test]
fn an_installation_outside_a_public_repository_reads_only_what_github_serves_publicly() {
    // Given a public repository that the installation does not cover
    let store = store();
    store
        .update(|tx| {
            let mut r = (**tx.table::<Repos>().get(UNINSTALLED).expect("r")).clone();
            r.visibility = hubdouble_model::Visibility::Public;
            tx.table_mut::<Repos>().replace(r)?;
            Ok(())
        })
        .expect("publish");
    let full = actor(&store, "ghs_full");

    // When it reads the contents and the webhooks
    let contents = on_repo(&store, &full, &READ_CONTENTS, UNINSTALLED);
    let hooks = on_repo(&store, &full, &READ_HOOKS, UNINSTALLED).expect_err("denied");

    // Then only the contents pass
    assert_eq!(status(contents), 200);
    assert_eq!(hooks.status, 403);
    assert_eq!(
        header(&hooks, "X-Accepted-GitHub-Permissions"),
        Some("repository_hooks=read")
    );
    assert_eq!(
        status(on_repo(&store, &Actor::Anonymous, &READ_HOOKS, UNINSTALLED)),
        404
    );
}

#[test]
fn the_org_dashboard_needs_a_signed_in_user_and_an_app_installed_on_the_org() {
    // Given an org that the App is not installed on
    let store = store();
    let other = 12;
    store
        .update(|tx| {
            let mut org = (**tx.table::<Accounts>().get(ACME).expect("acme")).clone();
            org.id = other;
            org.node_id = format!("O_{other}");
            org.login = "other".to_owned();
            tx.table_mut::<Accounts>().insert(org)?;
            Ok(())
        })
        .expect("org");

    // When the dashboard is read
    let anonymous = on_org(&store, &Actor::Anonymous, &ORG_EVENTS, ACME);
    let uninstalled = on_org(&store, &actor(&store, "ghu_alice"), &ORG_EVENTS, other);
    let member = on_org(&store, &actor(&store, "ghp_carol"), &ORG_EVENTS, ACME);
    let outsider = on_org(&store, &actor(&store, "ghp_alice"), &ORG_EVENTS, ACME);

    // Then an anonymous caller is 401 and the uninstalled App's user token 403
    assert_eq!(status(anonymous), 401);
    let e = uninstalled.expect_err("denied");
    assert_eq!(e.status, 403);
    assert_eq!(
        header(&e, "X-Accepted-GitHub-Permissions"),
        Some("organization_events=read")
    );
    // And a signed-in user passes, member or not, as GitHub then serves the public events
    assert_eq!(status(member), 200);
    assert_eq!(status(outsider), 200);
}

#[test]
fn a_classic_token_with_only_a_webhook_scope_manages_the_webhooks_of_a_repository_it_admins() {
    // Given dave, an acme owner, and bob, a reader, with tokens that hold only admin:repo_hook
    let store = store();
    store
        .update(|tx| {
            let mut tokens = tx.table_mut::<hubdouble_store::Tokens>();
            tokens.insert(personal(290, "ghp_dave_hooks", DAVE, &["admin:repo_hook"]))?;
            tokens.insert(personal(291, "ghp_bob_hooks", BOB, &["admin:repo_hook"]))?;
            Ok(())
        })
        .expect("tokens");
    let dave = actor(&store, "ghp_dave_hooks");
    let bob = actor(&store, "ghp_bob_hooks");

    // When they update and delete the webhooks of the public and the private repository
    let answers = |who: &Actor| {
        [PUBLIC, PRIVATE].map(|repo| {
            [&WRITE_HOOKS, &DELETE_HOOKS].map(|req| status(on_repo(&store, who, req, repo)))
        })
    };

    // Then the owner passes, and the reader still needs the admin role
    assert_eq!(answers(&dave), [[200, 200], [200, 200]]);
    assert_eq!(answers(&bob), [[404, 404], [404, 404]]);
}
