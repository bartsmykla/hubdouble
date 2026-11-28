use hubdouble_model::{AccountKind, Event};
use hubdouble_store::{Accounts, ActivityEvent, ActivityEvents, Counters, Repos, Sequence, TxMut};
use serde_json::{Map, Value, json};

const EXPIRED_EVENTS_PER_WRITE: usize = 64;

const ISSUE_ACTIONS: [&str; 8] = [
    "opened",
    "edited",
    "closed",
    "reopened",
    "assigned",
    "unassigned",
    "labeled",
    "unlabeled",
];

const PULL_ACTIONS: [&str; 11] = [
    "opened",
    "edited",
    "closed",
    "reopened",
    "assigned",
    "unassigned",
    "review_requested",
    "review_request_removed",
    "labeled",
    "unlabeled",
    "synchronize",
];

fn kind(e: &Event) -> Option<&'static str> {
    let action = e.action.unwrap_or_default();
    Some(match e.name {
        "push" if e.payload.get("deleted") != Some(&Value::Bool(true)) => "PushEvent",
        "create" => "CreateEvent",
        "delete" => "DeleteEvent",
        "issues" if ISSUE_ACTIONS.contains(&action) => "IssuesEvent",
        "issue_comment" if action == "created" => "IssueCommentEvent",
        "pull_request" if PULL_ACTIONS.contains(&action) => "PullRequestEvent",
        "pull_request_review" if action == "submitted" => "PullRequestReviewEvent",
        "pull_request_review_comment" if action == "created" => "PullRequestReviewCommentEvent",
        "release" if action == "published" => "ReleaseEvent",
        "member" if action == "added" => "MemberEvent",
        "repository" if action == "publicized" => "PublicEvent",
        _ => return None,
    })
}

fn pick(p: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    keys.iter()
        .filter_map(|k| p.get(*k).map(|v| ((*k).to_owned(), v.clone())))
        .collect()
}

fn full_ref(p: &Map<String, Value>) -> Value {
    let name = p.get("ref").and_then(Value::as_str).unwrap_or_default();
    let space = match p.get("ref_type").and_then(Value::as_str) {
        Some("tag") => "tags",
        _ => "heads",
    };
    Value::String(format!("refs/{space}/{name}"))
}

fn side(side: &Value) -> Value {
    let repo = &side["repo"];
    json!({
        "ref": side["ref"],
        "sha": side["sha"],
        "repo": {"id": repo["id"], "url": repo["url"], "name": repo["full_name"]},
    })
}

fn minimal_pull(pull: &Value) -> Value {
    json!({
        "id": pull["id"],
        "number": pull["number"],
        "url": pull["url"],
        "head": side(&pull["head"]),
        "base": side(&pull["base"]),
    })
}

fn with_action(mut payload: Map<String, Value>, action: &str) -> Map<String, Value> {
    payload.insert("action".to_owned(), Value::from(action));
    payload
}

fn payload(kind: &str, e: &Event, repo_id: i64, push_id: i64) -> Map<String, Value> {
    let p = &e.payload;
    let action = e.action.unwrap_or_default();
    let pull = || minimal_pull(p.get("pull_request").unwrap_or(&Value::Null));
    match kind {
        "PushEvent" => Map::from_iter([
            ("repository_id".to_owned(), Value::from(repo_id)),
            ("push_id".to_owned(), Value::from(push_id)),
            ("ref".to_owned(), p.get("ref").cloned().unwrap_or_default()),
            (
                "head".to_owned(),
                p.get("after").cloned().unwrap_or_default(),
            ),
            (
                "before".to_owned(),
                p.get("before").cloned().unwrap_or_default(),
            ),
        ]),
        "CreateEvent" | "DeleteEvent" => {
            let mut out = pick(
                p,
                &[
                    "ref",
                    "ref_type",
                    "master_branch",
                    "description",
                    "pusher_type",
                ],
            );
            out.insert("full_ref".to_owned(), full_ref(p));
            out
        }
        "IssuesEvent" => with_action(pick(p, &["issue", "assignee", "label"]), action),
        "IssueCommentEvent" => with_action(pick(p, &["issue", "comment"]), action),
        "PullRequestEvent" => {
            let mut out = with_action(pick(p, &["number", "assignee", "label"]), action);
            out.insert("pull_request".to_owned(), pull());
            out
        }
        "PullRequestReviewEvent" => {
            let mut review = p.get("review").cloned().unwrap_or_default();
            if let Some(state) = review.get("state").and_then(Value::as_str) {
                review["state"] = Value::from(state.to_ascii_uppercase());
            }
            Map::from_iter([
                ("action".to_owned(), Value::from("created")),
                ("review".to_owned(), review),
                ("pull_request".to_owned(), pull()),
            ])
        }
        "PullRequestReviewCommentEvent" => {
            let mut out = with_action(pick(p, &["comment"]), action);
            out.insert("pull_request".to_owned(), pull());
            out
        }
        "ReleaseEvent" => with_action(pick(p, &["release"]), action),
        "MemberEvent" => with_action(pick(p, &["member"]), action),
        _ => Map::new(),
    }
}

pub(crate) fn record(tx: &mut TxMut<'_>, e: &Event) {
    if e.app_id.is_some() || e.installation_id.is_some() {
        return;
    }
    let (Some(repo_id), Some(kind)) = (e.repo_id, kind(e)) else {
        return;
    };
    let Some(repo) = tx.table::<Repos>().get(repo_id).cloned() else {
        return;
    };
    let Some(owner) = tx.table::<Accounts>().get(repo.owner_id).cloned() else {
        return;
    };
    let id = tx.table_mut::<Counters>().next(Sequence::Event);
    let row = ActivityEvent {
        id,
        kind,
        actor_id: e.sender_id,
        repo_id,
        repo_name: format!("{}/{}", owner.login, repo.name),
        org_id: (owner.kind == AccountKind::Organization).then_some(owner.id),
        payload: Value::Object(payload(kind, e, repo_id, id)),
        public: !repo.is_private(),
        created_at: tx.now(),
    };
    let oldest = row
        .created_at
        .plus_seconds(-ActivityEvent::RETENTION_SECONDS);
    let mut events = tx.table_mut::<ActivityEvents>();
    let expired: Vec<i64> = events
        .iter()
        .take(EXPIRED_EVENTS_PER_WRITE)
        .filter(|e| e.created_at < oldest)
        .map(|e| e.id)
        .collect();
    events.remove_all(expired);
    let _ = events.insert(row);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &'static str, action: Option<&'static str>, payload: Value) -> Event {
        Event {
            name,
            action,
            repo_id: Some(1),
            org_id: None,
            sender_id: 2,
            app_id: None,
            installation_id: None,
            payload: match payload {
                Value::Object(map) => map,
                _ => Map::new(),
            },
        }
    }

    #[test]
    fn webhook_events_map_to_the_event_types_of_the_events_api() {
        let cases = [
            (
                event("push", None, json!({"deleted": false})),
                Some("PushEvent"),
            ),
            (event("push", None, json!({"deleted": true})), None),
            (event("create", None, json!({})), Some("CreateEvent")),
            (
                event("issues", Some("opened"), json!({})),
                Some("IssuesEvent"),
            ),
            (event("issues", Some("deleted"), json!({})), None),
            (
                event("issue_comment", Some("created"), json!({})),
                Some("IssueCommentEvent"),
            ),
            (event("issue_comment", Some("edited"), json!({})), None),
            (
                event("pull_request", Some("synchronize"), json!({})),
                Some("PullRequestEvent"),
            ),
            (event("pull_request", Some("locked"), json!({})), None),
            (
                event("pull_request_review", Some("submitted"), json!({})),
                Some("PullRequestReviewEvent"),
            ),
            (
                event("repository", Some("publicized"), json!({})),
                Some("PublicEvent"),
            ),
            (event("repository", Some("renamed"), json!({})), None),
            (event("check_run", Some("created"), json!({})), None),
        ];
        for (e, expected) in cases {
            assert_eq!(kind(&e), expected, "{} {:?}", e.name, e.action);
        }
    }

    #[test]
    fn a_push_payload_has_the_short_form_of_the_events_api() {
        // Given a push webhook payload
        let e = event(
            "push",
            None,
            json!({"ref": "refs/heads/main", "before": "a", "after": "b", "commits": []}),
        );

        // When it becomes an event payload
        let p = payload("PushEvent", &e, 7, 9);

        // Then it carries the five fields of GitHub's PushEvent and nothing else
        assert_eq!(
            Value::Object(p),
            json!({"repository_id": 7, "push_id": 9, "ref": "refs/heads/main",
                   "head": "b", "before": "a"})
        );
    }

    #[test]
    fn a_tag_creation_gets_the_full_tag_ref() {
        let e = event(
            "create",
            None,
            json!({"ref": "v1", "ref_type": "tag", "master_branch": "main",
                   "description": null, "pusher_type": "user"}),
        );
        let p = payload("CreateEvent", &e, 7, 9);
        assert_eq!(p["full_ref"], "refs/tags/v1");
        assert_eq!(p["ref"], "v1");
    }

    #[test]
    fn a_pull_request_payload_keeps_only_the_minimal_pull_request() {
        // Given a pull request webhook payload with a full pull request
        let side = |r: &str| {
            json!({"ref": r, "sha": "s", "label": "x",
                   "repo": {"id": 1, "url": "u", "full_name": "o/r", "name": "r"}})
        };
        let e = event(
            "pull_request",
            Some("opened"),
            json!({"number": 3, "pull_request": {"id": 5, "number": 3, "url": "p",
                   "title": "t", "head": side("feature"), "base": side("main")}}),
        );

        // When it becomes an event payload
        let p = payload("PullRequestEvent", &e, 1, 9);

        // Then the pull request has the minimal form, with the repository's full name
        assert_eq!(
            Value::Object(p),
            json!({"action": "opened", "number": 3, "pull_request": {
                "id": 5, "number": 3, "url": "p",
                "head": {"ref": "feature", "sha": "s", "repo": {"id": 1, "url": "u", "name": "o/r"}},
                "base": {"ref": "main", "sha": "s", "repo": {"id": 1, "url": "u", "name": "o/r"}},
            }})
        );
    }
}
