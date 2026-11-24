#[path = "graphql_docs/git.rs"]
mod git;
#[path = "graphql_docs/requests.rs"]
mod requests;
#[path = "graphql_docs/reviews.rs"]
mod reviews;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use http::{Method, StatusCode};
use hubdouble_model::node_id;
use hubdouble_testkit::{GoGithub, Graphql, TestServer, fixture};
use serde_json::{Value, json};

const REPO: &str = "repos/acme/docs";
const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/inventory/graphql");
const REVIEW: i64 = 7_001;
const PENDING: i64 = 7_002;
const THREAD: i64 = 7_003;
const OPENING: i64 = 7_004;
const ROOT_TEAM: i64 = 7_005;
const CHILD_TEAM: i64 = 7_006;

struct Inventory {
    sources: BTreeMap<String, String>,
    ran: BTreeSet<String>,
    api: Graphql,
}

impl Inventory {
    fn new(api: Graphql) -> Self {
        let sources = Path::new(ROOT)
            .read_dir()
            .unwrap_or_else(|e| panic!("read the GraphQL inventory: {e}"))
            .map(|entry| {
                entry
                    .unwrap_or_else(|e| panic!("read an inventory entry: {e}"))
                    .path()
            })
            .filter(|path| path.extension().is_some_and(|e| e == "graphql"))
            .map(|path| {
                (
                    path.file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or_else(|| panic!("invalid inventory path: {}", path.display()))
                        .to_owned(),
                    std::fs::read_to_string(&path)
                        .unwrap_or_else(|e| panic!("read {}: {e}", path.display())),
                )
            })
            .collect();
        Self {
            sources,
            ran: BTreeSet::new(),
            api,
        }
    }

    async fn run(&mut self, name: &str, vars: Value) -> Value {
        let data = success(&self.api, &self.sources[name], vars).await;
        self.ran.insert(name.to_owned());
        data
    }

    async fn access(&mut self, api: &Graphql) {
        let vars = json!({"owner":"acme", "login":"alice", "n0":"public", "n1":"docs"});
        let body = self.run("access", vars).await;
        for alias in ["r0", "r1"] {
            assert_eq!(
                body[alias],
                json!({"collaborators":{"edges":[
                    {"permission":"ADMIN","node":{"databaseId":fixture::ALICE_ID}}
                ]}})
            );
        }
        let result = api
            .query(
                &self.sources["access"],
                &json!({
                    "owner":"acme", "login":"alice", "n0":"docs", "n1":"private"
                }),
            )
            .await
            .json();
        assert_eq!(result["data"]["r0"], body["r0"]);
        assert_eq!(result["data"]["r1"], Value::Null);
        assert_eq!(result["errors"].as_array().map(Vec::len), Some(1));
        assert_eq!(result["errors"][0]["type"], "NOT_FOUND");
        assert_eq!(result["errors"][0]["path"], json!(["r1"]));
    }
}

async fn success(api: &Graphql, doc: &str, vars: Value) -> Value {
    let response = api.query(doc, &vars).await;
    assert_eq!(response.status, StatusCode::OK);
    let body = response.json();
    assert!(body.get("errors").is_none(), "{doc}\n{body}");
    body["data"].clone()
}

fn args(extra: Value) -> Value {
    let Value::Object(mut vars) = extra else {
        panic!("expected variables: {extra}")
    };
    vars.extend(
        ["owner", "name", "repo"]
            .into_iter()
            .zip(["acme", "docs", "docs"])
            .map(|(key, value)| (key.to_owned(), json!(value))),
    );
    Value::Object(vars)
}

async fn post(client: &GoGithub, path: &str, input: Value, status: StatusCode) -> Value {
    let response = client.post(path, &input).await;
    assert_eq!(response.status, status, "{path}: {}", response.json());
    response.json()
}

fn client(server: &TestServer, token: &str) -> GoGithub {
    GoGithub::new(server).with_token(token)
}

async fn server() -> TestServer {
    let repo = json!({"kind":"repository", "owner":"acme", "name":"docs",
    "visibility":"public", "has_issues":true, "branches":["develop"],
    "collaborators":[{"login":"bob","permission":"write"}],
    "files":{"README.md":"# Docs\n", ".github/pull_request_template.md":"# Template\n",
        ".github/workflows/ci.yml":"name: CI\non: push\njobs: {}\n"},
    "tags":[{"name":"agent/v5.0.0","message":"v5 release"}],
    "commits":[
        {"branch":"prompts/proposal-reviewed","message":"One","files":{"guide.md":"# Guide\n"}},
        {"branch":"prompts/proposal-pending","message":"Two","files":{"pending.md":"# Pending\n"}},
        {"branch":"feature","message":"Three","files":{"draft.md":"# Draft\n",
            ".github/workflows/ci.yml":"name: CI\non: pull_request\njobs: {}\n"}}
    ]});
    let pull = |title, branch, draft| {
        json!({"kind":"pull_request", "repository":"acme/docs",
        "user":"alice", "title":title, "head":branch, "base":"main", "draft":draft})
    };
    let review = |id, number, author, event: Option<&str>| {
        let mut review = json!({"kind":"review", "id":id, "repository":"acme/docs",
            "pull_number":number, "author":author, "body":"Review body"});
        if let Some(event) = event {
            review["event"] = json!(event);
        }
        review
    };
    let check = |branch, state, conclusion| {
        json!({"kind":"check_run", "repository":"acme/docs",
        "app":fixture::APP_SLUG, "name":"CI", "head_sha":branch, "status":state,
        "conclusion":conclusion})
    };
    TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .entry(repo)
        .entry(
            json!({"kind":"ruleset","repository":"acme/docs","name":"Require review",
            "bypass_actors":[{"actor_type":"RepositoryRole","actor_id":5,"bypass_mode":"always"}],
            "conditions":{"ref_name":{"include":["~DEFAULT_BRANCH"],"exclude":[]}},
            "rules":[{"type":"pull_request","parameters":{"required_approving_review_count":1}}]}),
        )
        .entry(json!({"kind":"team","id":ROOT_TEAM,"org":"acme","name":"Root"}))
        .entry(
            json!({"kind":"team","id":CHILD_TEAM,"org":"acme","name":"Child",
            "parent":"root","members":[{"login":"bob"}]}),
        )
        .entry(pull(
            "Reviewed proposal",
            "prompts/proposal-reviewed",
            false,
        ))
        .entry(pull("Pending proposal", "prompts/proposal-pending", false))
        .entry(pull("Draft change", "feature", true))
        .entry(review(REVIEW, 1, "bob", Some("APPROVE")))
        .entry(review(PENDING, 1, "alice", None))
        .entry(review(7_007, 2, "bob", Some("COMMENT")))
        .entry(
            json!({"kind":"review_thread","id":THREAD,"review_id":REVIEW,
            "comment":{"id":OPENING,"body":"Thread opening","path":"guide.md","line":1}}),
        )
        .entry(check(
            "prompts/proposal-reviewed",
            "completed",
            Some("success"),
        ))
        .entry(check("prompts/proposal-pending", "queued", None))
        .start()
        .await
}

#[tokio::test]
async fn every_inventory_document_runs_on_the_complete_server() {
    let server = server().await;
    let alice = client(&server, fixture::ALICE_TOKEN);
    let bob = Graphql::new(client(&server, fixture::BOB_TOKEN));
    let mut cases = Inventory::new(Graphql::new(alice.clone()));
    let teams = cases
        .run("teams", json!({"org":"acme","login":"bob"}))
        .await;
    assert_eq!(
        teams["organization"]["teams"]["nodes"],
        json!([{
            "databaseId": CHILD_TEAM, "ancestors": {"nodes": [{"databaseId": ROOT_TEAM}]}
        }])
    );
    assert_eq!(
        teams["organization"]["teams"]["pageInfo"]["hasNextPage"],
        false
    );
    cases.access(&bob).await;
    proposals(&mut cases).await;
    reviews::run(&server, &alice, &mut cases).await;
    git::run(&alice, &mut cases).await;
    assert_eq!(cases.ran, cases.sources.keys().cloned().collect());
}

async fn proposals(cases: &mut Inventory) {
    let data = cases
        .run(
            "proposals",
            args(json!({
                "query":"repo:acme/docs is:pr is:open in:title Reviewed", "first":20
            })),
        )
        .await;
    let search = data["search"]["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("invalid search: {data}"));
    assert_eq!(search.len(), 1);
    assert_proposal(&search[0], 1, "APPROVED", "SUCCESS", "reviewed");
    let refs = data["repository"]["refs"]["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("invalid refs: {data}"));
    assert_eq!(refs.len(), 2);
    let pulls: BTreeMap<_, _> = refs
        .iter()
        .map(|r| {
            let pull = &r["associatedPullRequests"]["nodes"][0];
            (
                pull["number"]
                    .as_u64()
                    .unwrap_or_else(|| panic!("invalid pull: {pull}")),
                pull,
            )
        })
        .collect();
    assert_eq!(pulls[&1], &search[0]);
    assert_proposal(pulls[&2], 2, "REVIEW_REQUIRED", "PENDING", "pending");
    for (number, suffix) in [(1, "reviewed"), (2, "pending")] {
        let data = cases
            .run(
                "proposalOpen",
                args(json!({"query":format!("proposal-{suffix}")})),
            )
            .await;
        assert_eq!(
            data["repository"]["refs"]["nodes"],
            json!([{
                "name": format!("proposal-{suffix}"), "associatedPullRequests": {
                    "nodes":[{"number":number, "url":pulls[&number]["url"]}]
                }
            }])
        );
    }
}

fn assert_proposal(pull: &Value, number: u64, review: &str, checks: &str, suffix: &str) {
    assert_eq!(pull["number"], number);
    assert_eq!(pull["state"], "OPEN");
    assert_eq!(pull["isDraft"], false);
    assert_eq!(pull["headRepository"]["nameWithOwner"], "acme/docs");
    assert_eq!(pull["headRefName"], format!("prompts/proposal-{suffix}"));
    assert!(
        pull["url"]
            .as_str()
            .unwrap_or_default()
            .ends_with(&format!("/acme/docs/pull/{number}"))
    );
    assert_eq!(pull["mergedAt"], Value::Null);
    assert_eq!(pull["mergeCommit"], Value::Null);
    assert_eq!(pull["reviewDecision"], review);
    assert_eq!(
        pull["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["state"],
        checks
    );
}
