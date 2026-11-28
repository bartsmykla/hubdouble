use http::Method;
use hubdouble_store::{CheckRuns, IssueComments, Issues, Releases, Rulesets, Statuses, Teams};
use hubdouble_testkit::{Answer, GoGithub, TestServer, fixture};
use serde_json::{Value, json};

const KEPT_REPO_ID: i64 = 3101;
const NOTES_REPO_ID: i64 = 3102;
const DOOMED_ORG_ID: i64 = 2101;
const DOOMED_REPO_ID: i64 = 3103;
const DOOMED_INSTALLATION_ID: i64 = 5101;
const TEMP_TEAM_ID: i64 = 6101;
const KEPT_TEAM_ID: i64 = 6102;

const REPO_FEEDS: [&str; 8] = [
    "issues",
    "issues/comments",
    "pulls",
    "commits/main/check-runs",
    "commits/main/statuses",
    "releases",
    "rules/branches/main",
    "actions/variables",
];

fn repository(id: i64, owner: &str, name: &str) -> Value {
    json!({"kind": "repository", "id": id, "owner": owner, "name": name,
           "visibility": "public", "has_issues": true,
           "files": {"README.md": format!("# {name}\n")},
           "commits": [{"branch": "feature", "message": "add the feature",
                        "files": {"src/feature.txt": "feature\n"}}]})
}

fn repo_rows(owner: &str, name: &str) -> Vec<Value> {
    let full = format!("{owner}/{name}");
    vec![
        json!({"kind": "pull_request", "repository": full, "user": "alice",
               "title": "Ship the feature", "head": "feature", "base": "main"}),
        json!({"kind": "check_run", "repository": full, "app": fixture::APP_SLUG,
               "name": "build", "head_sha": "main"}),
        json!({"kind": "status", "repository": full, "sha": "main", "creator": "alice",
               "state": "pending", "context": "ci"}),
        json!({"kind": "release", "repository": full, "tag_name": "v2.0.0"}),
        json!({"kind": "ruleset", "repository": full, "name": "rules", "target": "branch",
               "enforcement": "active", "bypass_actors": [],
               "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
               "rules": [{"type": "required_linear_history"}]}),
        json!({"kind": "actions_variable", "repository": full, "name": "STAGE",
               "value": "prod"}),
    ]
}

fn carol_in_acme() -> Value {
    json!({"kind": "org_membership", "org": "acme", "login": "carol"})
}

async fn start(extra: Vec<Value>) -> TestServer {
    let mut builder = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture();
    for entry in extra {
        builder = builder.entry(entry);
    }
    builder.start().await
}

async fn admin(server: &TestServer, method: Method, path: &str, body: Option<&Value>) -> Answer {
    let answer = server.admin(method, path, body).await;
    assert!(
        answer.status.is_success(),
        "{path}: {} {:?}",
        answer.status,
        answer.body
    );
    answer
}

async fn status(client: &GoGithub, path: &str) -> u16 {
    client.get(path).await.status.as_u16()
}

async fn add_comment(client: &GoGithub, owner: &str, name: &str) {
    let answer = client
        .post(
            &format!("repos/{owner}/{name}/issues/1/comments"),
            &json!({"body": "looks good"}),
        )
        .await;
    assert_eq!(answer.status.as_u16(), 201, "{:?}", answer.body);
}

async fn feed_sizes(client: &GoGithub, owner: &str, name: &str) -> Vec<usize> {
    let mut sizes = Vec::new();
    for feed in REPO_FEEDS {
        let path = format!("repos/{owner}/{name}/{feed}");
        let answer = client.get(&path).await;
        assert_eq!(answer.status.as_u16(), 200, "{path}: {:?}", answer.body);
        let body = answer.json();
        let size = body.as_array().map_or_else(
            || {
                body["total_count"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .unwrap_or_else(|| panic!("{path}: {body}"))
            },
            Vec::len,
        );
        sizes.push(size);
    }
    sizes
}

fn assert_no_repo_rows(server: &TestServer, repo_id: i64) {
    server
        .server
        .services()
        .store
        .view(|tx| {
            assert!(tx.table::<Issues>().in_repo(repo_id).next().is_none());
            assert!(
                tx.table::<IssueComments>()
                    .in_repo(repo_id)
                    .next()
                    .is_none()
            );
            assert!(tx.table::<CheckRuns>().in_repo(repo_id).next().is_none());
            assert!(tx.table::<Statuses>().in_repo(repo_id).next().is_none());
            assert!(tx.table::<Releases>().in_repo(repo_id).next().is_none());
            assert!(tx.table::<Rulesets>().in_repo(repo_id).next().is_none());
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{error}"));
}

#[tokio::test]
async fn deleting_a_repository_drops_the_rows_of_every_feature_and_keeps_other_repositories() {
    // Given two repositories with an issue, a comment, a pull request, a check run, a status,
    // a release, a ruleset and an Actions variable each
    let mut extra = vec![repository(KEPT_REPO_ID, fixture::ORG, "kept")];
    extra.extend(repo_rows(fixture::ORG, fixture::PUBLIC_REPO));
    extra.extend(repo_rows(fixture::ORG, "kept"));
    let server = start(extra).await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    for name in [fixture::PUBLIC_REPO, "kept"] {
        add_comment(&alice, fixture::ORG, name).await;
    }
    let kept = feed_sizes(&alice, fixture::ORG, "kept").await;
    assert!(kept.iter().all(|size| *size > 0), "{kept:?}");

    // When the public repository is deleted
    admin(&server, Method::DELETE, "repos/acme/public", None).await;

    // Then its reads answer 404, its rows are gone, and the other repository keeps its rows
    for feed in REPO_FEEDS {
        assert_eq!(
            status(&alice, &format!("repos/acme/public/{feed}")).await,
            404
        );
    }
    assert_no_repo_rows(&server, fixture::PUBLIC_REPO_ID);
    assert_eq!(feed_sizes(&alice, fixture::ORG, "kept").await, kept);

    // And a new repository with the same name starts empty
    let mut fresh = repository(0, fixture::ORG, fixture::PUBLIC_REPO);
    if let Some(entry) = fresh.as_object_mut() {
        entry.remove("id");
        entry.remove("kind");
    }
    admin(&server, Method::POST, "repository", Some(&fresh)).await;
    let fresh = feed_sizes(&alice, fixture::ORG, fixture::PUBLIC_REPO).await;
    assert!(fresh.iter().all(|size| *size == 0), "{fresh:?}");
}

#[tokio::test]
async fn deleting_a_team_drops_its_members_and_repositories_and_keeps_other_teams() {
    // Given a team with a member and a repository, and another team
    let server = start(vec![
        carol_in_acme(),
        json!({"kind": "team", "id": TEMP_TEAM_ID, "org": "acme", "name": "Temp",
               "members": [{"login": "carol"}],
               "repositories": [{"repository": "acme/public", "permission": "write"}]}),
        json!({"kind": "team", "id": KEPT_TEAM_ID, "org": "acme", "name": "Kept",
               "members": [{"login": "bob"}]}),
    ])
    .await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);

    // When the first team is deleted
    admin(&server, Method::DELETE, "team/acme/temp", None).await;

    // Then it is gone from reads and the other team stays with its member
    assert_eq!(status(&alice, "orgs/acme/teams/temp").await, 404);
    assert_eq!(
        server
            .admin(Method::GET, "team/acme/temp", None)
            .await
            .status
            .as_u16(),
        404
    );
    let teams = alice.get("orgs/acme/teams").await.json();
    let slugs: Vec<&str> = teams
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["slug"].as_str())
        .collect();
    assert_eq!(slugs, ["kept"]);
    let kept = admin(&server, Method::GET, "team/acme/kept", None)
        .await
        .json();
    assert_eq!(kept["members"].as_array().map(Vec::len), Some(1));

    // And a new team with the same name has no members or repositories
    admin(
        &server,
        Method::POST,
        "team",
        Some(&json!({"org": "acme", "name": "Temp"})),
    )
    .await;
    let fresh = admin(&server, Method::GET, "team/acme/temp", None)
        .await
        .json();
    assert_eq!(fresh["members"], json!([]));
    assert_eq!(fresh["repos"], json!([]));
    server
        .server
        .services()
        .store
        .view(|tx| {
            assert!(tx.table::<Teams>().get(TEMP_TEAM_ID).is_none());
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{error}"));
}

#[tokio::test]
async fn deleting_a_user_drops_their_repositories_memberships_and_tokens_and_keeps_other_users() {
    // Given carol with a repository and an issue in it, an org membership, a team membership
    // and a collaborator role
    let server = start(vec![
        repository(NOTES_REPO_ID, fixture::CAROL, "notes"),
        carol_in_acme(),
        json!({"kind": "team", "id": TEMP_TEAM_ID, "org": "acme", "name": "Temp",
               "members": [{"login": "carol"}, {"login": "bob"}]}),
    ])
    .await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let carol = GoGithub::new(&server).with_token(fixture::CAROL_TOKEN);
    admin(
        &server,
        Method::PUT,
        "repository/acme/public/collaborators/carol",
        Some(&json!({"permission": "write"})),
    )
    .await;
    let issue = carol
        .post("repos/carol/notes/issues", &json!({"title": "todo"}))
        .await;
    assert_eq!(issue.status.as_u16(), 201, "{:?}", issue.body);
    assert_eq!(status(&alice, "orgs/acme/members/carol").await, 204);

    // When carol is deleted
    admin(&server, Method::DELETE, "user/carol", None).await;

    // Then carol, her repository, her token and her memberships are gone
    assert_eq!(status(&alice, "users/carol").await, 404);
    assert_eq!(status(&alice, "repos/carol/notes").await, 404);
    assert_eq!(status(&carol, "user").await, 401);
    assert_eq!(status(&alice, "orgs/acme/members/carol").await, 404);
    assert_eq!(
        server
            .admin(Method::GET, "user/carol", None)
            .await
            .status
            .as_u16(),
        404
    );
    assert_no_repo_rows(&server, NOTES_REPO_ID);
    let team = admin(&server, Method::GET, "team/acme/temp", None)
        .await
        .json();
    assert_eq!(team["members"].as_array().map(Vec::len), Some(1));

    // And bob keeps his account and memberships
    assert_eq!(status(&alice, "users/bob").await, 200);
    assert_eq!(status(&alice, "orgs/acme/members/bob").await, 204);

    // And a new user with the same login has no memberships or roles
    admin(
        &server,
        Method::POST,
        "user",
        Some(&json!({"login": "carol", "tokens": [{"token": fixture::CAROL_TOKEN}]})),
    )
    .await;
    assert_eq!(status(&alice, "orgs/acme/members/carol").await, 404);
    let permission = alice
        .get("repos/acme/public/collaborators/carol/permission")
        .await
        .json();
    assert_ne!(permission["permission"], "write");
}

#[tokio::test]
async fn deleting_an_org_drops_its_repositories_teams_and_installations_and_keeps_other_orgs() {
    // Given an org with a member, a repository with an issue, a team and an App installation
    let server = start(vec![
        json!({"kind": "org", "id": DOOMED_ORG_ID, "login": "doomed",
               "members": [{"login": "alice", "role": "admin"}, {"login": "bob"}]}),
        repository(DOOMED_REPO_ID, "doomed", "app"),
        json!({"kind": "team", "org": "doomed", "name": "Core", "members": [{"login": "bob"}]}),
        json!({"kind": "installation", "id": DOOMED_INSTALLATION_ID, "app": fixture::APP_SLUG,
               "account": "doomed", "repositories": ["app"],
               "permissions": {"issues": "write", "metadata": "read"}}),
    ])
    .await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let issue = alice
        .post("repos/doomed/app/issues", &json!({"title": "todo"}))
        .await;
    assert_eq!(issue.status.as_u16(), 201, "{:?}", issue.body);

    // When the org is deleted
    admin(&server, Method::DELETE, "org/doomed", None).await;

    // Then the org, its repository, team, memberships and installation are gone
    assert_eq!(status(&alice, "orgs/doomed/members/alice").await, 404);
    assert_eq!(status(&alice, "repos/doomed/app").await, 404);
    assert_eq!(status(&alice, "orgs/doomed/teams/core").await, 404);
    assert_eq!(
        server
            .admin(
                Method::GET,
                &format!("installation/{DOOMED_INSTALLATION_ID}"),
                None
            )
            .await
            .status
            .as_u16(),
        404
    );
    assert_no_repo_rows(&server, DOOMED_REPO_ID);

    // And the other org, its repository and the App stay
    assert_eq!(status(&alice, "orgs/acme/members/bob").await, 204);
    assert_eq!(status(&alice, "repos/acme/public").await, 200);
    admin(&server, Method::GET, "app/fixture-app", None).await;

    // And a new org with the same login has no members or teams
    admin(
        &server,
        Method::POST,
        "org",
        Some(&json!({"login": "doomed"})),
    )
    .await;
    for path in ["org/doomed/members/bob", "team/doomed/core"] {
        let answer = server.admin(Method::GET, path, None).await;
        assert_eq!(answer.status.as_u16(), 404, "{path}");
    }
}
