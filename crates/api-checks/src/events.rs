use hubdouble_git::{Git, ObjectData, ObjectId, Signature};
use hubdouble_hooks::emit;
use hubdouble_model::{ApiError, CheckRun, CheckSuite, Event, Status, node_id};
use hubdouble_render::Urls;
use hubdouble_store::{Accounts, CheckSuites, Installations, Repos, Tx, TxMut};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::view::View;

const RUN_SUITE: &[&str] = &[
    "id",
    "node_id",
    "head_branch",
    "head_sha",
    "status",
    "conclusion",
    "url",
    "before",
    "after",
    "pull_requests",
    "app",
    "created_at",
    "updated_at",
];

const SUITE: &[&str] = &[
    "id",
    "node_id",
    "head_branch",
    "head_sha",
    "status",
    "conclusion",
    "url",
    "before",
    "after",
    "pull_requests",
    "app",
    "created_at",
    "updated_at",
    "latest_check_runs_count",
    "check_runs_url",
    "head_commit",
];

#[derive(Clone, Copy)]
pub enum To {
    Subscribers,
    Owner(i64),
}

pub fn object(value: &impl Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|e| ApiError::internal(e.to_string()))
}

fn retain(value: &mut Value, keep: &[&str]) {
    if let Some(map) = value.as_object_mut() {
        map.retain(|key, _| keep.contains(&key.as_str()));
    }
}

fn webhook_app(value: &mut Value) {
    if let Some(app) = value.get_mut("app").and_then(Value::as_object_mut) {
        app.remove("client_id");
    }
}

fn send(
    tx: &mut TxMut<'_>,
    repo_id: i64,
    sender: i64,
    (name, action): (&'static str, Option<&'static str>),
    to: To,
    payload: Map<String, Value>,
) {
    let (app_id, installation_id) = match to {
        To::Subscribers => (None, None),
        To::Owner(app) => {
            let owner = tx.table::<Repos>().get(repo_id).map(|r| r.owner_id);
            match owner.and_then(|owner| tx.table::<Installations>().of(app, owner)) {
                Some(installation) => (Some(app), Some(installation.id)),
                None => return,
            }
        }
    };
    emit(
        tx,
        Event {
            name,
            action,
            repo_id: Some(repo_id),
            org_id: None,
            sender_id: sender,
            app_id,
            installation_id,
            payload,
        },
    );
}

pub fn run_payload(tx: &Tx, git: &Git, run: &CheckRun) -> Result<Value, ApiError> {
    let urls = Urls::new(tx.config(), "");
    let view = View::of(tx, git, &urls, run.repo_id)?;
    let rendered = view.run(run);
    let suite = tx
        .table::<CheckSuites>()
        .get(run.check_suite_id)
        .ok_or_else(ApiError::not_found)?;
    let mut suite_view =
        hubdouble_render::CheckSuite::new(suite, view.repository(), rendered.app.clone());
    suite_view.pull_requests.clone_from(&rendered.pull_requests);
    let mut value = object(&rendered)?;
    let mut suite = object(&suite_view)?;
    retain(&mut suite, RUN_SUITE);
    webhook_app(&mut suite);
    webhook_app(&mut value);
    value["check_suite"] = suite;
    Ok(value)
}

pub fn check_run(
    tx: &mut TxMut<'_>,
    git: &Git,
    run: &CheckRun,
    (action, sender, to): (&'static str, i64, To),
    requested: Option<&str>,
) -> Result<(), ApiError> {
    let mut payload = Map::from_iter([("check_run".to_owned(), run_payload(tx, git, run)?)]);
    if let Some(identifier) = requested {
        payload.insert(
            "requested_action".to_owned(),
            json!({ "identifier": identifier }),
        );
    }
    send(
        tx,
        run.repo_id,
        sender,
        ("check_run", Some(action)),
        to,
        payload,
    );
    Ok(())
}

pub fn check_suite(
    tx: &mut TxMut<'_>,
    git: &Git,
    suite: &CheckSuite,
    (action, sender, to): (&'static str, i64, To),
) -> Result<(), ApiError> {
    let urls = Urls::new(tx.config(), "");
    let mut value = object(&View::of(tx, git, &urls, suite.repo_id)?.suite(suite))?;
    retain(&mut value, SUITE);
    webhook_app(&mut value);
    if action != "rerequested" {
        value["rerequestable"] = Value::Bool(suite.rerequestable);
        value["runs_rerequestable"] = Value::Bool(crate::view::runs_rerequestable(suite));
    }
    send(
        tx,
        suite.repo_id,
        sender,
        ("check_suite", Some(action)),
        to,
        Map::from_iter([("check_suite".to_owned(), value)]),
    );
    Ok(())
}

fn person(signature: &Signature) -> Value {
    json!({ "name": signature.name, "email": signature.email, "date": signature.time })
}

fn commit_object(view: &View<'_>, sha: &str) -> Result<Value, ApiError> {
    let id = sha.parse().map_err(|_| ApiError::not_found())?;
    let ObjectData::Commit(commit) = view.git.object(view.tx, view.repo.id, id)?.data else {
        return Err(ApiError::not_found());
    };
    let (api, web, full) = (&view.urls.api, &view.urls.web, &view.name);
    let account = |signature: &Signature| {
        view.tx
            .table::<Accounts>()
            .iter()
            .find(|a| {
                a.email
                    .as_deref()
                    .is_some_and(|e| e.eq_ignore_ascii_case(&signature.email))
            })
            .and_then(|a| view.user(a.id))
    };
    let signature = view.git.signature(view.repo.id, id, &commit);
    let reason = signature.as_ref().map_or_else(
        || view.git.verification(&commit).as_str(),
        |s| s.state.as_str(),
    );
    let parents: Vec<Value> = commit
        .parents
        .iter()
        .map(|parent| {
            json!({
                "sha": parent.to_string(),
                "url": format!("{api}/repos/{full}/commits/{parent}"),
                "html_url": format!("{web}/{full}/commit/{parent}"),
            })
        })
        .collect();
    Ok(json!({
        "sha": sha,
        "node_id": node_id::encode("C", &[
            node_id::Part::Int(0),
            node_id::Part::Int(view.repo.id),
            node_id::Part::Str(sha.to_owned()),
        ]),
        "commit": {
            "author": person(&commit.author),
            "committer": person(&commit.committer),
            "message": commit.message,
            "tree": {
                "sha": commit.tree.to_string(),
                "url": format!("{api}/repos/{full}/git/trees/{}", commit.tree),
            },
            "url": format!("{api}/repos/{full}/git/commits/{sha}"),
            "comment_count": 0,
            "verification": {
                "verified": reason == "valid",
                "reason": reason,
                "signature": signature.as_ref().map(|s| s.signature.clone()),
                "payload": signature.map(|s| s.payload),
            },
        },
        "url": format!("{api}/repos/{full}/commits/{sha}"),
        "html_url": format!("{web}/{full}/commit/{sha}"),
        "comments_url": format!("{api}/repos/{full}/commits/{sha}/comments"),
        "author": account(&commit.author),
        "committer": account(&commit.committer),
        "parents": parents,
    }))
}

fn branches(view: &View<'_>, sha: &str) -> Vec<Value> {
    let (api, full) = (&view.urls.api, &view.name);
    let Ok(commit) = sha.parse::<ObjectId>() else {
        return Vec::new();
    };
    view.git
        .refs(view.tx, view.repo.id, "refs/heads/")
        .into_iter()
        .filter(|r| {
            view.git
                .is_ancestor(view.tx, view.repo.id, commit, r.target)
                .unwrap_or(false)
        })
        .take(10)
        .map(|r| {
            let head = r.target;
            json!({
                "name": r.name.trim_start_matches("refs/heads/"),
                "commit": { "sha": head, "url": format!("{api}/repos/{full}/commits/{head}") },
                "protected": false,
            })
        })
        .collect()
}

pub fn status(tx: &mut TxMut<'_>, git: &Git, status: &Status) -> Result<(), ApiError> {
    let urls = Urls::new(tx.config(), "");
    let payload = {
        let view = View::of(tx, git, &urls, status.repo_id)?;
        let owner = tx
            .table::<Accounts>()
            .get(view.repo.owner_id)
            .ok_or_else(ApiError::not_found)?;
        let rendered = view.status(status);
        Map::from_iter([
            ("id".to_owned(), json!(status.id)),
            ("sha".to_owned(), json!(status.sha)),
            (
                "name".to_owned(),
                json!(format!("{}/{}", owner.login, view.repo.name)),
            ),
            ("avatar_url".to_owned(), json!(rendered.avatar_url)),
            ("target_url".to_owned(), json!(status.target_url)),
            ("context".to_owned(), json!(status.context)),
            ("description".to_owned(), json!(status.description)),
            ("state".to_owned(), json!(status.state.as_str())),
            ("commit".to_owned(), commit_object(&view, &status.sha)?),
            (
                "branches".to_owned(),
                Value::from(branches(&view, &status.sha)),
            ),
            ("created_at".to_owned(), json!(status.created_at)),
            ("updated_at".to_owned(), json!(status.updated_at)),
        ])
    };
    send(
        tx,
        status.repo_id,
        status.creator_id,
        ("status", None),
        To::Subscribers,
        payload,
    );
    Ok(())
}
