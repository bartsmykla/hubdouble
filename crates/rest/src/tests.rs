use http::Method;
use hubdouble_model::{ApiError, Requirement};
use hubdouble_spec::OPERATIONS;

use crate::{Ctx, Registry, Reply};

#[expect(clippy::unnecessary_wraps, reason = "a handler returns a Result")]
fn ok(_: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    Ok(Reply::empty())
}

fn full_registry() -> Registry {
    let mut r = Registry::new();
    for op in &OPERATIONS {
        r.handle(op.id, Requirement::public(), ok);
    }
    r
}

#[track_caller]
fn routes_to(
    r: &Registry,
    method: &Method,
    path: &str,
    operation_id: &str,
    params: &[(&str, &str)],
) {
    let route = r
        .route(method, path)
        .unwrap_or_else(|| panic!("{method} {path} routes nowhere"));
    assert_eq!(route.operation_id, operation_id, "{method} {path}");
    let got: Vec<(&str, &str)> = route.params.iter().collect();
    assert_eq!(got, params, "{method} {path}");
    assert!(route.entry.is_some());
}

const O: (&str, &str) = ("owner", "o");
const R: (&str, &str) = ("repo", "r");

#[test]
fn a_literal_suffix_beats_a_slash_holding_parameter() {
    let r = full_registry();
    for (suffix, op) in [
        ("check-runs", "checks/list-for-ref"),
        ("check-suites", "checks/list-suites-for-ref"),
        ("status", "repos/get-combined-status-for-ref"),
        ("statuses", "repos/list-commit-statuses-for-ref"),
    ] {
        routes_to(
            &r,
            &Method::GET,
            &format!("/repos/o/r/commits/abc/{suffix}"),
            op,
            &[O, R, ("ref", "abc")],
        );
        routes_to(
            &r,
            &Method::GET,
            &format!("/repos/o/r/commits/heads/main/{suffix}"),
            op,
            &[O, R, ("ref", "heads/main")],
        );
    }
    routes_to(
        &r,
        &Method::GET,
        "/repos/o/r/commits/abc/pulls",
        "repos/list-pull-requests-associated-with-commit",
        &[O, R, ("commit_sha", "abc")],
    );
    routes_to(
        &r,
        &Method::GET,
        "/repos/o/r/commits/abc",
        "repos/get-commit",
        &[O, R, ("ref", "abc")],
    );
    routes_to(
        &r,
        &Method::GET,
        "/repos/o/r/commits/feature/x",
        "repos/get-commit",
        &[O, R, ("ref", "feature/x")],
    );
    routes_to(
        &r,
        &Method::GET,
        "/repos/o/r/branches/main",
        "repos/get-branch",
        &[O, R, ("branch", "main")],
    );
    routes_to(
        &r,
        &Method::GET,
        "/repos/o/r/branches/main/protection",
        "repos/get-branch-protection",
        &[O, R, ("branch", "main")],
    );
    routes_to(
        &r,
        &Method::GET,
        "/repos/o/r/compare/main...feature/x",
        "repos/compare-commits",
        &[O, R, ("basehead", "main...feature/x")],
    );
}

#[test]
fn a_literal_segment_beats_a_parameter_at_the_same_depth() {
    let r = full_registry();
    routes_to(
        &r,
        &Method::GET,
        "/repos/o/r/issues/comments/42",
        "issues/get-comment",
        &[O, R, ("comment_id", "42")],
    );
    routes_to(
        &r,
        &Method::GET,
        "/repos/o/r/issues/42/events",
        "issues/list-events",
        &[O, R, ("issue_number", "42")],
    );
    routes_to(
        &r,
        &Method::GET,
        "/repos/o/r/issues/comments/events",
        "issues/get-comment",
        &[O, R, ("comment_id", "events")],
    );
    routes_to(&r, &Method::GET, "/repos/o/r/pulls", "pulls/list", &[O, R]);
    routes_to(&r, &Method::GET, "/", "meta/root", &[]);
}

struct GoGithub {
    call: &'static str,
    source: &'static str,
    path: &'static str,
    operation_id: &'static str,
    param: (&'static str, &'static str),
}

#[test]
fn go_github_v93_escaped_paths_route_to_their_operations() {
    let r = full_registry();
    let cases = [
        GoGithub {
            call: r#"Repositories.GetContents(ctx, "o", "r", "docs/my file.md", nil)"#,
            source: "go-github v93.0.0 github/repos_contents.go:230-232",
            path: "/repos/o/r/contents/docs/my%20file.md",
            operation_id: "repos/get-content",
            param: ("path", "docs/my file.md"),
        },
        GoGithub {
            call: r#"Repositories.GetContents(ctx, "o", "r", "", nil)"#,
            source: "go-github v93.0.0 github/repos_contents.go:230-232",
            path: "/repos/o/r/contents/",
            operation_id: "repos/get-content",
            param: ("path", ""),
        },
        GoGithub {
            call: r#"Git.GetRef(ctx, "o", "r", "refs/heads/feature/a b")"#,
            source: "go-github v93.0.0 github/git_refs.go:58-60, refURLEscape at :77-83",
            path: "/repos/o/r/git/ref/heads/feature/a%20b",
            operation_id: "git/get-ref",
            param: ("ref", "heads/feature/a b"),
        },
        GoGithub {
            call: r#"Repositories.GetBranch(ctx, "o", "r", "feature/protection", 1)"#,
            source: "go-github v93.0.0 github/repos.go:1425-1426",
            path: "/repos/o/r/branches/feature%2Fprotection",
            operation_id: "repos/get-branch",
            param: ("branch", "feature/protection"),
        },
        GoGithub {
            call: r#"Repositories.GetRulesForBranch(ctx, "o", "r", "feature/protection", nil)"#,
            source: "go-github v93.0.0 github/repos_rules.go:22-23",
            path: "/repos/o/r/rules/branches/feature%2Fprotection",
            operation_id: "repos/get-branch-rules",
            param: ("branch", "feature/protection"),
        },
    ];
    for c in cases {
        let route = r
            .route(&Method::GET, c.path)
            .unwrap_or_else(|| panic!("{} ({}) routes nowhere", c.call, c.source));
        assert_eq!(
            route.operation_id, c.operation_id,
            "{} ({})",
            c.call, c.source
        );
        assert_eq!(
            route.params.get(c.param.0),
            Some(c.param.1),
            "{} ({})",
            c.call,
            c.source
        );
    }
    routes_to(
        &r,
        &Method::GET,
        "/repos/o/r/branches/feature/protection",
        "repos/get-branch-protection",
        &[O, R, ("branch", "feature")],
    );
}

#[test]
fn an_unregistered_operation_routes_without_a_handler() {
    let r = Registry::new();
    let route = r.route(&Method::POST, "/repos/o/r/pulls").unwrap();
    assert_eq!(route.operation_id, "pulls/create");
    assert!(route.entry.is_none());
    assert!(r.route(&Method::GET, "/no/such/route").is_none());
    assert!(
        r.route(&Method::PUT, "/repos/o/r/issues/comments/1")
            .is_none()
    );
}

#[test]
fn extra_routes_sit_next_to_the_table() {
    let mut r = Registry::new();
    r.extra(
        Method::GET,
        "/login/oauth/authorize",
        "oauth/authorize",
        Requirement::public(),
        ok,
    );
    r.extra(
        Method::GET,
        "/orgs/{org}/sso",
        "oauth/sso",
        Requirement::public(),
        ok,
    );

    routes_to(
        &r,
        &Method::GET,
        "/login/oauth/authorize",
        "oauth/authorize",
        &[],
    );
    routes_to(
        &r,
        &Method::GET,
        "/orgs/octo-org/sso",
        "oauth/sso",
        &[("org", "octo-org")],
    );
    assert_eq!(
        r.route(&Method::GET, "/orgs/octo-org").map(|x| x.operation_id),
        Some("orgs/get")
    );
    assert_eq!(r.operations().filter(|o| o.registered).count(), 2);
}

#[test]
#[should_panic(expected = "registered twice")]
fn an_operation_registers_once() {
    let mut r = Registry::new();
    r.handle("pulls/create", Requirement::public(), ok);
    r.handle("pulls/create", Requirement::public(), ok);
}

#[test]
#[should_panic(expected = "unknown operation")]
fn an_unknown_operation_does_not_register() {
    Registry::new().handle("pulls/teleport", Requirement::public(), ok);
}
