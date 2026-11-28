//! The response variants of the inventory call table that the inventory gate requires a checked
//! answer for. `tests/inventory.rs` reads that table and fails when a status, media type, header
//! or body option it names is in neither [`VARIANTS`] nor [`ANSWER_FIELDS`].

const GO_GITHUB: &str = "accept=application/vnd.github.v3+json";

/// One response variant: a checked answer proves it when its route and status are these, its
/// case is not an unseen repository, and it has every fact (as `hubdouble_testkit::coverage::
/// facts` writes them) and none of the facts written with a leading `!`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Variant {
    /// The `Call` cell of the call table the variant comes from, as the table writes it.
    pub plan: &'static str,
    /// The route as `docs/inventory/rest-routes.txt` writes it.
    pub route: &'static str,
    pub status: u16,
    pub facts: &'static [&'static str],
}

const fn v(
    plan: &'static str,
    route: &'static str,
    status: u16,
    facts: &'static [&'static str],
) -> Variant {
    Variant {
        plan,
        route,
        status,
        facts,
    }
}

const JOB_LOGS: &str = "`GET .../actions/jobs/{id}/logs`";
const ASSET: &str = "`GET .../releases/assets/{id}`";
const BLOB: &str = "`GET .../git/blobs/{sha}`";
const CHECK_RUNS: &str = "`POST` and `PATCH .../check-runs`";
const COMMIT: &str = "`GET .../commits/{ref}`";
const CONTENTS: &str = "`GET .../contents/{path}`";
const UNREQUEST: &str = "`DELETE .../pulls/{n}/requested_reviewers`";
const MERGES: &str = "`POST .../merges`";
const DISPATCH: &str = "`POST .../actions/workflows/{id}/dispatches`";
const ACCESS_TOKEN: &str = "`POST /login/oauth/access_token`";
const RULES: &str = "`GET .../rules/branches/{branch}`";
const BRANCH: &str = "`GET .../branches/{branch}`";
const RULESET: &str = "`GET .../rulesets/{id}`";
const STATUS: &str = "`GET .../commits/{ref}/status`";
const RUNS: &str = "`GET .../actions/runs` and `.../workflows/{id}/runs`";

const ASSET_ROUTE: &str = "GET /repos/{owner}/{repo}/releases/assets/{asset_id}";
const BLOB_ROUTE: &str = "GET /repos/{owner}/{repo}/git/blobs/{file_sha}";
const CREATE_RUN: &str = "POST /repos/{owner}/{repo}/check-runs";
const UPDATE_RUN: &str = "PATCH /repos/{owner}/{repo}/check-runs/{check_run_id}";
const COMMIT_ROUTE: &str = "GET /repos/{owner}/{repo}/commits/{ref}";
const CONTENTS_ROUTE: &str = "GET /repos/{owner}/{repo}/contents/{path}";
const UNREQUEST_ROUTE: &str =
    "DELETE /repos/{owner}/{repo}/pulls/{pull_number}/requested_reviewers";
const MERGES_ROUTE: &str = "POST /repos/{owner}/{repo}/merges";
const DISPATCH_ROUTE: &str =
    "POST /repos/{owner}/{repo}/actions/workflows/{workflow_id}/dispatches";

const ACTIONS: &[&str] = &["body.actions", "!body.actions=empty"];
const ANNOTATIONS: &[&str] = &["body.output.annotations", "!body.output.annotations=empty"];

/// Every response variant the call table names, one row per variant.
pub const VARIANTS: &[Variant] = &[
    v(
        JOB_LOGS,
        "GET /repos/{owner}/{repo}/actions/jobs/{job_id}/logs",
        302,
        &[],
    ),
    v(JOB_LOGS, "HEAD /_blobs/{id}", 200, &[]),
    v(JOB_LOGS, "GET /_blobs/{id}", 206, &["range"]),
    v(
        ASSET,
        ASSET_ROUTE,
        200,
        &[GO_GITHUB, "media=application/json"],
    ),
    v(
        ASSET,
        ASSET_ROUTE,
        302,
        &["accept=application/octet-stream"],
    ),
    v(
        BLOB,
        BLOB_ROUTE,
        200,
        &[GO_GITHUB, "media=application/json"],
    ),
    v(
        BLOB,
        BLOB_ROUTE,
        200,
        &["accept=application/vnd.github.raw", "media=text/plain"],
    ),
    v(
        BLOB,
        BLOB_ROUTE,
        200,
        &["accept=application/vnd.github.v3.raw", "media=text/plain"],
    ),
    v(
        BLOB,
        BLOB_ROUTE,
        200,
        &[
            "accept=application/vnd.github.raw",
            "media=application/octet-stream",
        ],
    ),
    v(CHECK_RUNS, CREATE_RUN, 201, ACTIONS),
    v(CHECK_RUNS, UPDATE_RUN, 200, ACTIONS),
    v(CHECK_RUNS, CREATE_RUN, 201, ANNOTATIONS),
    v(CHECK_RUNS, UPDATE_RUN, 200, ANNOTATIONS),
    v(
        COMMIT,
        COMMIT_ROUTE,
        200,
        &[GO_GITHUB, "media=application/json"],
    ),
    v(
        COMMIT,
        COMMIT_ROUTE,
        200,
        &[
            "accept=application/vnd.github.v3.sha",
            "media=application/vnd.github.v3.sha",
        ],
    ),
    v(
        COMMIT,
        COMMIT_ROUTE,
        304,
        &["accept=application/vnd.github.v3.sha", "if-none-match"],
    ),
    v(CONTENTS, CONTENTS_ROUTE, 200, &[GO_GITHUB, "json=object"]),
    v(
        CONTENTS,
        CONTENTS_ROUTE,
        200,
        &[GO_GITHUB, "json=array", "!trailing-slash"],
    ),
    v(
        CONTENTS,
        CONTENTS_ROUTE,
        200,
        &[GO_GITHUB, "json=array", "trailing-slash"],
    ),
    v(
        CONTENTS,
        CONTENTS_ROUTE,
        200,
        &[
            "accept=application/vnd.github.raw",
            "media=application/vnd.github.raw",
        ],
    ),
    v(
        UNREQUEST,
        UNREQUEST_ROUTE,
        200,
        &["body.reviewers", "body.team_reviewers"],
    ),
    v(UNREQUEST, UNREQUEST_ROUTE, 422, &["!body.reviewers"]),
    v(MERGES, MERGES_ROUTE, 201, &[]),
    v(MERGES, MERGES_ROUTE, 204, &[]),
    v(MERGES, MERGES_ROUTE, 409, &[]),
    v(MERGES, MERGES_ROUTE, 404, &[]),
    v(MERGES, MERGES_ROUTE, 403, &[]),
    v(
        DISPATCH,
        DISPATCH_ROUTE,
        200,
        &["body.return_run_details=true"],
    ),
    v(
        DISPATCH,
        DISPATCH_ROUTE,
        204,
        &["!body.return_run_details=true"],
    ),
    v(
        ACCESS_TOKEN,
        "POST /login/oauth/access_token",
        200,
        &["error"],
    ),
    v(
        RULES,
        "GET /repos/{owner}/{repo}/rules/branches/{branch}",
        200,
        &["query.page", "query.per_page", "link", "json=array"],
    ),
    v(
        BRANCH,
        "GET /repos/{owner}/{repo}/branches/{branch}",
        404,
        &[],
    ),
];

/// Words a call table row puts in backticks that name a member or header of an answer,
/// a member of a request body item, or a message, rather than a variant: the `OpenAPI` check
/// and the track's own tests hold them, so the gate does not.
pub const ANSWER_FIELDS: &[(&str, &[&str])] = &[
    (JOB_LOGS, &["Content-Length"]),
    (CHECK_RUNS, &["label", "description", "identifier"]),
    (DISPATCH, &["workflow_run_id", "run_url", "html_url"]),
    (RULES, &["type", "parameters"]),
    (BRANCH, &["protected", "Branch not found"]),
    (RULESET, &["current_user_can_bypass"]),
    (STATUS, &["state", "pending"]),
    (
        RUNS,
        &[
            "total_count",
            "path",
            "check_suite_id",
            "run_attempt",
            "status",
            "run_started_at",
            "updated_at",
        ],
    ),
];
