use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use http::{Method, StatusCode};
use hubdouble_conformance::{ANSWER_FIELDS, VARIANTS, Variant};
use hubdouble_model::{Access, Clock, Scope, Token, TokenKind};
use hubdouble_rest::Registry;
use hubdouble_server::Registries;
use hubdouble_store::Tokens;
use hubdouble_testkit::answer::resolve_operation;
use hubdouble_testkit::coverage::{self, Case, Record};
use hubdouble_testkit::{Answer, GoGithub, TestServer, fixture};
use serde_json::json;

const GATE: &str = "every_inventory_case_gets_a_checked_answer_or_signed_delivery";

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
        "GET /repos/{owner}/{repo}/pulls/{pull_number}",
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
    let repository_id = fixture::PRIVATE_REPO_ID.to_string();
    template
        .split('/')
        .map(|s| match s {
            "{owner}" | "{org}" => owner,
            "{repo}" => repo,
            "{repository_id}" => &repository_id,
            s if s.starts_with('{') => "1",
            s => s,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn installation_token(server: &TestServer, token: &str, app_id: i64, installation_id: i64) {
    let now = server.clock.now();
    server
        .server
        .services()
        .store
        .update(|tx| {
            let id = tx.next_id();
            tx.table_mut::<Tokens>().insert(Token {
                id,
                token: token.to_owned(),
                kind: TokenKind::Installation,
                app_id: Some(app_id),
                installation_id: Some(installation_id),
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
        .unwrap_or_else(|e| panic!("add the token {token}: {e:?}"));
}

async fn send(client: &GoGithub, method: &Method, path: &str) -> Answer {
    if [Method::POST, Method::PATCH, Method::PUT].contains(method) {
        client.call_json(method.clone(), path, &json!({})).await
    } else {
        client.call(method.clone(), path).await
    }
}

#[tokio::test]
async fn every_repository_route_answers_404_for_a_private_repository_outside_the_installation() {
    // Given an installation token of the fixture App, whose installation covers only the public repository, and the org's existing private repository
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .start()
        .await;
    installation_token(
        &server,
        "ghs_public_only",
        fixture::APP_ID,
        fixture::INSTALLATION_ID,
    );
    let client = GoGithub::new(&server).with_token("ghs_public_only");

    for (method, template) in routes_needing(Case::UnseenRepository) {
        // When the route is called on that private repository
        let path = concrete(&template, fixture::ORG, fixture::PRIVATE_REPO);
        let answer = send(&client, &method, &path).await;

        // Then GitHub answers 404, as for a repository that does not exist
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
    installation_token(&server, "ghs_metadata_only", 4009, 5009);
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

fn required_webhooks(plan: &str) -> BTreeSet<(String, Option<String>)> {
    let (_, section) = plan
        .split_once("### 4.3 Webhooks")
        .unwrap_or_else(|| panic!("plan 4.3 is missing"));
    let mut required = BTreeSet::new();
    let rows = section.lines().take_while(|line| !line.starts_with("### "));
    for row in rows.filter(|row| row.starts_with("| `")) {
        let (events, cause) = row[2..]
            .split_once(" | ")
            .unwrap_or_else(|| panic!("invalid plan 4.3 row: {row}"));
        if cause.contains("does not block done") {
            continue;
        }
        let mut rest = events;
        while let Some((_, quoted)) = rest.split_once('`') {
            let (event, tail) = quoted
                .split_once('`')
                .unwrap_or_else(|| panic!("unclosed event name: {quoted}"));
            if let Some(actions) = tail.strip_prefix(" (") {
                let (actions, next) = actions
                    .split_once(')')
                    .unwrap_or_else(|| panic!("unclosed {event} action list: {actions}"));
                if !actions.contains("does not block done") {
                    let names = actions.split_once(';').map_or(actions, |(names, _)| names);
                    for action in names.split(", ") {
                        required.insert((event.to_owned(), Some(action.to_owned())));
                    }
                }
                rest = next;
            } else {
                required.insert((event.to_owned(), None));
                rest = tail;
            }
        }
    }
    assert!(!required.is_empty(), "plan 4.3 has no webhook cases");
    required
}

#[test]
fn every_inventory_case_gets_a_checked_answer_or_signed_delivery() {
    // Given every other conformance test, run in its own nextest process, records its checked answers
    let dir = tempfile::TempDir::new().unwrap();
    let mut run = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    run.args([
        "nextest",
        "run",
        "--workspace",
        "--locked",
        "--no-fail-fast",
        "--status-level=fail",
        "--final-status-level=fail",
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
    let records = coverage::read(dir.path());
    let proved: BTreeSet<(String, Case)> =
        records.iter().map(|r| (r.route.clone(), r.case)).collect();
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
    for variant in VARIANTS {
        if !records.iter().any(|r| proves(r, variant)) {
            missing.push(format!(
                "{}: no checked {} answer with [{}] (plan 4.8 {})",
                variant.route,
                variant.status,
                variant.facts.join(" "),
                variant.plan
            ));
        }
    }
    let webhooks = coverage::read_webhooks(dir.path());
    let plan = include_str!("../../../docs/PLAN.md");
    for (event, action) in required_webhooks(plan).difference(&webhooks) {
        let name = action
            .as_ref()
            .map_or_else(|| event.clone(), |a| format!("{event}.{a}"));
        missing.push(format!("{name}: no checked signed delivery (plan 4.3)"));
    }
    assert!(
        missing.is_empty(),
        "inventory cases without a checked answer or signed delivery:\n  {}",
        missing.join("\n  ")
    );
}

fn proves(record: &Record, variant: &Variant) -> bool {
    record.route == variant.route
        && record.status == variant.status
        && record.case != Case::UnseenRepository
        && variant.facts.iter().all(|f| match f.strip_prefix('!') {
            Some(absent) => !record.facts.iter().any(|g| g == absent),
            None => record.facts.iter().any(|g| g == f),
        })
}

fn plan_table(plan: &str, heading: &str) -> Vec<Vec<String>> {
    let (_, section) = plan
        .split_once(heading)
        .unwrap_or_else(|| panic!("the plan has no {heading}"));
    section
        .lines()
        .skip_while(|l| !l.starts_with('|'))
        .take_while(|l| l.starts_with('|'))
        .skip(2)
        .map(|l| {
            l.trim()
                .trim_matches('|')
                .split('|')
                .map(|c| c.trim().to_owned())
                .collect()
        })
        .collect()
}

fn plan_variant_rows() -> Vec<(String, String)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/PLAN.md");
    let plan =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let requirements = plan_table(&plan, "### 1.1 ");
    plan_table(&plan, "### 4.8 ")
        .into_iter()
        .map(|row| {
            let cell = match requirements.iter().find(|r| r[0] == row[1]) {
                Some(requirement) => requirement[1].clone(),
                None => row[1].clone(),
            };
            (row[0].clone(), cell)
        })
        .collect()
}

fn backticked(cell: &str) -> Vec<String> {
    let mut text = String::new();
    let mut rest = cell;
    while let Some(open) = rest.find('(') {
        let close = rest[open..].find(')').map_or(rest.len(), |c| open + c + 1);
        text.push_str(&rest[..open]);
        if !rest[open..close].contains(".go") {
            text.push_str(&rest[open..close]);
        }
        rest = &rest[close..];
    }
    text.push_str(rest);
    text.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

fn names(variant: &Variant, token: &str) -> bool {
    let t = token.strip_prefix("Accept: ").unwrap_or(token);
    let t = t.split(": ").next().unwrap_or(t).trim_end_matches("[]");
    variant.route.split(' ').any(|p| p == t)
        || variant.status.to_string() == t
        || variant.facts.iter().any(|f| {
            let f = f.trim_start_matches('!');
            let subject = match f.strip_prefix("body.").or_else(|| f.strip_prefix("query.")) {
                Some(member) => member.split('=').next().unwrap_or(member),
                None => f.split_once('=').map_or(f, |(_, value)| value),
            };
            subject.eq_ignore_ascii_case(t)
        })
}

fn answer_fields(call: &str) -> impl Iterator<Item = &'static str> + '_ {
    ANSWER_FIELDS
        .iter()
        .filter(move |(p, _)| *p == call)
        .flat_map(|(_, f)| f.iter().copied())
}

#[test]
fn the_variant_table_holds_every_variant_plan_section_4_8_names() {
    // Given the call table's rows, with a requirement such as E9 read from the requirement list
    let rows = plan_variant_rows();

    // When each word a row puts in backticks, outside its source citations, is looked up
    let mut by_call: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (call, cell) in &rows {
        let call_words = backticked(call);
        by_call.entry(call.as_str()).or_default().extend(
            backticked(cell)
                .into_iter()
                .filter(|w| !call_words.contains(w)),
        );
    }
    let mut unknown = Vec::new();
    for (call, words) in &by_call {
        for word in words {
            let variant = VARIANTS.iter().any(|v| v.plan == *call && names(v, word));
            if !variant && !answer_fields(call).any(|f| f == word) {
                unknown.push(format!("{call}: {word}"));
            }
        }
        unknown.extend(
            answer_fields(call)
                .filter(|f| !words.iter().any(|w| w == f))
                .map(|f| format!("{call}: the answer field {f} is not in the plan row")),
        );
    }

    // Then each is a variant of the table or an answer field, and the table cites only plan rows
    assert!(
        unknown.is_empty(),
        "plan 4.8 names what the variant table lacks:\n  {}",
        unknown.join("\n  ")
    );
    for plan in VARIANTS
        .iter()
        .map(|v| v.plan)
        .chain(ANSWER_FIELDS.iter().map(|(p, _)| *p))
    {
        assert!(by_call.contains_key(plan), "{plan} is not a plan 4.8 row");
    }
}
