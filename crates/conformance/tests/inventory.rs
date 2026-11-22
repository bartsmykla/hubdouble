use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use http::{Method, StatusCode};
use hubdouble_model::{Access, Clock, Scope, Token, TokenKind};
use hubdouble_rest::Registry;
use hubdouble_server::Registries;
use hubdouble_store::Tokens;
use hubdouble_testkit::answer::resolve_operation;
use hubdouble_testkit::coverage::{self, Case};
use hubdouble_testkit::{Answer, GoGithub, TestServer, fixture};
use serde_json::json;

const GATE: &str = "every_inventory_route_gets_a_checked_answer_for_every_case_that_applies";

const NO_GITHUB_422: &str = "the pinned description lists 422, but no input sent to api.github.com gave one (gh api GET, 2026-11-22)";
const APP_ONLY_422: &str =
    "the pinned description lists 422, the docs name no cause, and only an App can call it";

const DESCRIBED_NOT_SENT: &[(&str, Case, &str)] = &[
    (
        "GET /app/hook/deliveries/{delivery_id}",
        Case::Invalid,
        APP_ONLY_422,
    ),
    (
        "POST /app/hook/deliveries/{delivery_id}/attempts",
        Case::Invalid,
        APP_ONLY_422,
    ),
    (
        "GET /installation/repositories",
        Case::Invalid,
        APP_ONLY_422,
    ),
    (
        "GET /repos/{owner}/{repo}/commits",
        Case::Invalid,
        NO_GITHUB_422,
    ),
    (
        "GET /repos/{owner}/{repo}/compare/{basehead}",
        Case::Invalid,
        NO_GITHUB_422,
    ),
    (
        "GET /repos/{owner}/{repo}/pulls",
        Case::Invalid,
        NO_GITHUB_422,
    ),
    (
        "GET /repos/{owner}/{repo}/pulls/{pull_number}",
        Case::Invalid,
        NO_GITHUB_422,
    ),
    (
        "GET /repos/{owner}/{repo}/pulls/{pull_number}/files",
        Case::Invalid,
        NO_GITHUB_422,
    ),
];

fn inventory() -> BTreeSet<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/inventory/rest-routes.txt");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

fn sample_path(template: &str) -> String {
    template
        .split('/')
        .map(|s| if s.starts_with('{') { "1" } else { s })
        .collect::<Vec<_>>()
        .join("/")
}

fn split(route: &str) -> (Method, &str) {
    let (verb, template) = route
        .split_once(' ')
        .unwrap_or_else(|| panic!("a route without a method: {route}"));
    let method = Method::from_bytes(verb.as_bytes())
        .unwrap_or_else(|e| panic!("a bad method in {route}: {e}"));
    (method, template)
}

fn required(registry: &Registry, route: &str) -> Vec<Case> {
    let (method, template) = split(route);
    let mut cases = vec![Case::Success];
    let sample = sample_path(template);
    if let Some(entry) = registry.route(&method, &sample).and_then(|r| r.entry) {
        let req = entry.requirement;
        if req.scope == Scope::Repo {
            cases.push(Case::UnseenRepository);
        }
        let every_installation_has_it = req
            .any_of
            .iter()
            .any(|g| g.permission == "metadata" && g.access == Access::Read);
        if !req.any_of.is_empty() && !every_installation_has_it {
            cases.push(Case::MissingPermission);
        }
    }
    if resolve_operation(method.as_str(), &sample)
        .is_some_and(|op| op.template == template && op.documents(422))
    {
        cases.push(Case::Invalid);
    }
    cases
}

#[test]
fn the_required_cases_come_from_the_registration_and_the_description() {
    // Given the whole server's registrations
    let registry = Registries::new(hubdouble_api::register_all).rest;

    // When the cases of routes whose GitHub docs differ are derived
    let case = |route| required(&registry, route);

    // Then each route needs exactly the cases its GitHub docs give it
    assert_eq!(
        case("POST /repos/{owner}/{repo}/pulls"),
        [
            Case::Success,
            Case::UnseenRepository,
            Case::MissingPermission,
            Case::Invalid
        ]
    );
    assert_eq!(
        case("GET /repos/{owner}/{repo}"),
        [Case::Success, Case::UnseenRepository]
    );
    assert_eq!(case("GET /app"), [Case::Success]);
    assert_eq!(case("POST /graphql"), [Case::Success]);
}

fn routes_needing(case: Case) -> Vec<(Method, String)> {
    let registry = Registries::new(hubdouble_api::register_all).rest;
    inventory()
        .into_iter()
        .filter(|route| required(&registry, route).contains(&case))
        .map(|route| {
            let (method, template) = split(&route);
            (method, template.to_owned())
        })
        .collect()
}

fn concrete(template: &str, owner: &str, repo: &str) -> String {
    template
        .split('/')
        .map(|s| match s {
            "{owner}" | "{org}" => owner,
            "{repo}" => repo,
            "{repository_id}" => "999999999",
            s if s.starts_with('{') => "1",
            s => s,
        })
        .collect::<Vec<_>>()
        .join("/")
}

async fn send(client: &GoGithub, method: &Method, path: &str) -> Answer {
    if [Method::POST, Method::PATCH, Method::PUT].contains(method) {
        client.call_json(method.clone(), path, &json!({})).await
    } else {
        client.call(method.clone(), path).await
    }
}

#[tokio::test]
async fn every_repository_route_answers_404_for_a_repository_that_does_not_exist() {
    // Given a user whose token is valid and a repository name that no account holds
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .start()
        .await;
    let client = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);

    for (method, template) in routes_needing(Case::UnseenRepository) {
        // When the route is called on that repository
        let path = concrete(&template, fixture::ORG, "does-not-exist");
        let answer = send(&client, &method, &path).await;

        // Then GitHub's answer for a repository the caller cannot see is 404
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{method} {path}");
    }
}

#[tokio::test]
async fn every_route_with_a_permission_answers_403_to_an_installation_without_it() {
    // Given an installation on every repository of the org that holds only metadata read
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .entry(
            json!({"kind": "app", "id": 4009, "slug": "metadata-only", "owner": fixture::ORG,
                      "public_key": fixture::APP_PUBLIC_KEY,
                      "permissions": {"metadata": "read"}, "events": []}),
        )
        .entry(
            json!({"kind": "installation", "id": 5009, "app": "metadata-only",
                      "account": fixture::ORG, "repository_selection": "all",
                      "permissions": {"metadata": "read"}}),
        )
        .start()
        .await;
    let now = server.clock.now();
    server
        .server
        .services()
        .store
        .update(|tx| {
            let id = tx.next_id();
            tx.table_mut::<Tokens>().insert(Token {
                id,
                token: "ghs_metadata_only".to_owned(),
                kind: TokenKind::Installation,
                app_id: Some(4009),
                installation_id: Some(5009),
                user_id: None,
                repo_ids: None,
                permissions: BTreeMap::new(),
                scopes: Vec::new(),
                expires_at: None,
                revoked: false,
                owner_seeded: false,
                created_at: now,
            })?;
            Ok(())
        })
        .unwrap();
    let client = GoGithub::new(&server).with_token("ghs_metadata_only");

    for (method, template) in routes_needing(Case::MissingPermission) {
        // When the route is called on a repository the installation can see
        let path = concrete(&template, fixture::ORG, fixture::PRIVATE_REPO);
        let answer = send(&client, &method, &path).await;

        // Then GitHub answers 403 and names the permissions it accepts
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{method} {path}");
        assert!(
            !answer.header("x-accepted-github-permissions").is_empty(),
            "{method} {path}"
        );
    }
}

#[test]
fn every_inventory_route_gets_a_checked_answer_for_every_case_that_applies() {
    // Given every other conformance test, run in its own nextest process, records its checked answers
    let dir = tempfile::TempDir::new().unwrap();
    let mut run = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    run.args([
        "nextest",
        "run",
        "--workspace",
        "--locked",
        "--no-fail-fast",
        "-E",
        &format!("package(hubdouble-conformance) and not test(={GATE})"),
    ])
    .env(coverage::DIR_ENV, dir.path());
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("NEXTEST") {
            run.env_remove(name);
        }
    }

    // When the suite runs
    let out = run.output().unwrap();

    // Then it passes and every inventory route has a checked answer for each case that applies
    let stderr = String::from_utf8_lossy(&out.stderr);
    let tail: Vec<&str> = stderr.lines().rev().take(40).collect();
    assert!(
        out.status.success(),
        "the conformance suite failed:\n{}",
        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    );
    let proved: BTreeSet<(String, Case)> = coverage::read(dir.path())
        .into_iter()
        .map(|r| (r.route, r.case))
        .collect();
    let registry = Registries::new(hubdouble_api::register_all).rest;
    let mut missing = Vec::new();
    for route in inventory() {
        for case in required(&registry, &route) {
            let unsent = DESCRIBED_NOT_SENT
                .iter()
                .any(|(r, c, _)| *r == route && *c == case);
            let found = proved.contains(&(route.clone(), case));
            if found == unsent {
                let state = if found {
                    "described-not-sent but proved"
                } else {
                    "no checked"
                };
                missing.push(format!("{route}: {state} {} answer", case.as_str()));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "inventory cases without a matching checked answer:\n  {}",
        missing.join("\n  ")
    );
}
