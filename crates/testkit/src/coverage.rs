//! The record of checked answers that the inventory gate reads. Each nextest test runs in its
//! own process, so a run that wants the record sets [`DIR_ENV`] to a `tempfile::TempDir` it
//! owns; every test process appends one line per checked answer to its own file there. Without
//! the variable nothing is written.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use hubdouble_hooks::Hooks;
use hubdouble_store::{Apps, Store};
use serde_json::Value;

use crate::answer::{Checked, Exchange, header, is_json, media_type};
use crate::receiver::Delivery;

/// The directory for checked answers and signed webhook records.
pub const DIR_ENV: &str = "HUBDOUBLE_TEST_COVERAGE_DIR";

/// Which case of the per-operation test list a checked answer proves, read from the
/// answer and the server's state, never from what the test says it meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Case {
    /// A `2xx` or `3xx` answer.
    Success,
    /// A `404` to a request whose `{owner}`/`{repo}` or `{repository_id}` names a repository
    /// that does not exist on the server, so no actor can see it.
    UnseenRepository,
    /// A `403` with `X-Accepted-GitHub-Permissions`, GitHub's answer to a token without the
    /// operation's permission.
    MissingPermission,
    /// A `422`.
    Invalid,
    /// Any other answer.
    Other,
}

impl Case {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Case::Success => "success",
            Case::UnseenRepository => "unseen-repository",
            Case::MissingPermission => "missing-permission",
            Case::Invalid => "invalid",
            Case::Other => "other",
        }
    }

    fn parse(s: &str) -> Option<Case> {
        [
            Case::Success,
            Case::UnseenRepository,
            Case::MissingPermission,
            Case::Invalid,
            Case::Other,
        ]
        .into_iter()
        .find(|c| c.as_str() == s)
    }
}

/// One recorded answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The route as `docs/inventory/rest-routes.txt` writes it.
    pub route: String,
    pub status: u16,
    pub case: Case,
    /// The answer's variant, as [`facts`] reads it.
    pub facts: Vec<String>,
}

fn member(out: &mut Vec<String>, name: &str, value: &Value) {
    let empty = match value {
        Value::Null => return,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::Bool(_) | Value::Number(_) => false,
    };
    out.push(name.to_owned());
    if empty {
        out.push(format!("{name}=empty"));
    }
    if value == &Value::Bool(true) {
        out.push(format!("{name}=true"));
    }
}

/// The variant of one exchange, read from the request and the answer only, by what plan
/// section 4.8 tells variants apart:
///
/// - `accept=<value>`: the request's `Accept`, empty when it sent none;
/// - `if-none-match` and `range`: the request sent that header;
/// - `trailing-slash`: the request path ends in `/`, as go-github's `GetContents` sends the
///   root path;
/// - `query.<name>`: the query holds that parameter;
/// - `body.<member>` and `body.<member>.<member>`: the JSON request body has that member, or
///   that member of an object member, with a value other than `null`; and `<that fact>=empty`
///   when the value is `""`, `[]` or `{}`, `<that fact>=true` when it is `true`;
/// - `media=<type>`: the answer's media type without parameters, empty when it has none;
/// - `json=array` and `json=object`: the shape of a JSON answer;
/// - `link`: the answer has a `Link` header;
/// - `error`: the answer, JSON or form-encoded, has an `error` member, as OAuth answers a
///   failure.
#[must_use]
pub fn facts(x: &Exchange<'_>) -> Vec<String> {
    let mut out = vec![format!("accept={}", header(x.request_headers, "accept"))];
    for name in ["if-none-match", "range"] {
        if x.request_headers.contains_key(name) {
            out.push(name.to_owned());
        }
    }
    if x.path.len() > 1 && x.path.ends_with('/') {
        out.push("trailing-slash".to_owned());
    }
    out.extend(
        x.query
            .split('&')
            .filter_map(|kv| kv.split('=').next())
            .filter(|k| !k.is_empty())
            .map(|k| format!("query.{k}")),
    );
    if let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(x.request_body) {
        for (name, value) in &body {
            member(&mut out, &format!("body.{name}"), value);
            if let Value::Object(inner) = value {
                for (inner, value) in inner {
                    member(&mut out, &format!("body.{name}.{inner}"), value);
                }
            }
        }
    }
    let media = media_type(header(x.headers, "content-type"));
    out.push(format!("media={media}"));
    if is_json(media) {
        match serde_json::from_slice::<Value>(x.body) {
            Ok(Value::Array(_)) => out.push("json=array".to_owned()),
            Ok(Value::Object(o)) => {
                out.push("json=object".to_owned());
                if o.contains_key("error") {
                    out.push("error".to_owned());
                }
            }
            _ => {}
        }
    } else if media == "application/x-www-form-urlencoded"
        && String::from_utf8_lossy(x.body)
            .split('&')
            .any(|kv| kv.split('=').next() == Some("error"))
    {
        out.push("error".to_owned());
    }
    if !header(x.headers, "link").is_empty() {
        out.push("link".to_owned());
    }
    out.into_iter()
        .map(|f| f.replace(['\t', '\n', '\r'], " "))
        .collect()
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Recorder {
    file: Option<PathBuf>,
}

impl Recorder {
    pub(crate) fn new(lookup: &dyn Fn(&str) -> Option<String>) -> Recorder {
        Recorder {
            file: lookup(DIR_ENV)
                .filter(|d| !d.is_empty())
                .map(|d| PathBuf::from(d).join(format!("{}.tsv", std::process::id()))),
        }
    }

    pub(crate) fn from_process_env() -> Recorder {
        Recorder::new(&|name| std::env::var(name).ok())
    }

    pub(crate) fn record(&self, checked: &Checked, case: Case, facts: &[String]) {
        let Some(file) = &self.file else { return };
        let mut line = format!("{}\t{}\t{}", checked.route, checked.status, case.as_str());
        for fact in facts {
            line.push('\t');
            line.push_str(fact);
        }
        line.push('\n');
        append(file, &line);
    }

    pub(crate) fn record_webhook(&self, delivery: &Delivery, payload: &Value, store: &Store) {
        let Some(file) = &self.file else { return };
        let Ok(hook_id) = delivery.header("x-github-hook-id").parse() else {
            return;
        };
        let target = delivery.header("x-github-hook-installation-target-type");
        let signed = store
            .view(|tx| {
                let secret = if target == "integration" {
                    tx.table::<Apps>()
                        .get(hook_id)
                        .and_then(|app| app.webhook.as_ref()?.secret.clone())
                } else {
                    tx.try_table::<Hooks>()
                        .and_then(|hooks| hooks.get(hook_id))
                        .and_then(|hook| hook.secret.clone())
                };
                Ok(secret.map(|secret| delivery.signed_with(&secret)))
            })
            .unwrap_or_else(|e| panic!("read the webhook secret: {e}"));
        if let Some(signed) = signed {
            assert!(signed, "invalid signature for {target} hook {hook_id}");
            let event = delivery.header("x-github-event");
            let action = payload["action"].as_str().unwrap_or_default();
            append(
                &file.with_extension("webhooks"),
                &format!("{event}\t{action}\n"),
            );
        }
    }
}

fn append(file: &Path, line: &str) {
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if let Err(e) = written {
        panic!("record a checked exchange in {}: {e}", file.display());
    }
}

fn files<'a>(dir: &'a Path, extension: &'a str) -> impl Iterator<Item = PathBuf> + 'a {
    std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| {
            entry
                .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
                .path()
        })
        .filter(move |path| path.extension().is_some_and(|ext| ext == extension))
}

/// Every checked answer recorded under `dir`.
///
/// # Panics
///
/// When `dir` or a file in it cannot be read, or a line is not
/// `<route>\t<status>\t<case>[\t<fact>]...`.
#[must_use]
pub fn read(dir: &Path) -> Vec<Record> {
    let mut out = Vec::new();
    for path in files(dir, "tsv") {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for line in text.lines() {
            let bad = || panic!("a bad line in {}: {line}", path.display());
            let mut fields = line.split('\t');
            let (Some(route), Some(status), Some(case)) =
                (fields.next(), fields.next(), fields.next())
            else {
                bad()
            };
            out.push(Record {
                route: route.to_owned(),
                status: status.parse().unwrap_or_else(|_| bad()),
                case: Case::parse(case).unwrap_or_else(|| bad()),
                facts: fields.map(str::to_owned).collect(),
            });
        }
    }
    out
}

/// Each signed webhook that passed the delivery checks, as an event and an optional action.
///
/// # Panics
///
/// When `dir` or a webhook record cannot be read.
#[must_use]
pub fn read_webhooks(dir: &Path) -> BTreeSet<(String, Option<String>)> {
    let mut out = BTreeSet::new();
    for path in files(dir, "webhooks") {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for line in text.lines() {
            let (event, action) = line
                .split_once('\t')
                .unwrap_or_else(|| panic!("a bad line in {}: {line}", path.display()));
            out.insert((
                event.to_owned(),
                (!action.is_empty()).then(|| action.to_owned()),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use http::HeaderMap;

    use super::*;

    #[test]
    fn records_go_to_the_directory_only_when_it_is_set() {
        // Given
        let dir = tempfile::TempDir::new().unwrap();
        let set = dir.path().to_string_lossy().into_owned();
        let on = Recorder::new(&|n| (n == DIR_ENV).then(|| set.clone()));
        let off = Recorder::new(&|_| None);
        let checked = Checked {
            route: "GET /repos/{owner}/{repo}".to_owned(),
            status: 404,
        };
        let facts = vec!["accept=application/json".to_owned(), "link".to_owned()];
        // When
        on.record(&checked, Case::UnseenRepository, &facts);
        on.record(&checked, Case::Other, &[]);
        off.record(&checked, Case::Other, &[]);
        // Then
        let record = |case, facts| Record {
            route: checked.route.clone(),
            status: 404,
            case,
            facts,
        };
        assert_eq!(
            read(dir.path()),
            vec![
                record(Case::UnseenRepository, facts.clone()),
                record(Case::Other, Vec::new())
            ]
        );
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(n, v)| (http::HeaderName::from_static(n), v.parse().unwrap()))
            .collect()
    }

    #[test]
    fn the_variant_comes_from_the_request_and_the_answer() {
        // Given a conditional raw request with a body and query, and a paged JSON list answer
        let request = headers(&[
            ("accept", "application/vnd.github.v3.sha"),
            ("if-none-match", "\"abc\""),
        ]);
        let answer = headers(&[
            ("content-type", "application/json; charset=utf-8"),
            ("link", "<http://x/y?page=2>; rel=\"next\""),
        ]);
        let x = Exchange {
            method: "POST",
            path: "/repos/o/r/contents/",
            query: "per_page=3&page",
            request_headers: &request,
            request_body:
                br#"{"actions":[{"label":"a"}],"inputs":{},"draft":false,"reviewers":null,
                "return_run_details":true,"output":{"annotations":[1],"title":""}}"#,
            status: 200,
            headers: &answer,
            body: b"[]",
        };

        // When its facts are read
        let mut facts = facts(&x);

        // Then each request and answer trait is one fact, and a null body member is absent
        facts.sort_unstable();
        assert_eq!(
            facts,
            [
                "accept=application/vnd.github.v3.sha",
                "body.actions",
                "body.draft",
                "body.inputs",
                "body.inputs=empty",
                "body.output",
                "body.output.annotations",
                "body.output.title",
                "body.output.title=empty",
                "body.return_run_details",
                "body.return_run_details=true",
                "if-none-match",
                "json=array",
                "link",
                "media=application/json",
                "query.page",
                "query.per_page",
                "trailing-slash",
            ]
        );
    }

    #[test]
    fn an_oauth_failure_is_an_error_answer_in_either_encoding() {
        let empty = HeaderMap::new();
        for (media, body) in [
            (
                "application/json",
                &br#"{"error":"bad_verification_code"}"#[..],
            ),
            (
                "application/x-www-form-urlencoded",
                &b"error=bad_verification_code&error_description=x"[..],
            ),
        ] {
            let answer = headers(&[("content-type", media)]);
            let x = Exchange {
                method: "POST",
                path: "/login/oauth/access_token",
                query: "",
                request_headers: &empty,
                request_body: b"",
                status: 200,
                headers: &answer,
                body,
            };
            assert!(facts(&x).contains(&"error".to_owned()), "{media}");
        }
    }
}
