#![expect(
    clippy::unwrap_used,
    reason = "tests fail immediately when an answer and its contract differ"
)]

use http::{HeaderMap, HeaderValue, Method, StatusCode};
use hubdouble_testkit::fixture::{self, ALICE, ALICE_TOKEN};
use hubdouble_testkit::{GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const REPO: &str = "repos/acme/public";

async fn whole_server(extra: &[Value]) -> TestServer {
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for entry in fixture::seed().into_iter().chain(extra.iter().cloned()) {
        builder = builder.entry(entry);
    }
    builder.start().await
}

async fn tip(client: &GoGithub, branch: &str) -> String {
    let answer = client.get(&format!("{REPO}/git/ref/heads/{branch}")).await;
    assert_eq!(answer.status, StatusCode::OK);
    answer.json()["object"]["sha"].as_str().unwrap().to_owned()
}

fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(*name, HeaderValue::from_static(value));
    }
    map
}

#[tokio::test]
async fn graphql_ignores_the_rest_api_version_and_answers_the_v4_media_type() {
    // Given
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .start()
        .await;
    let alice = Graphql::new(GoGithub::new(&server).with_token(ALICE_TOKEN));
    let query = "{ viewer { login } }";

    // When the client sends a version GitHub does not publish, as live gh api graphql did
    let unknown = alice
        .query_headers(
            query,
            &json!({}),
            headers(&[("x-github-api-version", "1999-01-01"), ("accept", "*/*")]),
        )
        .await;
    let plain_json = alice
        .query_headers(
            query,
            &json!({}),
            headers(&[("accept", "application/json")]),
        )
        .await;
    let go_github = alice.query(query, &json!({})).await;

    // Then GitHub answers 200 with the v4 media type, no selected version and its scope headers
    assert_eq!(unknown.status, StatusCode::OK);
    assert_eq!(unknown.json()["data"]["viewer"]["login"], ALICE);
    assert_eq!(
        unknown.header("x-github-media-type"),
        "github.v4; format=json"
    );
    assert!(
        !unknown
            .headers
            .contains_key("x-github-api-version-selected")
    );
    assert_eq!(unknown.header("x-ratelimit-resource"), "graphql");
    assert_eq!(unknown.header("x-accepted-oauth-scopes"), "repo");
    assert_ne!(unknown.header("x-oauth-scopes"), "");
    assert_ne!(unknown.header("x-github-request-id"), "");
    assert_eq!(plain_json.header("x-github-media-type"), "github.v4");
    assert_eq!(
        go_github.header("x-github-media-type"),
        "github.v3; format=json"
    );
    assert!(
        !go_github
            .headers
            .contains_key("x-github-api-version-selected")
    );
}

fn accepting(value: &'static str) -> HeaderMap {
    headers(&[("accept", value)])
}

fn unsupported(answer: &hubdouble_testkit::Answer, ranges: &str) {
    assert_eq!(
        answer.status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "{ranges}"
    );
    assert_eq!(answer.header("x-github-media-type"), "unknown");
    assert_eq!(
        answer.json(),
        json!({"message": format!("Unsupported 'Accept' header: '{ranges}'. Must accept 'application/json'."),
            "documentation_url": "https://docs.github.com/v3/media", "status": "415"})
    );
}

#[tokio::test]
async fn an_accept_header_without_json_answers_github_415() {
    // Given an anonymous caller, alice, and a token that GitHub does not know
    let server = whole_server(&[]).await;
    let anonymous = GoGithub::new(&server);
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let stranger = GoGithub::new(&server).with_token("ghp_000000000000000000000000000000000bad");
    let get = |client: &GoGithub, path: &'static str, accept: &'static str| {
        let client = client.clone();
        async move {
            client
                .call_headers(Method::GET, path, accepting(accept))
                .await
        }
    };

    // When each reads with Accept headers that the live API refused or served on 2026-11-24
    let html = get(&anonymous, REPO, "text/html").await;
    let ordered = get(&anonymous, REPO, "text/html;q=0.5, image/png").await;
    let joined = get(&alice, REPO, "text/html,application/xhtml+xml").await;
    let missing = get(&anonymous, "repos/acme/absent", "text/html").await;
    let bad_token = get(&stranger, REPO, "text/html").await;
    let user = get(&anonymous, "user", "text/html").await;
    let write = anonymous
        .call_headers(
            Method::POST,
            &format!("{REPO}/issues"),
            accepting("text/html"),
        )
        .await;
    let graphql = Graphql::new(alice.clone())
        .query_headers("{ viewer { login } }", &json!({}), accepting("text/html"))
        .await;
    let answered = [
        ("text/*", "github.v3"),
        ("application/*", "github.v3"),
        ("text/html, */*", "unknown, github.v3"),
        ("application/json, text/html", "github.v3"),
        (
            "text/html, application/vnd.github.raw",
            "unknown, github.v3; param=raw",
        ),
    ];

    // Then only a header without a range GitHub serves is refused, after credentials and sign-in
    unsupported(&html, "text/html");
    unsupported(&ordered, "image/png, text/html;q=0.5");
    unsupported(&joined, "text/html, application/xhtml+xml");
    unsupported(&missing, "text/html");
    unsupported(&graphql, "text/html");
    assert_eq!(bad_token.status, StatusCode::UNAUTHORIZED);
    assert_eq!(user.status, StatusCode::UNAUTHORIZED);
    assert_eq!(write.status, StatusCode::UNAUTHORIZED);
    for (accept, media) in answered {
        let answer = get(&anonymous, REPO, accept).await;
        assert_eq!(answer.status, StatusCode::OK, "{accept}");
        assert_eq!(answer.header("x-github-media-type"), media, "{accept}");
    }
}

#[tokio::test]
async fn anonymous_graphql_answers_github_rate_limit_403() {
    // Given an anonymous caller of the GraphQL endpoint
    let server = whole_server(&[]).await;
    let anonymous = GoGithub::new(&server);
    let body = br#"{"query":"{ viewer { login } }"}"#.to_vec();

    // When it posts a query, posts a body that is not JSON with an HTML Accept, and reads it
    let posted = anonymous.call_bytes(Method::POST, "graphql", body).await;
    let broken = anonymous
        .call_json_headers(
            Method::POST,
            "graphql",
            b"nope".to_vec(),
            accepting("text/html"),
        )
        .await;
    let read = anonymous.call(Method::GET, "graphql").await;

    // Then each is GitHub's anonymous GraphQL limit of 0, with its body and headers
    for answer in [&posted, &broken, &read] {
        assert_eq!(answer.status, StatusCode::FORBIDDEN);
        assert_eq!(
            answer.json(),
            json!({"message": "API rate limit exceeded for 127.0.0.1. (But here's the good news: Authenticated requests get a higher rate limit. Check out the documentation for more details.)",
                "documentation_url": "https://docs.github.com/rest/overview/resources-in-the-rest-api#rate-limiting"})
        );
        assert_eq!(
            answer.header("x-github-media-type"),
            "github.v3; format=json"
        );
        for name in [
            "x-ratelimit-limit",
            "x-ratelimit-remaining",
            "x-ratelimit-used",
        ] {
            assert_eq!(answer.header(name), "0", "{name}");
        }
        assert_eq!(answer.header("x-ratelimit-resource"), "graphql");
        assert_ne!(answer.header("x-ratelimit-reset"), "");
    }
}

#[tokio::test]
async fn an_authenticated_graphql_read_answers_the_introspection_of_the_served_schema() {
    // Given a user of the GraphQL endpoint
    let server = whole_server(&[]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);

    // When it reads the endpoint, reads it again with the tag and reads it with an HTML Accept
    let read = alice.call(Method::GET, "graphql").await;
    let mut tagged = HeaderMap::new();
    tagged.insert(
        http::header::IF_NONE_MATCH,
        HeaderValue::from_str(read.header("etag")).unwrap(),
    );
    let again = alice.call_headers(Method::GET, "graphql", tagged).await;
    let html = alice
        .call_headers(Method::GET, "graphql", accepting("text/html"))
        .await;

    // Then it is GitHub's introspection answer, with GitHub's headers
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(
        read.header("content-type"),
        "application/json; charset=utf-8"
    );
    assert_eq!(read.header("x-github-media-type"), "github.v3; format=json");
    assert_eq!(read.header("x-ratelimit-resource"), "graphql");
    assert_eq!(read.header("x-accepted-oauth-scopes"), "repo");
    assert_eq!(
        read.header("cache-control"),
        "private, max-age=60, s-maxage=60"
    );
    assert!(read.header("etag").starts_with("W/\""));
    let text = std::str::from_utf8(&read.body).unwrap();
    assert!(text.starts_with(
        r#"{"data":{"__schema":{"queryType":{"name":"Query"},"mutationType":{"name":"Mutation"},"subscriptionType":null,"types":[{"kind":"#
    ));
    assert!(text.contains(r#"{"kind":"SCALAR","name":"String","description":"Represents textual data as UTF-8 character sequences. This type is most often used by GraphQL to represent free-form human-readable text.","fields":null,"inputFields":null,"interfaces":null,"enumValues":null,"possibleTypes":null}"#));
    let schema = &read.json()["data"]["__schema"];
    let names: Vec<&str> = schema["types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
    let pinned = hubdouble_gql::Schema::load().unwrap();
    assert_eq!(names.len(), pinned.type_count());
    let repository = schema["types"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "Repository")
        .unwrap();
    let fields: Vec<&str> = repository["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    let mut sorted = fields.clone();
    sorted.sort_unstable();
    assert_eq!(fields, sorted);
    let directives: Vec<&str> = schema["directives"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        directives,
        ["deprecated", "include", "oneOf", "skip", "specifiedBy"]
    );

    // Then the tagged read is not modified and the HTML read is refused
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    assert_eq!(html.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

const LIVE_INTROSPECTION_QUERY: &str = r#"query { __typename t: __type(name: "Nope") { name } input: __type(name: "RepositoryRulesetBypassActorInput") { ...T inputFields { name } all: inputFields(includeDeprecated: true) { name isDeprecated deprecationReason defaultValue type { kind name ofType { name } } } } __schema { description subscriptionType { name } queryType { __typename kind isOneOf ofType { name } } directives { name isRepeatable onField args { name defaultValue } } } enum: __type(name: "IssueState") { enumValues(includeDeprecated: true) { name isDeprecated } kind @include(if: false) } } fragment T on __Type { name kind isOneOf specifiedByURL }"#;

const LIVE_INTROSPECTION_ANSWER: &str = r#"{"data":{"__typename":"Query","t":null,"input":{"name":"RepositoryRulesetBypassActorInput","kind":"INPUT_OBJECT","isOneOf":false,"specifiedByURL":null,"inputFields":[{"name":"actorId"},{"name":"repositoryRoleDatabaseId"},{"name":"organizationAdmin"},{"name":"enterpriseOwner"},{"name":"enterpriseRoleDatabaseId"},{"name":"deployKey"},{"name":"bypassMode"}],"all":[{"name":"actorId","isDeprecated":false,"deprecationReason":null,"defaultValue":null,"type":{"kind":"SCALAR","name":"ID","ofType":null}},{"name":"repositoryRoleDatabaseId","isDeprecated":false,"deprecationReason":null,"defaultValue":null,"type":{"kind":"SCALAR","name":"Int","ofType":null}},{"name":"organizationAdmin","isDeprecated":false,"deprecationReason":null,"defaultValue":null,"type":{"kind":"SCALAR","name":"Boolean","ofType":null}},{"name":"enterpriseOwner","isDeprecated":false,"deprecationReason":null,"defaultValue":null,"type":{"kind":"SCALAR","name":"Boolean","ofType":null}},{"name":"enterpriseRole","isDeprecated":true,"deprecationReason":"Use enterpriseRoleDatabaseId.","defaultValue":null,"type":{"kind":"SCALAR","name":"Boolean","ofType":null}},{"name":"enterpriseRoleDatabaseId","isDeprecated":false,"deprecationReason":null,"defaultValue":null,"type":{"kind":"SCALAR","name":"BigInt","ofType":null}},{"name":"deployKey","isDeprecated":false,"deprecationReason":null,"defaultValue":null,"type":{"kind":"SCALAR","name":"Boolean","ofType":null}},{"name":"bypassMode","isDeprecated":false,"deprecationReason":null,"defaultValue":null,"type":{"kind":"NON_NULL","name":null,"ofType":{"name":"RepositoryRulesetBypassActorBypassMode"}}}]},"__schema":{"description":null,"subscriptionType":null,"queryType":{"__typename":"__Type","kind":"OBJECT","isOneOf":false,"ofType":null},"directives":[{"name":"deprecated","isRepeatable":false,"onField":false,"args":[{"name":"reason","defaultValue":"\"No longer supported\""}]},{"name":"include","isRepeatable":false,"onField":true,"args":[{"name":"if","defaultValue":null}]},{"name":"oneOf","isRepeatable":false,"onField":false,"args":[]},{"name":"skip","isRepeatable":false,"onField":true,"args":[{"name":"if","defaultValue":null}]},{"name":"specifiedBy","isRepeatable":false,"onField":false,"args":[{"name":"url","defaultValue":null}]}]},"enum":{"enumValues":[{"name":"OPEN","isDeprecated":false},{"name":"CLOSED","isDeprecated":false}]}}}"#;

const CLASSIC_INTROSPECTION_QUERY: &str = "query IntrospectionQuery { __schema { queryType { name } mutationType { name } subscriptionType { name } types { ...FullType } directives { name description locations args { ...InputValue } } } } fragment FullType on __Type { kind name description fields(includeDeprecated: true) { name description args { ...InputValue } type { ...TypeRef } isDeprecated deprecationReason } inputFields { ...InputValue } interfaces { ...TypeRef } enumValues(includeDeprecated: true) { name description isDeprecated deprecationReason } possibleTypes { ...TypeRef } } fragment InputValue on __InputValue { name description type { ...TypeRef } defaultValue } fragment TypeRef on __Type { kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name } } } } } } } }";

const AUTHENTICATED_VARY: &str =
    "Accept, Authorization, Cookie, X-GitHub-OTP,Accept-Encoding, Accept, X-Requested-With";
const ANONYMOUS_VARY: &str = "Accept,Accept-Encoding, Accept, X-Requested-With";
const PLAIN_VARY: &str = "Accept-Encoding, Accept, X-Requested-With";

fn tagged(tag: &str, extra: &[(&'static str, &'static str)]) -> HeaderMap {
    let mut map = headers(extra);
    map.insert(
        http::header::IF_NONE_MATCH,
        HeaderValue::from_str(tag).unwrap(),
    );
    map
}

#[tokio::test]
async fn rest_reads_answer_githubs_cache_vary_and_etag_headers() {
    // Given a user and an anonymous caller of a public repository
    let server = whole_server(&[]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let anonymous = GoGithub::new(&server);
    let plain = headers(&[("accept-encoding", "identity")]);

    // When each reads it with and without gzip and reads it again with the tag
    let read = alice.get(REPO).await;
    let unzipped = alice.call_headers(Method::GET, REPO, plain.clone()).await;
    let again = alice
        .call_headers(Method::GET, REPO, tagged(read.header("etag"), &[]))
        .await;
    let public = anonymous.get(REPO).await;
    let public_again = anonymous
        .call_headers(Method::GET, REPO, tagged(public.header("etag"), &[]))
        .await;
    let file = alice.get(&format!("{REPO}/contents/README.md")).await;
    let sha = file.json()["sha"].as_str().unwrap().to_owned();
    let blob = alice.get(&format!("{REPO}/git/blobs/{sha}")).await;
    let missing = alice.get(&format!("{REPO}/issues/999999")).await;
    let posted = alice
        .call_bytes(
            Method::POST,
            "graphql",
            br#"{"query":"{ viewer { login } }"}"#.to_vec(),
        )
        .await;

    // Then a user's read is private and varies on the credentials, and a gzip client gets the
    // weak form of the tag that a plain client gets
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(
        read.header("cache-control"),
        "private, max-age=60, s-maxage=60"
    );
    assert_eq!(read.header("vary"), AUTHENTICATED_VARY);
    assert_eq!(unzipped.header("content-encoding"), "");
    let strong = unzipped.header("etag");
    assert!(strong.starts_with('"'), "{strong}");
    assert_eq!(read.header("etag"), format!("W/{strong}"));

    // Then the user's 304 keeps the cache headers and the strong tag and drops the content type
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    assert_eq!(
        again.header("cache-control"),
        "private, max-age=60, s-maxage=60"
    );
    assert_eq!(again.header("vary"), AUTHENTICATED_VARY);
    assert_eq!(again.header("etag"), strong);
    assert_eq!(again.header("content-type"), "");

    // Then an anonymous read is public, and its 304 keeps the content type
    assert_eq!(
        public.header("cache-control"),
        "public, max-age=60, s-maxage=60"
    );
    assert_eq!(public.header("vary"), ANONYMOUS_VARY);
    assert_eq!(public_again.status, StatusCode::NOT_MODIFIED);
    assert_eq!(
        public_again.header("cache-control"),
        "public, max-age=60, s-maxage=60"
    );
    assert_eq!(
        public_again.header("content-type"),
        "application/json; charset=utf-8"
    );

    // Then a file read is not compressed, varies with GitHub's spacing and is tagged with its
    // blob SHA, and a blob read is cached for a day
    assert_eq!(file.header("etag"), format!("\"{sha}\""));
    assert_eq!(
        file.header("vary"),
        "Accept, Authorization, Cookie, X-GitHub-OTP, Accept-Encoding, Accept, X-Requested-With"
    );
    assert_eq!(
        blob.header("cache-control"),
        "private, max-age=86400, s-maxage=86400"
    );

    // Then an error and a GraphQL post have no cache header and vary on the encoding only
    for answer in [&missing, &posted] {
        assert_eq!(answer.header("cache-control"), "");
        assert_eq!(answer.header("vary"), PLAIN_VARY);
    }
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rest_reads_answer_last_modified_and_if_modified_since_as_github_does() {
    // Given a user of a seeded repository
    let server = whole_server(&[]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let since = |date: &'static str, extra: &[(&'static str, &'static str)]| {
        let mut map = headers(extra);
        map.insert(
            http::header::IF_MODIFIED_SINCE,
            HeaderValue::from_static(date),
        );
        map
    };

    // When the user reads the repository, its contents and commits, and reads the repository
    // again with If-Modified-Since
    let read = alice.get(REPO).await;
    let file = alice.get(&format!("{REPO}/contents/README.md")).await;
    let root = alice.get(&format!("{REPO}/contents/")).await;
    let commits = alice.get(&format!("{REPO}/commits")).await;
    let modified = read.header("last-modified").to_owned();
    let at_modified = alice
        .call_headers(
            Method::GET,
            REPO,
            since("Thu, 01 Jan 2026 00:00:00 GMT", &[]),
        )
        .await;
    let rfc850 = alice
        .call_headers(
            Method::GET,
            REPO,
            since("Thursday, 01-Jan-26 00:00:00 GMT", &[]),
        )
        .await;
    let before = alice
        .call_headers(
            Method::GET,
            REPO,
            since("Wed, 31 Dec 2025 23:59:59 GMT", &[]),
        )
        .await;
    let other_tag = alice
        .call_headers(
            Method::GET,
            REPO,
            since(
                "Thu, 01 Jan 2026 00:00:00 GMT",
                &[("if-none-match", "\"other\"")],
            ),
        )
        .await;

    // Then each read sends the time of what it shows: the repository's update, the commit of
    // the ref for a file and the first listed commit for a list
    assert_eq!(read.json()["updated_at"], "2026-01-01T00:00:00Z");
    assert_eq!(modified, "Thu, 01 Jan 2026 00:00:00 GMT");
    assert_eq!(root.header("last-modified"), modified);
    assert_eq!(
        commits.json()[0]["commit"]["committer"]["date"],
        "2026-01-01T00:00:00Z"
    );
    assert_eq!(commits.header("last-modified"), modified);
    assert_eq!(file.header("last-modified"), modified);

    // Then a time at or after it answers 304 with the headers of the read, in either date form
    for answer in [&at_modified, &rfc850] {
        assert_eq!(answer.status, StatusCode::NOT_MODIFIED);
        assert_eq!(answer.header("last-modified"), modified);
        assert!(answer.header("etag").starts_with('"'));
    }

    // Then an earlier time, or a tag that does not match, answers the full read
    assert_eq!(before.status, StatusCode::OK);
    assert_eq!(other_tag.status, StatusCode::OK);
}

#[tokio::test]
async fn a_graphql_answer_keeps_the_order_of_its_selection() {
    // Given a user of the GraphQL endpoint
    let server = whole_server(&[]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);

    // When it selects fields in an order that is not sorted
    let body = br#"{"query":"{ viewer { login __typename } b: __typename a: __typename }"}"#;
    let answer = alice
        .call_bytes(Method::POST, "graphql", body.to_vec())
        .await;

    // Then the answer keeps that order, as the GraphQL spec and live GitHub do
    assert_eq!(
        std::str::from_utf8(&answer.body).unwrap(),
        format!(
            r#"{{"data":{{"viewer":{{"login":"{ALICE}","__typename":"User"}},"b":"Query","a":"Query"}}}}"#
        )
    );
}

#[tokio::test]
async fn a_posted_introspection_query_answers_as_live_github_does() {
    // Given a user of the GraphQL endpoint
    let server = whole_server(&[]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let query = |q: &str| serde_json::to_vec(&json!({ "query": q })).unwrap();

    // When it posts a mixed introspection selection, posts the query that GET /graphql answers
    // and reads GET /graphql
    let mixed = alice
        .call_bytes(Method::POST, "graphql", query(LIVE_INTROSPECTION_QUERY))
        .await;
    let classic = alice
        .call_bytes(Method::POST, "graphql", query(CLASSIC_INTROSPECTION_QUERY))
        .await;
    let read = alice.call(Method::GET, "graphql").await;

    // Then the mixed selection is the live answer byte for byte, and the posted classic query
    // is the GET answer byte for byte
    assert_eq!(mixed.status, StatusCode::OK);
    assert_eq!(
        std::str::from_utf8(&mixed.body).unwrap(),
        LIVE_INTROSPECTION_ANSWER
    );
    assert_eq!(classic.status, StatusCode::OK);
    assert!(
        classic.body == read.body,
        "the POST and GET introspection answers differ"
    );
}

#[tokio::test]
async fn a_commit_status_is_validated_as_github_validates_a_status() {
    // Given
    let server = whole_server(&[]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let head = tip(&alice, "main").await;
    let path = format!("{REPO}/statuses/{head}");
    let longest: String = "é".repeat(140);
    let too_long: String = "a".repeat(141);

    // When the client writes statuses that GitHub refuses, as the samples in the plan show
    let long = alice
        .post(&path, &json!({"state": "success", "description": too_long}))
        .await;
    let relative = alice
        .post(
            &path,
            &json!({"state": "success", "target_url": "key-concepts/"}),
        )
        .await;
    let no_state = alice.post(&path, &json!({"context": "ci"})).await;
    let emoji = alice
        .post(&path, &json!({"state": "success", "context": "ci 🚀"}))
        .await;
    let listed = alice.get(&format!("{REPO}/commits/{head}/statuses")).await;
    let fits = alice
        .post(
            &path,
            &json!({"state": "success", "description": longest, "target_url": "HTTPS://ci.example/1"}),
        )
        .await;

    // Then each refusal is a 422 of the Status model and writes nothing, and a model
    // validation has no documentation_url, as the live answers quoted in the plan
    for answer in [&long, &relative] {
        assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
        let body = answer.json();
        assert!(body["errors"].is_string(), "{body}");
        assert_eq!(body["status"], "422", "{body}");
        assert_eq!(body.get("documentation_url"), None, "{body}");
    }
    for (answer, field) in [(&no_state, "state"), (&emoji, "context")] {
        assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
        let error = &answer.json()["errors"][0];
        assert_eq!(error["resource"], "Status");
        assert_eq!(error["code"], "custom");
        assert_eq!(error["field"], field);
    }
    assert_eq!(listed.json(), json!([]));
    assert_eq!(fits.status, StatusCode::CREATED);
}

#[tokio::test]
async fn status_contexts_match_without_case_and_keep_their_spelling() {
    // Given a ruleset on main that requires the context CI/Build
    let rules = json!({"kind": "ruleset", "repository": "acme/public", "name": "rules",
        "target": "branch", "enforcement": "active", "bypass_actors": [],
        "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
        "rules": [{"type": "required_status_checks", "parameters": {
            "strict_required_status_checks_policy": false,
            "required_status_checks": [{"context": "CI/Build"}]}}]});
    let server = whole_server(&[rules]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let head = tip(&alice, "feature").await;
    let path = format!("{REPO}/statuses/{head}");

    // When a pending CI/Build is followed by a successful ci/build on the head
    for (state, context) in [("pending", "CI/Build"), ("success", "ci/build")] {
        let made = alice
            .post(&path, &json!({"state": state, "context": context}))
            .await;
        assert_eq!(made.status, StatusCode::CREATED);
    }
    let combined = alice.get(&format!("{REPO}/commits/{head}/status")).await;
    let merged = alice
        .post(
            &format!("{REPO}/merges"),
            &json!({"base": "main", "head": "feature"}),
        )
        .await;

    // Then both are one context, shown as the newest status spelled it, and it passes the rule
    let combined = combined.json();
    assert_eq!(combined["total_count"], 1);
    assert_eq!(combined["state"], "success");
    assert_eq!(combined["statuses"][0]["context"], "ci/build");
    assert_eq!(merged.status, StatusCode::CREATED, "{}", merged.json());
}

#[tokio::test]
async fn a_sha_and_context_take_at_most_1000_statuses() {
    // Given 999 statuses of the context ci on the tip of main
    let seeded: Vec<Value> = (0..999)
        .map(|_| {
            json!({"kind": "status", "repository": "acme/public", "sha": "main",
                "creator": "alice", "state": "pending", "context": "ci"})
        })
        .collect();
    let server = whole_server(&seeded).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let head = tip(&alice, "main").await;
    let path = format!("{REPO}/statuses/{head}");

    // When the client writes the 1000th, a 1001st spelled CI, and one of another context
    let last = alice
        .post(&path, &json!({"state": "success", "context": "ci"}))
        .await;
    let past = alice
        .post(&path, &json!({"state": "failure", "context": "CI"}))
        .await;
    let other = alice
        .post(&path, &json!({"state": "success", "context": "lint"}))
        .await;
    let combined = alice.get(&format!("{REPO}/commits/{head}/status")).await;

    // Then the 1001st is GitHub's validation error and does not change the combined state
    assert_eq!(last.status, StatusCode::CREATED);
    assert_eq!(past.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        past.json(),
        json!({"message": "Validation Failed",
            "errors": "Validation failed: This SHA and context has reached the maximum number of statuses.",
            "documentation_url": "https://docs.github.com/rest/commits/statuses#create-a-commit-status",
            "status": "422"})
    );
    assert_eq!(other.status, StatusCode::CREATED);
    assert_eq!(combined.json()["state"], "success");
}

const NO_SCOPE_PAT: &str = "ghp_0000000000000000000000000000000000c0";
const NO_SCOPE_OAUTH: &str = "gho_0000000000000000000000000000000000c1";

#[tokio::test]
async fn a_classic_token_without_scopes_reads_public_repositories_only() {
    // Given carol's classic personal and OAuth App tokens without scopes
    let pull = json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
        "title": "feature", "head": "feature", "base": "main"});
    let tokens: Vec<Value> = [NO_SCOPE_PAT, NO_SCOPE_OAUTH]
        .iter()
        .map(|t| json!({"kind": "token", "user": "carol", "token": t, "scopes": []}))
        .collect();
    let server = whole_server(&[vec![pull], tokens].concat()).await;
    let anonymous = GoGithub::new(&server);

    for token in [NO_SCOPE_PAT, NO_SCOPE_OAUTH] {
        let carol = GoGithub::new(&server).with_token(token);

        // When the token reads public reviews and runs, a private repository, and writes
        let reads = [
            "repos/acme/public/pulls/1/reviews",
            "repos/acme/public/actions/runs",
        ];
        let private = carol.get("repos/acme/private/actions/runs").await;
        let write = carol
            .post(
                "repos/acme/public/issues/1/comments",
                &json!({"body": "hi"}),
            )
            .await;

        // Then the public reads answer as for anyone, as GitHub's scope docs say
        for path in reads {
            assert_eq!(anonymous.get(path).await.status, StatusCode::OK, "{path}");
            assert_eq!(
                carol.get(path).await.status,
                StatusCode::OK,
                "{token} {path}"
            );
        }
        assert_eq!(private.status, StatusCode::NOT_FOUND, "{token}");
        assert_eq!(write.status, StatusCode::FORBIDDEN, "{token}");
        assert_eq!(
            write.header("x-accepted-oauth-scopes"),
            "repo, public_repo",
            "{token}"
        );
    }
}

#[tokio::test]
async fn an_issue_locks_with_each_reason_and_unlocks_as_github_does() {
    // Given an issue of acme/public, alice who pushes to it, and carol who reads it
    let server = whole_server(&[]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let carol = GoGithub::new(&server).with_token(fixture::CAROL_TOKEN);
    let opened = alice
        .post(&format!("{REPO}/issues"), &json!({"title": "talk"}))
        .await;
    assert_eq!(opened.status, StatusCode::CREATED);
    let issue = format!("{REPO}/issues/{}", opened.json()["number"]);
    let lock = format!("{issue}/lock");
    let query = r#"query($n: Int!) { repository(owner: "acme", name: "public") {
        issue(number: $n) { activeLockReason } } }"#;
    let vars = json!({"n": opened.json()["number"]});
    let graphql = Graphql::new(alice.clone());
    let mut actions = Vec::new();

    for (reason, value) in [
        (Some("off-topic"), "OFF_TOPIC"),
        (Some("too heated"), "TOO_HEATED"),
        (Some("resolved"), "RESOLVED"),
        (Some("spam"), "SPAM"),
        (None, ""),
    ] {
        // When alice locks it with the reason, or with no body, and unlocks it
        let locked = match reason {
            Some(r) => {
                alice
                    .call_json(Method::PUT, &lock, &json!({"lock_reason": r}))
                    .await
            }
            None => alice.call(Method::PUT, &lock).await,
        };
        let read = alice.get(&issue).await.json();
        let gql = graphql.query(query, &vars).await.json();
        let unlocked = alice.call(Method::DELETE, &lock).await;
        let after = alice.get(&issue).await.json();
        let gql_after = graphql.query(query, &vars).await.json();

        // Then REST and GraphQL answer the reason while it is locked, and none after
        assert_eq!(locked.status, StatusCode::NO_CONTENT, "{reason:?}");
        assert_eq!(read["locked"], true);
        assert_eq!(read["active_lock_reason"], json!(reason));
        let active = &gql["data"]["repository"]["issue"]["activeLockReason"];
        match reason {
            Some(_) => assert_eq!(active, value, "{gql}"),
            None => assert!(active.is_null(), "{gql}"),
        }
        assert_eq!(unlocked.status, StatusCode::NO_CONTENT);
        assert_eq!(after["locked"], false);
        assert!(after["active_lock_reason"].is_null());
        assert!(gql_after["data"]["repository"]["issue"]["activeLockReason"].is_null());
        actions.extend(["locked", "unlocked"]);
    }
    let bad = alice
        .call_json(Method::PUT, &lock, &json!({"lock_reason": "rude"}))
        .await;
    let refused = carol.call(Method::PUT, &lock).await;
    let events = alice.get(&format!("{issue}/events")).await.json();
    server.admin(Method::POST, "hooks/drain", None).await;
    let received = server.receiver.received().await;

    // And a wrong reason or a reader changes nothing, and each change is an event and a webhook
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bad.json()["errors"][0]["field"], "lock_reason");
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(alice.get(&issue).await.json()["locked"], false);
    let events = events.as_array().unwrap();
    let named: Vec<_> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(named, actions);
    assert_eq!(events[2]["lock_reason"], "too heated");
    assert!(events[8]["lock_reason"].is_null());
    assert!(events[1].get("lock_reason").is_none());
    assert!(events[0]["node_id"].as_str().unwrap().starts_with("LOE_"));
    assert!(events[1]["node_id"].as_str().unwrap().starts_with("UNLOE_"));
    let delivered: Vec<_> = received
        .iter()
        .filter(|d| d.header("x-github-event") == "issues")
        .map(hubdouble_testkit::Delivery::json)
        .filter(|p| p["action"] != "opened")
        .collect();
    let delivered_actions: Vec<_> = delivered
        .iter()
        .map(|p| p["action"].as_str().unwrap())
        .collect();
    assert_eq!(delivered_actions, actions);
    assert_eq!(delivered[4]["issue"]["active_lock_reason"], "resolved");
    assert_eq!(delivered[4]["issue"]["locked"], true);
}

#[tokio::test]
async fn a_pull_request_locks_through_its_issue_and_sends_pull_request() {
    // Given pull request 1 of acme/public
    let pull = json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
        "title": "feature", "head": "feature", "base": "main"});
    let server = whole_server(&[pull]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let lock = format!("{REPO}/issues/1/lock");

    // When alice locks it as resolved, reads it, and unlocks it
    let locked = alice
        .call_json(Method::PUT, &lock, &json!({"lock_reason": "resolved"}))
        .await;
    let read = alice.get(&format!("{REPO}/pulls/1")).await.json();
    let unlocked = alice.call(Method::DELETE, &lock).await;
    server.admin(Method::POST, "hooks/drain", None).await;
    let received = server.receiver.received().await;

    // Then the pull request shows the lock, and pull_request carries each change
    assert_eq!(locked.status, StatusCode::NO_CONTENT);
    assert_eq!(unlocked.status, StatusCode::NO_CONTENT);
    assert_eq!(read["locked"], true);
    assert_eq!(read["active_lock_reason"], "resolved");
    let changes: Vec<_> = received
        .iter()
        .filter(|d| d.header("x-github-event") == "pull_request")
        .map(hubdouble_testkit::Delivery::json)
        .filter(|p| p["action"] != "opened")
        .map(|p| (p["action"].clone(), p["pull_request"]["locked"].clone()))
        .collect();
    assert_eq!(
        changes,
        [
            (json!("locked"), json!(true)),
            (json!("unlocked"), json!(false))
        ]
    );
}

#[tokio::test]
async fn a_file_comment_answers_line_and_position_1_and_an_empty_hunk_in_rest() {
    // Given a pull request from feature to main
    let pull = json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
        "title": "feature", "head": "feature", "base": "main"});
    let server = whole_server(&[pull]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let files = alice.get(&format!("{REPO}/pulls/1/files")).await.json();
    let path = files[0]["filename"].as_str().unwrap().to_owned();

    // When the client writes a review with a comment on the whole file and reads it back
    let review = alice
        .post(
            &format!("{REPO}/pulls/1/reviews"),
            &json!({"event": "COMMENT", "comments": [
                {"path": path, "subject_type": "file", "body": "file"}]}),
        )
        .await;
    let listed = alice.get(&format!("{REPO}/pulls/1/comments")).await.json();
    let id = listed[0]["id"].as_i64().unwrap();
    let single = alice
        .get(&format!("{REPO}/pulls/comments/{id}"))
        .await
        .json();

    // Then REST answers the fields live GitHub answers for a file comment
    assert_eq!(review.status, StatusCode::OK, "{}", review.json());
    for comment in [&listed[0], &single] {
        assert_eq!(comment["subject_type"], "file");
        for field in ["line", "original_line", "position", "original_position"] {
            assert_eq!(comment[field], 1, "{field}");
        }
        assert_eq!(comment["diff_hunk"], "");
        assert_eq!(comment["side"], "RIGHT");
        assert!(comment["start_line"].is_null());
        assert!(comment["start_side"].is_null());
    }
}

#[tokio::test]
async fn team_and_collaborator_reads_take_the_scopes_github_accepts() {
    // Given a team of acme, and alice's classic tokens with no scope and with each single scope
    let team = json!({"kind": "team", "org": "acme", "name": "Child",
        "members": [{"login": "bob"}],
        "repositories": [{"repository": "acme/public", "permission": "read"}]});
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for mut entry in fixture::seed().into_iter().chain([team]) {
        if entry["kind"] == "user" && entry["login"] == ALICE {
            entry["tokens"] = json!([
                {"token": ALICE_TOKEN},
                {"token": NO_SCOPE_PAT, "scopes": []},
                {"token": "ghp_0000000000000000000000000000000000c2", "scopes": ["user"]},
                {"token": "ghp_0000000000000000000000000000000000c3", "scopes": ["repo"]},
                {"token": "ghp_0000000000000000000000000000000000c4", "scopes": ["read:user"]},
            ]);
        }
        builder = builder.entry(entry);
    }
    let server = builder.start().await;
    let client = |token: &str| GoGithub::new(&server).with_token(token);
    let reads = [
        (
            "orgs/acme/teams",
            "admin:org, read:org, repo, user, write:org",
        ),
        (
            "orgs/acme/teams/child",
            "admin:org, read:org, repo, user, write:org",
        ),
        (
            "orgs/acme/teams/child/repos/acme/public",
            "admin:org, read:org, repo, user, write:org",
        ),
        (
            "orgs/acme/teams/child/memberships/bob",
            "admin:org, read:org, repo, write:org",
        ),
        ("orgs/acme/members/bob", "read:org, repo, user"),
    ];

    for (path, accepted) in reads {
        // When the token without scopes and each token with one accepted scope read it
        let bare = client(NO_SCOPE_PAT).get(path).await;

        // Then only the scopes in GitHub's X-Accepted-OAuth-Scopes header let it through
        assert_eq!(bare.status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(bare.header("x-accepted-oauth-scopes"), accepted, "{path}");
        for (token, scope) in [
            ("ghp_0000000000000000000000000000000000c2", "user"),
            ("ghp_0000000000000000000000000000000000c3", "repo"),
            ("ghp_0000000000000000000000000000000000c4", "read:user"),
        ] {
            let answer = client(token).get(path).await;
            let accepts = accepted.split(", ").any(|s| s == scope);
            assert_eq!(answer.status.is_success(), accepts, "{path} {scope}");
        }
    }
    let permission = client(NO_SCOPE_PAT)
        .get("repos/acme/public/collaborators/bob/permission")
        .await;
    assert_eq!(permission.status, StatusCode::OK);
}

#[test]
fn every_fallback_leaf_of_every_registered_type_is_checked_against_its_type() {
    // Given the registry of the whole server
    let mut store = hubdouble_store::StoreBuilder::new();
    let mut git = hubdouble_git::GitBuilder::new(&mut store);
    let mut gql = hubdouble_gql::Registry::new();
    hubdouble_api::register_all(&mut hubdouble_admin::Registrar {
        store: &mut store,
        rest: &mut hubdouble_rest::Registry::new(),
        gql: &mut gql,
        git: &mut git,
        admin: &mut hubdouble_admin::Admin::new(),
    });

    // When every leaf field that a type answers from its serde form is walked
    let unchecked = gql.unchecked_leaves();

    // Then the executor checks each against its scalar or enum type
    assert_eq!(unchecked, Vec::<String>::new());
}

#[tokio::test]
async fn graphql_leaves_answer_values_of_their_schema_types() {
    // Given a secret and a visible team, and a pull with a line and a file review comment
    let teams = [("Visible", "closed"), ("Hidden", "secret")].map(|(name, privacy)| {
        json!({"kind": "team", "org": "acme", "name": name, "privacy": privacy,
            "members": [{"login": "alice"}]})
    });
    let pull = json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
        "title": "feature", "head": "feature", "base": "main"});
    let server = whole_server(&[vec![pull], teams.to_vec()].concat()).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let review = alice
        .post(
            &format!("{REPO}/pulls/1/reviews"),
            &json!({"event": "COMMENT", "body": "review", "comments": [
                {"path": "src/feature.txt", "line": 1, "body": "line"},
                {"path": "src/feature.txt", "subject_type": "file", "body": "file"}]}),
        )
        .await;
    assert_eq!(review.status, StatusCode::OK, "{}", review.json());

    // When the client reads the team privacy, the review ID, the lock reason and the positions
    let answer = Graphql::new(alice)
        .query(
            "{ organization(login: \"acme\") { teams(first: 10) { nodes { name privacy } } }
               repository(owner: \"acme\", name: \"public\") { pullRequest(number: 1) {
                 activeLockReason
                 reviews(first: 1) { nodes { id } }
                 reviewThreads(first: 5) { nodes { comments(first: 1) {
                   nodes { body originalPosition } } } } } } }",
            &json!({}),
        )
        .await;

    // Then each value is one of its GraphQL type, and a file comment has GitHub's position 1
    let data = &answer.json()["data"];
    let privacy: Vec<_> = data["organization"]["teams"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|team| (team["name"].as_str().unwrap(), team["privacy"].clone()))
        .collect();
    assert!(
        privacy.contains(&("Visible", json!("VISIBLE"))),
        "{privacy:?}"
    );
    assert!(
        privacy.contains(&("Hidden", json!("SECRET"))),
        "{privacy:?}"
    );
    let pull = &data["repository"]["pullRequest"];
    assert_eq!(pull["activeLockReason"], Value::Null);
    assert_eq!(pull["reviews"]["nodes"][0]["id"], review.json()["node_id"]);
    assert!(
        review.json()["node_id"]
            .as_str()
            .unwrap()
            .starts_with("PRR_")
    );
    let positions: Vec<_> = pull["reviewThreads"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["comments"]["nodes"][0]["originalPosition"].clone())
        .collect();
    assert_eq!(positions, [json!(1), json!(1)]);
    assert_eq!(answer.json().get("errors"), None, "{}", answer.json());
}

const SPOON_PATCH: &str = include_str!("live/spoon-knife-compare.patch");
const SPOON_DIFF: &str = include_str!("live/spoon-knife-compare.diff");
const RENAME_PATCH: &str = include_str!("live/gitignore-rename.patch");
const BINARY_DIFF: &str = include_str!("live/explore-binary.diff");
const SPOON: &str = "repos/acme/spoon";
const FIRST: &str = "bb4cc8d3b2e14b3af5df699876dd4ff3acd00b7f";
const SECOND: &str = "d0dd1f61b33d64e29d8bc1372a94ef6a2fee76a9";

fn added(diff: &str, path: &str) -> String {
    let start = diff.find(&format!("+++ b/{path}\n")).unwrap();
    let mut content = String::new();
    for line in diff[start..]
        .lines()
        .skip(2)
        .take_while(|line| line.starts_with('+'))
    {
        content.push_str(&line[1..]);
        content.push('\n');
    }
    content
}

fn abbreviated(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| match line.strip_prefix("index ") {
            Some(rest) => {
                let end = rest.find([' ', '\n']).unwrap();
                let (a, b) = rest[..end].split_once("..").unwrap();
                format!("index {}..{}{}", &a[..7], &b[..7], &rest[end..])
            }
            None => line.to_owned(),
        })
        .collect()
}

fn octocat(message: &str, date: &str, files: &Value) -> Value {
    json!({"branch": "fork", "message": message, "files": files,
        "author": {"name": "The Octocat", "email": "octocat@nowhere.com", "date": date}})
}

async fn spoon_server() -> TestServer {
    let v1 = added(SPOON_PATCH, "README.md");
    let css = added(SPOON_PATCH, "styles.css");
    let v2 = added(SPOON_DIFF, "README.md");
    let ignore = json!({"community/Hardhat.gitignore": "node_modules\n"});
    let moved = json!({"community/Hardhat.gitignore": null,
        "community/JavaScript/Hardhat.gitignore": "node_modules\n"});
    let mut commits = vec![
        octocat(
            "Create styles.css and updated README\n",
            "2014-02-04T22:38:36Z",
            &json!({"README.md": v1, "styles.css": css}),
        ),
        octocat(
            "Pointing to the guide for forking\n",
            "2014-02-12T23:20:44Z",
            &json!({"README.md": v2}),
        ),
    ];
    for files in [
        ignore,
        moved,
        json!({"topics/git/git.png": "\u{0}a"}),
        json!({"topics/git/git.png": "\u{0}b"}),
    ] {
        commits.push(json!({"branch": "moves", "message": "Move", "files": files}));
    }
    let repo = json!({"kind": "repository", "owner": "acme", "name": "spoon",
        "visibility": "public", "files": {"index.html": "<html></html>\n"}, "commits": commits});
    let pull = json!({"kind": "pull_request", "repository": "acme/spoon", "user": "alice",
        "title": "fork", "head": "fork", "base": "main"});
    whole_server(&[repo, pull]).await
}

async fn text(client: &GoGithub, path: &str, accept: &'static str) -> (String, String, String) {
    let answer = client
        .call_json_headers(
            http::Method::GET,
            path,
            Vec::new(),
            headers(&[("accept", accept)]),
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{path} {accept}");
    (
        String::from_utf8(answer.body.to_vec()).unwrap(),
        answer.header("content-type").to_owned(),
        answer.header("x-github-media-type").to_owned(),
    )
}

async fn shas(client: &GoGithub, branch: &str) -> Vec<String> {
    let answer = client.get(&format!("{SPOON}/commits?sha={branch}")).await;
    let mut shas: Vec<String> = answer
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["sha"].as_str().unwrap().to_owned())
        .collect();
    shas.reverse();
    shas
}

#[tokio::test]
async fn diff_and_patch_media_answer_the_bytes_github_writes() {
    // Given the two Spoon-Knife commits, seeded with their files, authors and dates in UTC
    let server = spoon_server().await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let fork = shas(&alice, "fork").await;
    let (first, second) = (&fork[1], &fork[2]);
    let live = abbreviated(SPOON_PATCH)
        .replace(FIRST, first)
        .replace(SECOND, second)
        .replace("4 Feb 2014 14:38:36 -0800", "4 Feb 2014 22:38:36 +0000")
        .replace("12 Feb 2014 15:20:44 -0800", "12 Feb 2014 23:20:44 +0000");
    let first_mail = &live[..live.find(&format!("\nFrom {second}")).unwrap()];

    // When the client asks a commit, the compare and the pull request for diff and patch text
    let commit = format!("{SPOON}/commits/{first}");
    let compare = format!("{SPOON}/compare/main...fork");
    let commit_diff = text(&alice, &commit, "application/vnd.github.diff").await;
    let commit_patch = text(&alice, &commit, "application/vnd.github.patch").await;
    let compare_diff = text(&alice, &compare, "application/vnd.github.v3.diff").await;
    let compare_patch = text(&alice, &compare, "application/vnd.github.patch").await;
    let pull_diff = text(
        &alice,
        &format!("{SPOON}/pulls/1"),
        "application/vnd.github.diff",
    )
    .await;
    let pull_patch = text(
        &alice,
        &format!("{SPOON}/pulls/1"),
        "application/vnd.github.v3.patch",
    )
    .await;

    // Then each body is live GitHub's, with the seed's SHAs, UTC dates and 7-character IDs
    let diff_type = "application/vnd.github.diff; charset=utf-8".to_owned();
    let patch_type = "application/vnd.github.patch; charset=utf-8".to_owned();
    assert_eq!(
        commit_diff,
        (
            first_mail[first_mail.find("diff --git").unwrap()..].to_owned(),
            diff_type.clone(),
            "github.v3; param=diff".to_owned()
        )
    );
    assert_eq!(commit_patch.0, first_mail.replace("[PATCH 1/2]", "[PATCH]"));
    assert_eq!(
        (commit_patch.1, commit_patch.2),
        (patch_type.clone(), "github.v3; param=patch".to_owned())
    );
    assert_eq!(compare_diff.0, abbreviated(SPOON_DIFF));
    assert_eq!(compare_patch.0, live);
    assert_eq!((pull_diff.0, pull_diff.1), (compare_diff.0, diff_type));
    assert_eq!((pull_patch.0, pull_patch.1), (live, patch_type));
}

async fn blob(client: &GoGithub, sha: &str) -> String {
    let answer = client.get(&format!("{SPOON}/commits/{sha}")).await;
    answer.json()["files"][0]["sha"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn renames_and_binary_files_take_the_lines_github_writes() {
    // Given a rename of an unchanged file and a change of a binary file
    let server = spoon_server().await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let moves = shas(&alice, "moves").await;
    let (rename, binary) = (&moves[2], &moves[4]);
    let (old, new) = (blob(&alice, &moves[3]).await, blob(&alice, binary).await);

    // When the client asks the two commits for patch and diff text
    let commit = |sha: &str| format!("{SPOON}/commits/{sha}");
    let rename_patch = text(&alice, &commit(rename), "application/vnd.github.patch").await;
    let binary_diff = text(&alice, &commit(binary), "application/vnd.github.diff").await;
    let binary_patch = text(&alice, &commit(binary), "application/vnd.github.patch").await;

    // Then the rename and binary lines are live GitHub's, and the patch holds the full binary
    assert!(rename_patch.0.ends_with(RENAME_PATCH), "{}", rename_patch.0);
    assert_eq!(
        binary_diff.0,
        BINARY_DIFF.replace(
            "e88d9a3c6525..d141110537fa",
            &format!("{}..{}", &old[..7], &new[..7])
        )
    );
    let patch = binary_patch.0;
    assert!(
        patch.contains("\n topics/git/git.png | Bin 2 -> 2 bytes\n 1 file changed, 0 insertions(+), 0 deletions(-)\n\n"),
        "{patch}"
    );
    assert!(
        patch.contains(&format!(
            "\nindex {old}..{new} 100644\nGIT binary patch\nliteral 2\n"
        )),
        "{patch}"
    );
}
