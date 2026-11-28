use std::collections::BTreeMap;
use std::fmt::Write as _;

use http::{Method, StatusCode};
use hubdouble_admin::Registrar;
use hubdouble_gql::{Args, Ctx, GqlError, Obj, Resolved};
use hubdouble_model::{Access, Account, Repo, Timestamp, Token, TokenKind};
use hubdouble_store::Tokens;
use hubdouble_testkit::{
    Answer, Builder, GoGithub, Graphql, TestServer, fixture, standins, validate,
};
use serde_json::{Value, json};

const TEAM_PATH: &str = "orgs/acme/teams";
const TEAM_REPO: &str = "orgs/acme/teams/child/repos/acme/public";
const BOB_PERMISSION: &str = "repos/acme/public/collaborators/bob/permission";
const TEAM_REPO_MEDIA: &str = "application/vnd.github.v3.repository+json";
const MEMBERS_TOKEN: &str = "ghs_members_conformance";
const METADATA_TOKEN: &str = "ghs_metadata_conformance";
const ORG_ONLY: &str = "ghp_org_only_conformance";
const OTHER_APP_USER: &str = "ghu_bob_of_an_app_outside_acme";
const ACME_APP_USER: &str = "ghu_bob_of_the_app_on_acme";
const OTHER_ORG_INSTALLATION: &str = "ghs_installation_of_acme_sso";
const OTHER_APP_ID: i64 = 4101;
const OTHER_INSTALLATION_ID: i64 = 5101;
const ROOT: i64 = 6001;
const CHILD: i64 = 6002;
const SECRET: i64 = 6003;

fn standin_types(r: &mut Registrar<'_>) {
    fn database_id(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
        Ok(Resolved::Leaf(json!(obj.get::<Account>()?.id)))
    }
    r.gql.object::<Account>("User");
    r.gql.field("User", "databaseId", database_id);
    r.gql.object::<Repo>("Repository");
}

fn setup() -> Builder {
    let mut builder = TestServer::builder()
        .register(hubdouble_api_orgs::register)
        .register(standins::query_repository)
        .register(standin_types);
    for mut entry in fixture::seed() {
        match entry["kind"].as_str() {
            Some("app") => {
                entry["events"] = json!(["organization", "membership", "member", "team"]);
                entry["permissions"]["members"] = json!("read");
            }
            Some("installation") => entry["permissions"]["members"] = json!("read"),
            Some("user") if entry["login"] == "alice" => {
                entry["tokens"] = json!([
                    {"token": fixture::ALICE_TOKEN},
                    {"token": ORG_ONLY, "scopes": ["read:org"]},
                ]);
            }
            _ => {}
        }
        builder = builder.entry(entry);
    }
    builder
        .entry(json!({
            "kind": "team", "id": ROOT, "org": "acme", "name": "Root",
            "repositories": [{"repository": "acme/public", "permission": "maintain"}],
        }))
        .entry(json!({
            "kind": "team", "id": CHILD, "org": "acme", "name": "Child", "parent": "root",
            "members": [{"login": "bob", "role": "maintainer"}],
            "repositories": [{"repository": "acme/public", "permission": "read"}],
        }))
        .entry(json!({
            "kind": "team", "id": SECRET, "org": "acme", "name": "Secret", "privacy": "secret",
            "members": [{"login": "alice"}],
        }))
}

async fn start(builder: Builder) -> TestServer {
    let server = builder.start().await;
    server
        .server
        .services()
        .store
        .update(|tx| {
            for (token, permission) in [(MEMBERS_TOKEN, "members"), (METADATA_TOKEN, "metadata")] {
                let id = tx.next_id();
                tx.table_mut::<Tokens>().insert(Token {
                    id,
                    token: token.into(),
                    kind: TokenKind::Installation,
                    user_id: None,
                    app_id: Some(fixture::APP_ID),
                    installation_id: Some(fixture::INSTALLATION_ID),
                    repo_ids: Some(vec![fixture::PUBLIC_REPO_ID]),
                    permissions: BTreeMap::from([(permission.into(), Access::Read)]),
                    scopes: Vec::new(),
                    expires_at: None,
                    revoked: false,
                    owner_seeded: true,
                    created_at: Timestamp::SEED,
                })?;
            }
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{error}"));
    server
}

fn body(answer: &Answer, operation: &str) -> Value {
    assert_eq!(
        answer.status,
        StatusCode::OK,
        "{operation}: {:?}",
        answer.body
    );
    answer.json()
}

async fn assert_status(client: &GoGithub, path: &str, status: u16) -> Answer {
    let answer = client.get(path).await;
    assert_eq!(answer.status.as_u16(), status, "{path}: {:?}", answer.body);
    answer
}

#[tokio::test]
async fn org_and_team_reads_use_members_grants_and_the_team_hierarchy() {
    let server = start(setup()).await;
    let client = GoGithub::enterprise(&server).with_token(MEMBERS_TOKEN);
    let denied = GoGithub::enterprise(&server).with_token(METADATA_TOKEN);
    let bob = GoGithub::enterprise(&server).with_token(fixture::BOB_TOKEN);

    // Given: go-github v93 github/orgs_members.go:111 and github/teams.go use these paths.
    assert!(
        assert_status(&client, "orgs/acme/members/BOB", 204)
            .await
            .body
            .is_empty()
    );
    assert_status(&client, "orgs/acme/members/carol", 404).await;
    let first = client.get(&format!("{TEAM_PATH}?page=1&per_page=1")).await;
    let next = format!(
        "<{}organizations/{}/teams?page=2&per_page=1>; rel=\"next\"",
        client.base(),
        fixture::ORG_ID
    );
    assert!(
        first.header("link").contains(&next),
        "{}",
        first.header("link")
    );
    assert_eq!(body(&first, "teams/list")[0]["id"], ROOT);
    let second = client.get(&format!("{TEAM_PATH}?page=2&per_page=1")).await;
    let second = body(&second, "teams/list");
    assert_eq!(second[0]["id"], CHILD);
    assert_eq!(second[0]["parent"]["id"], ROOT);
    let all = body(
        &bob.get(&format!("{TEAM_PATH}?per_page=100")).await,
        "teams/list",
    );
    assert_eq!(all.as_array().map(Vec::len), Some(2));
    assert_status(&bob, &format!("{TEAM_PATH}/secret"), 404).await;
    let child = body(
        &client.get(&format!("{TEAM_PATH}/CHILD")).await,
        "teams/get-by-name",
    );
    assert_eq!(child["id"], CHILD);
    assert_eq!(child["organization"]["id"], fixture::ORG_ID);
    assert_eq!(child["parent"]["id"], ROOT);
    assert_eq!(child["members_count"], 1);
    assert_eq!(child["repos_count"], 1);
    let direct = format!("{TEAM_PATH}/child/memberships/bob");
    let inherited = format!("{TEAM_PATH}/root/memberships/bob");
    for (path, role) in [(&direct, "maintainer"), (&inherited, "member")] {
        let membership = body(
            &client.get(path).await,
            "teams/get-membership-for-user-in-org",
        );
        assert_eq!(membership["state"], "active");
        assert_eq!(membership["role"], role);
    }
    assert_status(
        &client,
        &format!("{TEAM_PATH}/child/memberships/alice"),
        404,
    )
    .await;
    assert_status(&client, &format!("{TEAM_PATH}/absent"), 404).await;
    for path in [
        "orgs/acme/members/bob",
        TEAM_PATH,
        &format!("{TEAM_PATH}/child"),
        &direct,
    ] {
        let answer = assert_status(&denied, path, 403).await;
        assert_eq!(
            answer.header("x-accepted-github-permissions"),
            "members=read"
        );
    }
}

#[tokio::test]
async fn organization_membership_redirect_ends_at_public_membership() {
    let server = start(setup().entry(json!({
        "kind": "org_membership", "org": "acme", "login": "alice", "public": true,
    })))
    .await;
    let client = GoGithub::enterprise(&server).with_token(fixture::CAROL_TOKEN);
    for (user, status) in [("alice", 204), ("bob", 404), ("carol", 404)] {
        let response = assert_status(&client, &format!("orgs/acme/members/{user}"), 302).await;
        let path = response
            .header("location")
            .strip_prefix(client.base())
            .unwrap();
        assert_eq!(
            path,
            format!("organizations/{}/public_members/{user}", fixture::ORG_ID)
        );
        assert_status(&client, path, status).await;
    }
    let anonymous = GoGithub::enterprise(&server);
    assert_status(&anonymous, "orgs/acme/public_members/ALICE", 204).await;
    let update = server
        .admin(
            Method::PUT,
            "org/acme/members/alice",
            Some(&json!({"public": false})),
        )
        .await;
    assert_eq!(update.status, StatusCode::NO_CONTENT);
    assert_status(&anonymous, "orgs/acme/public_members/alice", 404).await;
}

#[tokio::test]
async fn team_reads_need_a_signed_in_member_of_the_org() {
    // Given an anonymous caller and carol, who is not a member of acme
    let server = start(setup()).await;
    let anonymous = GoGithub::enterprise(&server);
    let carol = GoGithub::enterprise(&server).with_token(fixture::CAROL_TOKEN);
    let team_reads = [
        format!("{TEAM_PATH}/child"),
        format!("{TEAM_PATH}/child/memberships/bob"),
        TEAM_REPO.to_owned(),
    ];

    // When each reads the teams, a team, a membership, a team repository and a permission
    let mut anonymous_answers = vec![assert_status(&anonymous, TEAM_PATH, 401).await];
    for path in team_reads
        .iter()
        .map(String::as_str)
        .chain([BOB_PERMISSION])
    {
        anonymous_answers.push(assert_status(&anonymous, path, 401).await);
    }
    let carol_list = assert_status(&carol, TEAM_PATH, 403).await;

    // Then the anonymous reads need authentication, and carol may not list or find teams
    for answer in &anonymous_answers {
        assert_eq!(answer.json()["status"], "401");
        assert_eq!(
            answer.json()["documentation_url"],
            "https://docs.github.com/rest"
        );
    }
    assert_eq!(
        carol_list.json()["documentation_url"],
        "https://docs.github.com/rest/teams/teams#list-teams"
    );
    for path in &team_reads {
        assert_status(&carol, path, 404).await;
    }
}

async fn with_other_org_app() -> TestServer {
    let server = start(
        setup()
            .entry(json!({
                "kind": "app", "id": OTHER_APP_ID, "slug": "other-org-app", "name": "Other Org App",
                "owner": fixture::SSO_ORG, "public_key": fixture::APP_PUBLIC_KEY,
                "permissions": {"members": "read", "metadata": "read"}, "events": [],
            }))
            .entry(json!({
                "kind": "installation", "id": OTHER_INSTALLATION_ID, "app": "other-org-app",
                "account": fixture::SSO_ORG,
                "permissions": {"members": "read", "metadata": "read"},
            })),
    )
    .await;
    server
        .server
        .services()
        .store
        .update(|tx| {
            for (token, kind, user_id, app_id, installation_id) in [
                (
                    OTHER_APP_USER,
                    TokenKind::User,
                    Some(fixture::BOB_ID),
                    OTHER_APP_ID,
                    None,
                ),
                (
                    ACME_APP_USER,
                    TokenKind::User,
                    Some(fixture::BOB_ID),
                    fixture::APP_ID,
                    None,
                ),
                (
                    OTHER_ORG_INSTALLATION,
                    TokenKind::Installation,
                    None,
                    OTHER_APP_ID,
                    Some(OTHER_INSTALLATION_ID),
                ),
            ] {
                let id = tx.next_id();
                tx.table_mut::<Tokens>().insert(Token {
                    id,
                    token: token.into(),
                    kind,
                    user_id,
                    app_id: Some(app_id),
                    installation_id,
                    repo_ids: None,
                    permissions: BTreeMap::new(),
                    scopes: Vec::new(),
                    expires_at: None,
                    revoked: false,
                    owner_seeded: true,
                    created_at: Timestamp::SEED,
                })?;
            }
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{error}"));
    server
}

#[tokio::test]
async fn team_reads_refuse_a_user_token_of_an_app_not_installed_on_the_org() {
    // Given bob, a member of acme, with a user token of an App installed only on acme-sso and
    // one of the App installed on acme with members=read
    let server = with_other_org_app().await;
    let outside = GoGithub::enterprise(&server).with_token(OTHER_APP_USER);
    let inside = GoGithub::enterprise(&server).with_token(ACME_APP_USER);
    let team_reads = [
        TEAM_PATH.to_owned(),
        format!("{TEAM_PATH}/child"),
        format!("{TEAM_PATH}/child/memberships/bob"),
        TEAM_REPO.to_owned(),
    ];

    for path in &team_reads {
        // When each token reads acme's teams, a team, a membership and a team repository
        let refused = assert_status(&outside, path, 403).await;

        // Then GitHub refuses the App without access to acme and names members=read, which
        // rest-permissions.tsv marks public_read=no, and serves the App installed on acme
        assert!(
            refused
                .header("x-accepted-github-permissions")
                .contains("members=read"),
            "{path}"
        );
        assert!(inside.get(path).await.status.is_success(), "{path}");
    }

    // And the membership check treats the App without access to acme as an outsider, which
    // GitHub sends to the public membership check, and answers the App on acme directly
    let redirect = assert_status(&outside, "orgs/acme/members/bob", 302).await;
    assert!(redirect.header("location").ends_with(&format!(
        "organizations/{}/public_members/bob",
        fixture::ORG_ID
    )));
    assert_status(&inside, "orgs/acme/members/bob", 204).await;
}

#[tokio::test]
async fn graphql_team_and_collaborator_reads_refuse_an_app_without_access_to_the_org() {
    // Given bob, a member of acme and a maintainer of acme/public, with a user token of an App
    // installed only on acme-sso and one of the App installed on acme, and an installation
    // of acme-sso
    let server = with_other_org_app().await;
    let inside = Graphql::new(GoGithub::enterprise(&server).with_token(ACME_APP_USER));
    let outside = Graphql::new(GoGithub::enterprise(&server).with_token(OTHER_APP_USER));
    let installation =
        Graphql::new(GoGithub::enterprise(&server).with_token(OTHER_ORG_INSTALLATION));
    let teams = r#"query { organization(login: "acme") { teams(first: 10) { nodes { id } } } }"#;
    let collaborators = r#"query { repository(owner: "acme", name: "public") { collaborators(first: 1) { edges { node { login } } } } }"#;
    let listed = inside.query(teams, &json!({})).await.json();
    let team_id = listed["data"]["organization"]["teams"]["nodes"][0]["id"].clone();
    assert!(team_id.is_string(), "{listed}");
    let node = "query($id: ID!) { node(id: $id) { ... on Team { id } } }";

    // When the App without access to acme reads acme's teams, a team node and the
    // collaborators, and the installation of acme-sso reads acme's teams
    let answers = [
        outside.query(teams, &json!({})).await.json(),
        outside.query(node, &json!({"id": team_id})).await.json(),
        outside.query(collaborators, &json!({})).await.json(),
        installation.query(teams, &json!({})).await.json(),
    ];

    // Then GitHub refuses each, as rest-permissions.tsv marks teams/list,
    // teams/get-by-name and repos/list-collaborators public_read=no
    for answer in &answers {
        assert_eq!(answer["errors"][0]["type"], "FORBIDDEN", "{answer}");
    }
    let inside_collaborators = inside.query(collaborators, &json!({})).await.json();
    assert!(
        inside_collaborators.get("errors").is_none(),
        "{inside_collaborators}"
    );
}

#[tokio::test]
async fn collaborator_permissions_refuse_an_installation_on_another_org() {
    // Given an installation of acme-sso, which does not cover the public acme/public
    let server = with_other_org_app().await;
    let installation = GoGithub::enterprise(&server).with_token(OTHER_ORG_INSTALLATION);
    let query = r#"query { repository(owner: "acme", name: "public") { collaborators(first: 1) { edges { permission node { login } } } } }"#;

    // When it reads a collaborator permission through REST and GraphQL
    let rest = assert_status(&installation, BOB_PERMISSION, 403).await;
    let graphql = Graphql::new(installation)
        .query(query, &json!({}))
        .await
        .json();

    // Then REST refuses it, as rest-permissions.tsv marks the read public_read=no, and GraphQL
    // refuses it the collaborators, which need push access
    assert_eq!(
        rest.header("x-accepted-github-permissions"),
        "metadata=read"
    );
    assert!(
        graphql["data"]["repository"]["collaborators"].is_null(),
        "{graphql}"
    );
    assert_eq!(graphql["errors"][0]["type"], "FORBIDDEN", "{graphql}");
}

#[tokio::test]
async fn collaborator_permissions_need_push_access() {
    // Given carol, a reader of acme/public, and bob, a maintainer of it through his team
    let server = start(setup()).await;
    let carol = GoGithub::enterprise(&server).with_token(fixture::CAROL_TOKEN);
    let bob = GoGithub::enterprise(&server).with_token(fixture::BOB_TOKEN);
    let query = r#"query { repository(owner: "acme", name: "public") { collaborators(first: 1) { edges { permission node { login } } } } }"#;

    // When each reads a collaborator permission through REST and GraphQL
    let carol_rest = assert_status(&carol, BOB_PERMISSION, 403).await;
    let bob_rest = body(
        &bob.get(BOB_PERMISSION).await,
        "repos/get-collaborator-permission-level",
    );
    let carol_graphql = Graphql::new(carol).query(query, &json!({})).await.json();
    let bob_graphql = Graphql::new(bob).query(query, &json!({})).await.json();

    // Then only bob reads them, and carol gets GitHub's refusal
    assert_eq!(
        carol_rest.json()["documentation_url"],
        "https://docs.github.com/rest/collaborators/collaborators#get-repository-permissions-for-a-user"
    );
    assert_eq!(bob_rest["permission"], "write");
    assert!(
        carol_graphql["data"]["repository"]["collaborators"].is_null(),
        "{carol_graphql}"
    );
    assert_eq!(carol_graphql["errors"][0]["type"], "FORBIDDEN");
    assert_eq!(
        carol_graphql["errors"][0]["path"],
        json!(["repository", "collaborators"])
    );
    assert!(bob_graphql.get("errors").is_none(), "{bob_graphql}");
    assert_eq!(
        bob_graphql["data"]["repository"]["collaborators"]["edges"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
}

#[tokio::test]
async fn repository_checks_return_team_permissions_and_legacy_collaborator_roles() {
    let server = start(setup()).await;
    let client = GoGithub::enterprise(&server).with_token(MEMBERS_TOKEN);
    assert!(assert_status(&client, TEAM_REPO, 204).await.body.is_empty());
    // When: go-github v93 github/teams.go:508 requests the repository media type.
    let response = client
        .call_headers(
            Method::GET,
            TEAM_REPO,
            http::HeaderMap::from_iter([(
                http::header::ACCEPT,
                http::HeaderValue::from_static(TEAM_REPO_MEDIA),
            )]),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    let repo = response.json();
    assert_eq!(repo["id"], fixture::PUBLIC_REPO_ID);
    assert_eq!(repo["role_name"], "maintain");
    assert_eq!(repo["permissions"]["push"], true);
    assert_eq!(repo["permissions"]["admin"], false);
    assert_eq!(repo["permissions"]["maintain"], true);

    // Then: go-github v93 github/repos_collaborators.go:111 keeps the default media type.
    let collaborator = body(
        &client.get(BOB_PERMISSION).await,
        "repos/get-collaborator-permission-level",
    );
    assert_eq!(collaborator["permission"], "write");
    assert_eq!(collaborator["role_name"], "maintain");
    assert_eq!(collaborator["user"]["id"], fixture::BOB_ID);
    assert_eq!(collaborator["user"]["permissions"]["push"], true);
    let no_role = body(
        &client
            .get("repos/acme/public/collaborators/carol/permission")
            .await,
        "repos/get-collaborator-permission-level",
    );
    assert_eq!(no_role["permission"], "none");
    assert_eq!(no_role["user"]["permissions"]["pull"], false);
    for path in [
        "repos/acme/private/collaborators/bob/permission",
        "repos/acme/public/collaborators/absent/permission",
        "orgs/acme/teams/child/repos/acme/private",
        "orgs/acme/teams/secret/repos/acme/public",
    ] {
        assert_status(&client, path, 404).await;
    }
    let insufficient = GoGithub::enterprise(&server).with_token(METADATA_TOKEN);
    assert_status(&insufficient, TEAM_REPO, 403).await;
    let org_only = GoGithub::enterprise(&server).with_token(ORG_ONLY);
    assert_status(&org_only, TEAM_REPO, 204).await;
    assert_status(&org_only, BOB_PERMISSION, 200).await;
}

#[tokio::test]
async fn team_document_filters_by_login_and_paginates_with_ancestors() {
    let mut builder = setup();
    for id in 6100..6200 {
        builder = builder.entry(json!({
            "kind": "team", "id": id, "org": "acme", "name": format!("Group {id}"),
            "members": [{"login": "bob"}],
        }));
    }
    let server = start(builder).await;
    let api = Graphql::new(GoGithub::enterprise(&server).with_token(MEMBERS_TOKEN));
    let doc = include_str!("../../../docs/inventory/graphql/teams.graphql");
    let first = api
        .query(doc, &json!({"org": "acme", "login": "BOB"}))
        .await
        .json();
    assert!(first.get("errors").is_none(), "{first}");
    let teams = &first["data"]["organization"]["teams"];
    let nodes = teams["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("{teams}"));
    assert_eq!(nodes.len(), 100);
    assert_eq!(nodes[0]["databaseId"], CHILD);
    assert_eq!(
        nodes[0]["ancestors"]["nodes"],
        json!([{ "databaseId": ROOT }])
    );
    assert_eq!(nodes[99]["databaseId"], 6198);
    assert_eq!(teams["pageInfo"]["hasNextPage"], true);
    let second = api
        .query(
            doc,
            &json!({
                "org": "acme", "login": "BOB", "after": teams["pageInfo"]["endCursor"],
            }),
        )
        .await
        .json();
    assert!(second.get("errors").is_none(), "{second}");
    let next = &second["data"]["organization"]["teams"];
    assert_eq!(
        next["nodes"],
        json!([{ "databaseId": 6199, "ancestors": {"nodes": []} }])
    );
    assert_eq!(next["pageInfo"]["hasNextPage"], false);

    let denied = Graphql::new(GoGithub::enterprise(&server).with_token(METADATA_TOKEN));
    let denied = denied
        .query(doc, &json!({"org": "acme", "login": "bob"}))
        .await
        .json();
    assert_eq!(denied["errors"][0]["type"], "FORBIDDEN");
}

#[tokio::test]
async fn access_document_preserves_effective_permissions_and_isolates_hidden_aliases() {
    let server = start(setup()).await;
    let api = Graphql::new(GoGithub::enterprise(&server).with_token(MEMBERS_TOKEN));
    let doc = include_str!("../../../docs/inventory/graphql/access.graphql");
    let want = json!([{ "permission": "MAINTAIN", "node": { "databaseId": fixture::BOB_ID } }]);
    let response = api
        .query(
            doc,
            &json!({
                "owner": "acme", "login": "BOB", "n0": "public", "n1": "private",
            }),
        )
        .await
        .json();
    assert_eq!(
        response["data"]["r0"]["collaborators"]["edges"], want,
        "{response}"
    );
    assert!(response["data"]["r1"].is_null());
    assert_eq!(response["errors"][0]["type"], "NOT_FOUND");
    assert_eq!(response["errors"][0]["path"], json!(["r1"]));

    for count in [1, 100] {
        let mut selections = String::new();
        for at in 0..count {
            let _ = write!(
                selections,
                "r{at}: repository(owner: \"acme\", name: \"public\") {{ collaborators(login: $login, first: 1) {{ edges {{ permission node {{ databaseId }} }} }} }}"
            );
        }
        let query = format!("query($login: String!) {{ {selections} }}");
        let answer = api.query(&query, &json!({"login": "bob"})).await.json();
        assert!(answer.get("errors").is_none(), "{answer}");
        let results = answer["data"]
            .as_object()
            .unwrap_or_else(|| panic!("{answer}"));
        assert_eq!(results.len(), count);
        assert!(
            results
                .values()
                .all(|value| value["collaborators"]["edges"] == want)
        );
    }
    let answer = api
        .query(
            doc,
            &json!({
                "owner": "acme", "login": "carol", "n0": "public", "n1": "public",
            }),
        )
        .await
        .json();
    assert!(answer.get("errors").is_none(), "{answer}");
    assert_eq!(answer["data"]["r0"]["collaborators"]["edges"], json!([]));
}

async fn admin_status(
    server: &TestServer,
    method: Method,
    path: &str,
    body: Option<&Value>,
    status: u16,
) {
    let answer = server.admin(method, path, body).await;
    assert_eq!(answer.status.as_u16(), status, "{path}: {:?}", answer.body);
}

#[tokio::test]
async fn deleting_a_parent_team_sends_team_deleted_for_it_and_each_child_team() {
    // Given root with a child team, and the child with a grandchild
    let server = start(setup().entry(json!({
        "kind": "team", "org": "acme", "name": "Grandchild", "parent": "child",
    })))
    .await;
    admin_status(&server, Method::POST, "hooks/drain", None, 204).await;
    server.receiver.received().await;

    // When root is deleted
    admin_status(&server, Method::DELETE, "team/acme/root", None, 204).await;

    // Then each of the three teams gets one team deleted delivery
    admin_status(&server, Method::POST, "hooks/drain", None, 204).await;
    let mut deleted: Vec<(String, Value)> = server
        .receiver
        .received()
        .await
        .iter()
        .filter(|d| d.header("x-github-event") == "team" && d.json()["action"] == "deleted")
        .map(|d| {
            let team = &d.json()["team"];
            (
                team["slug"].as_str().unwrap_or_default().to_owned(),
                team["parent"]["slug"].clone(),
            )
        })
        .collect();
    deleted.sort_by(|a, b| a.0.cmp(&b.0));
    // And each child names the parent it had
    assert_eq!(
        deleted,
        [
            ("child".to_owned(), json!("root")),
            ("grandchild".to_owned(), json!("child")),
            ("root".to_owned(), Value::Null),
        ]
    );
}

#[tokio::test]
async fn admin_transitions_publish_the_four_event_families_once() {
    let server = start(setup()).await;
    let client = GoGithub::new(&server).with_token(MEMBERS_TOKEN);
    admin_status(&server, Method::POST, "hooks/drain", None, 204).await;
    assert!(server.receiver.received().await.is_empty());
    let org_member = "org/acme/members/carol";
    let team = "team/acme/temp";
    let team_member = "team/acme/temp/members/carol";
    let team_repo = "team/acme/temp/repos/acme/public";
    let collaborator = "repository/acme/public/collaborators/carol";
    for (method, path, input, code) in [
        (
            Method::PUT,
            org_member,
            Some(json!({"state": "pending"})),
            204,
        ),
        (
            Method::PUT,
            org_member,
            Some(json!({"state": "active"})),
            204,
        ),
        (
            Method::POST,
            "team",
            Some(json!({"org": "acme", "name": "Temp", "parent": "root"})),
            201,
        ),
        (
            Method::PATCH,
            team,
            Some(json!({"name": "Temp renaming", "slug": "temp"})),
            204,
        ),
        (
            Method::PUT,
            team_member,
            Some(json!({"role": "maintainer"})),
            204,
        ),
        (
            Method::PUT,
            team_member,
            Some(json!({"role": "maintainer"})),
            204,
        ),
        (
            Method::PUT,
            team_repo,
            Some(json!({"permission": "read"})),
            204,
        ),
        (
            Method::PUT,
            collaborator,
            Some(json!({"permission": "triage"})),
            204,
        ),
        (
            Method::PUT,
            collaborator,
            Some(json!({"permission": "write"})),
            204,
        ),
        (Method::DELETE, collaborator, None, 204),
        (Method::DELETE, team_repo, None, 204),
        (Method::DELETE, org_member, None, 204),
        (Method::DELETE, team, None, 204),
    ] {
        admin_status(&server, method, path, input.as_ref(), code).await;
    }
    assert_status(&client, "orgs/acme/members/carol", 404).await;
    assert_status(&client, "orgs/acme/teams/temp", 404).await;
    admin_status(&server, Method::POST, "hooks/drain", None, 204).await;
    assert_webhooks(&server).await;
}

async fn assert_webhooks(server: &TestServer) {
    let mut events = BTreeMap::new();
    for delivery in server.receiver.received().await {
        let event = delivery.header("x-github-event").to_owned();
        let payload = delivery.json();
        let action = payload["action"]
            .as_str()
            .unwrap_or_else(|| panic!("{payload}"));
        assert_eq!(delivery.header("x-github-delivery").len(), 36);
        assert!(delivery.signed_with(fixture::WEBHOOK_SECRET));
        validate::webhook_payload(&event, Some(action), &payload)
            .unwrap_or_else(|errors| panic!("{event}.{action}: {errors:?}"));
        assert_eq!(payload["installation"]["id"], fixture::INSTALLATION_ID);
        assert_eq!(payload["sender"]["id"], fixture::ALICE_ID);
        let duplicate = events.insert(format!("{event}.{action}"), payload);
        assert!(duplicate.is_none());
    }
    assert_eq!(events.len(), 13);
    assert_eq!(
        events["organization.member_invited"]["invitation"]["login"],
        "carol"
    );
    for action in ["member_added", "member_removed"] {
        assert_eq!(
            events[&format!("organization.{action}")]["membership"]["user"]["id"],
            fixture::CAROL_ID
        );
    }
    for event in [
        "membership.added",
        "membership.removed",
        "member.added",
        "member.edited",
        "member.removed",
    ] {
        assert_eq!(events[event]["member"]["id"], fixture::CAROL_ID);
    }
    assert_eq!(events["team.edited"]["changes"]["name"]["from"], "Temp");
    assert_eq!(
        events["team.added_to_repository"]["repository"]["id"],
        fixture::PUBLIC_REPO_ID
    );
    assert_eq!(
        events["team.removed_from_repository"]["repository"]["id"],
        fixture::PUBLIC_REPO_ID
    );
    assert_eq!(events["team.created"]["team"]["parent"]["id"], ROOT);
}

#[tokio::test]
async fn failed_admin_changes_keep_team_state_and_the_outbox_unchanged() {
    let server = start(setup()).await;
    let client = GoGithub::new(&server).with_token(MEMBERS_TOKEN);
    let cycle = json!({"parent": "child"});
    let outsider = json!({"org": "acme", "name": "Broken", "members": [{"login": "carol"}]});
    admin_status(&server, Method::PATCH, "team/acme/root", Some(&cycle), 422).await;
    admin_status(&server, Method::POST, "team", Some(&outsider), 422).await;
    assert_status(&client, "orgs/acme/teams/broken", 404).await;
    let root = client.get("orgs/acme/teams/root").await.json();
    assert_eq!(root["id"], ROOT);
    assert!(root["parent"].is_null());
    admin_status(&server, Method::POST, "hooks/drain", None, 204).await;
    assert!(server.receiver.received().await.is_empty());
}
