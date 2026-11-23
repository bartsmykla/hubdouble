use http::header::LINK;
use hubdouble_admin::Registrar;
use hubdouble_model::{ApiError, FieldError, Grant, Requirement, Scope};
use hubdouble_rest::{Ctx, Page, Query, Reply};
use serde::Serialize;
use serde_json::json;

pub mod actions;
pub use runs::{AnnotationInput, annotation};
mod events;
mod graphql;
mod pushes;
mod runs;
mod seed;
mod statuses;
mod suites;
mod view;

const fn repo(grants: &'static [Grant], scopes: &'static [&'static str]) -> Requirement {
    Requirement {
        any_of: grants,
        scope: Scope::Repo,
        scopes,
    }
}

const CHECKS_READ: Requirement = repo(&[Grant::read("checks")], &[]);
const CHECKS_WRITE: Requirement = repo(&[Grant::write("checks")], &[]);
const STATUSES_READ: Requirement = repo(&[Grant::read("statuses")], &[]);
const STATUSES_WRITE: Requirement = repo(&[Grant::write("statuses")], &["repo:status"]);

/// Registers what T8 owns: tables, REST operations, GraphQL fields, ref subscribers, seed kinds
/// and admin routes.
pub fn register(r: &mut Registrar<'_>) {
    r.rest.handle("checks/create", CHECKS_WRITE, runs::create);
    r.rest.handle("checks/get", CHECKS_READ, runs::get);
    r.rest.handle("checks/update", CHECKS_WRITE, runs::update);
    r.rest
        .handle("checks/list-for-ref", CHECKS_READ, runs::list_for_ref);
    r.rest.handle(
        "checks/list-annotations",
        CHECKS_READ,
        runs::list_annotations,
    );
    r.rest
        .handle("checks/rerequest-run", CHECKS_WRITE, runs::rerequest);
    r.rest.handle(
        "checks/list-suites-for-ref",
        CHECKS_READ,
        suites::list_for_ref,
    );
    r.rest
        .handle("checks/rerequest-suite", CHECKS_WRITE, suites::rerequest);
    r.rest.handle(
        "repos/create-commit-status",
        STATUSES_WRITE,
        statuses::create,
    );
    r.rest.handle(
        "repos/list-commit-statuses-for-ref",
        STATUSES_READ,
        statuses::list,
    );
    r.rest.handle(
        "repos/get-combined-status-for-ref",
        STATUSES_READ,
        statuses::combined,
    );
    graphql::register(r.gql);
    r.store.register_table::<pushes::FirstPushes>();
    r.git.on_ref_update(suites::pushed);
    seed::register(r.admin);
}

fn field_error(field: &str, code: &str) -> ApiError {
    ApiError::unprocessable("Validation Failed").with_error(FieldError {
        resource: "CheckRun".to_owned(),
        field: field.to_owned(),
        code: code.to_owned(),
        message: None,
        value: None,
    })
}

fn invalid(field: &str) -> ApiError {
    field_error(field, "invalid")
}

fn missing(field: &str) -> ApiError {
    field_error(field, "missing_field")
}

fn filter<'q>(query: &'q Query, name: &str) -> Option<&'q str> {
    query.get(name).filter(|v| !v.trim().is_empty())
}

fn app_filter(query: &Query) -> Option<i64> {
    filter(query, "app_id").map(leading_integer)
}

fn leading_integer(text: &str) -> i64 {
    let text = text.trim_start();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let mut value: i64 = 0;
    let mut after_digit = false;
    for c in digits.chars() {
        match c.to_digit(10) {
            Some(d) => {
                value = value.saturating_mul(10).saturating_add(i64::from(d));
                after_digit = true;
            }
            None if c == '_' && after_digit => after_digit = false,
            None => break,
        }
    }
    if negative { -value } else { value }
}

fn window<'i, T>(ctx: &Ctx<'_>, items: &'i [T]) -> (&'i [T], Option<String>) {
    let Page { page, per_page } = ctx.page_params();
    let start = (page - 1).saturating_mul(per_page).min(items.len());
    let end = start.saturating_add(per_page).min(items.len());
    let last = items.len().div_ceil(per_page).max(1);
    (&items[start..end], ctx.link_header(page, last))
}

fn page_object<T, R: Serialize>(
    ctx: &Ctx<'_>,
    key: &str,
    items: &[T],
    render: impl FnMut(&T) -> R,
) -> Result<Reply, ApiError> {
    let (shown, link) = window(ctx, items);
    let rendered: Vec<R> = shown.iter().map(render).collect();
    let reply = Reply::json(&json!({ "total_count": items.len(), key: rendered }))?;
    match link {
        Some(link) => reply.header(LINK, &link),
        None => Ok(reply),
    }
}

#[cfg(test)]
mod tests {
    use super::leading_integer;

    #[test]
    fn an_app_id_filter_reads_the_leading_integer_as_github_does() {
        // Given the values live GitHub matched on a commit's check run list
        let read = [
            ("15368", 15368),
            ("15368abc", 15368),
            ("15368.0", 15368),
            ("015368", 15368),
            ("+15368", 15368),
            (" 15368", 15368),
            ("15_368", 15368),
            ("abc", 0),
            ("1e3", 1),
            ("1.5", 1),
            ("-1", -1),
            ("1__2", 1),
        ];
        for (text, expected) in read {
            // When
            let read = leading_integer(text);
            // Then
            assert_eq!(read, expected, "{text}");
        }
    }
}
