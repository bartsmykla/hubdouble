use hubdouble_hooks::emit;
use hubdouble_model::{ApiError, Event, Issue, Review, ReviewComment, ReviewThread};
use hubdouble_render::Urls;
use hubdouble_store::{Repos, ReviewComments, TxMut};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::view::View;

pub fn object(value: &impl Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|e| ApiError::internal(e.to_string()))
}

fn remove_fields(value: &mut Value, fields: &[&str]) {
    if let Some(map) = value.as_object_mut() {
        for &field in fields {
            map.remove(field);
        }
    }
}

pub fn webhook_team(value: &mut Value) {
    remove_fields(value, &["type", "organization_id"]);
    if let Some(parent) = value.get_mut("parent") {
        remove_fields(parent, &["type", "organization_id"]);
    }
}

fn pull_request(mut value: Value, detailed: bool) -> Value {
    if let Some(teams) = value["requested_teams"].as_array_mut() {
        teams.iter_mut().for_each(webhook_team);
    }
    if !detailed {
        remove_fields(
            &mut value,
            &[
                "additions",
                "changed_files",
                "comments",
                "commits",
                "deletions",
                "maintainer_can_modify",
                "mergeable",
                "mergeable_state",
                "merged",
                "merged_by",
                "rebaseable",
                "review_comments",
            ],
        );
    }
    value
}

pub fn send(
    tx: &mut TxMut<'_>,
    issue: &Issue,
    sender: i64,
    name: &'static str,
    action: &'static str,
    payload: impl FnOnce(&View<'_>) -> Result<Map<String, Value>, ApiError>,
) -> Result<(), ApiError> {
    let urls = Urls::new(tx.config(), "");
    let repo = tx
        .table::<Repos>()
        .get(issue.repo_id)
        .ok_or_else(ApiError::not_found)?;
    let view = View::new(tx, &urls, repo)?;
    let mut payload = payload(&view)?;
    payload.insert(
        "pull_request".to_owned(),
        pull_request(view.pull(issue)?, name == "pull_request"),
    );
    if name == "pull_request" {
        payload.insert("number".to_owned(), Value::from(issue.number));
    }
    emit(
        tx,
        Event {
            name,
            action: Some(action),
            repo_id: Some(repo.id),
            org_id: None,
            sender_id: sender,
            app_id: None,
            installation_id: None,
            payload,
        },
    );
    Ok(())
}

pub fn review(
    tx: &mut TxMut<'_>,
    issue: &Issue,
    sender: i64,
    review: &Review,
    action: &'static str,
    previous_body: Option<&str>,
) -> Result<(), ApiError> {
    send(tx, issue, sender, "pull_request_review", action, |view| {
        let mut rendered = view.review(review, issue.number);
        rendered.state = match review.state {
            hubdouble_model::ReviewState::Pending => "pending",
            hubdouble_model::ReviewState::Commented => "commented",
            hubdouble_model::ReviewState::Approved => "approved",
            hubdouble_model::ReviewState::ChangesRequested => "changes_requested",
            hubdouble_model::ReviewState::Dismissed => "dismissed",
        };
        let mut payload = Map::from_iter([("review".to_owned(), object(&rendered)?)]);
        if let Some(body) = previous_body {
            payload.insert("changes".to_owned(), json!({"body": {"from": body}}));
        }
        Ok(payload)
    })
}

pub fn comment(
    tx: &mut TxMut<'_>,
    issue: &Issue,
    sender: i64,
    comment: &ReviewComment,
    action: &'static str,
    previous_body: Option<&str>,
) -> Result<(), ApiError> {
    send(
        tx,
        issue,
        sender,
        "pull_request_review_comment",
        action,
        |view| {
            let mut payload = Map::from_iter([(
                "comment".to_owned(),
                comment_object(view, comment, issue.number)?,
            )]);
            if let Some(body) = previous_body {
                payload.insert("changes".to_owned(), json!({"body": {"from": body}}));
            }
            Ok(payload)
        },
    )
}

pub fn thread(
    tx: &mut TxMut<'_>,
    issue: &Issue,
    sender: i64,
    thread: &ReviewThread,
) -> Result<(), ApiError> {
    let action = if thread.is_resolved {
        "resolved"
    } else {
        "unresolved"
    };
    send(
        tx,
        issue,
        sender,
        "pull_request_review_thread",
        action,
        |view| {
            let comments: Vec<_> = view
                .tx
                .table::<ReviewComments>()
                .children(issue.id)
                .filter(|comment| comment.thread_id == thread.id)
                .filter(|comment| crate::comments::visible(view.tx, None, comment))
                .map(|comment| comment_object(view, comment, issue.number))
                .collect::<Result<_, _>>()?;
            Ok(Map::from_iter([(
                "thread".to_owned(),
                json!({ "node_id": thread.node_id, "comments": comments }),
            )]))
        },
    )
}

fn comment_object(
    view: &View<'_>,
    comment: &ReviewComment,
    number: i64,
) -> Result<Value, ApiError> {
    let mut value = object(&view.comment(comment, number))?;
    for field in ["position", "line"] {
        if value.get(field).is_none() {
            value[field] = Value::Null;
        }
    }
    for field in ["original_position", "original_line"] {
        if value.get(field).is_none() {
            value[field] = Value::from(0);
        }
    }
    Ok(value)
}
