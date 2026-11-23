use std::sync::Arc;

use http::StatusCode;
use hubdouble_auth::Actor;
use hubdouble_git::{Git, ObjectId};
use hubdouble_model::{
    Annotation, AnnotationLevel, ApiError, CheckAction, CheckRun, Conclusion, Repo, RunStatus,
    Timestamp, node_id,
};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Apps, CheckRuns, TxMut};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::events::{self, To};
use crate::view::{self, View, latest, no_commit};
use crate::{invalid, missing, page_object, suites};

const MAX_ANNOTATIONS: usize = 50;
const MAX_ACTIONS: usize = 3;

#[derive(Deserialize)]
pub struct AnnotationInput {
    path: String,
    start_line: i64,
    end_line: i64,
    start_column: Option<i64>,
    end_column: Option<i64>,
    annotation_level: String,
    message: String,
    title: Option<String>,
    raw_details: Option<String>,
}

#[derive(Deserialize)]
pub struct OutputInput {
    title: Option<String>,
    summary: Option<String>,
    text: Option<String>,
    #[serde(default)]
    annotations: Vec<AnnotationInput>,
}

#[derive(Deserialize)]
pub struct ActionInput {
    label: String,
    description: String,
    identifier: String,
}

#[derive(Deserialize, Default)]
pub struct RunInput {
    pub name: Option<String>,
    pub head_sha: Option<String>,
    details_url: Option<String>,
    external_id: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    started_at: Option<Timestamp>,
    completed_at: Option<Timestamp>,
    output: Option<OutputInput>,
    actions: Option<Vec<ActionInput>>,
}

pub fn app_actor(actor: &Actor) -> Result<(i64, i64), ApiError> {
    match actor {
        Actor::Installation { app, bot, .. } => Ok((app.id, bot.id)),
        _ => Err(ApiError::forbidden(
            "You must authenticate via a GitHub App.",
        )),
    }
}

fn owner(actor: &Actor, run: &CheckRun) -> Result<i64, ApiError> {
    let (app, bot) = app_actor(actor)?;
    if app == run.app_id {
        Ok(bot)
    } else {
        Err(ApiError::forbidden("Invalid app_id"))
    }
}

fn modifier(actor: &Actor, run: &CheckRun) -> Result<i64, ApiError> {
    let (app, bot) = app_actor(actor)?;
    if app == run.app_id {
        return Ok(bot);
    }
    Err(ApiError::forbidden(format!(
        "Invalid app_id `{}` - check run can only be modified by the GitHub App that created it.",
        run.app_id
    ))
    .with_docs("https://docs.github.com/rest/checks/runs#update-a-check-run"))
}

/// Reads one annotation of a check run's `output.annotations`, as the Checks API takes it.
///
/// # Errors
///
/// Returns `422` for an unknown `annotation_level`.
pub fn annotation(input: AnnotationInput) -> Result<Annotation, ApiError> {
    Ok(Annotation {
        path: input.path,
        start_line: input.start_line,
        end_line: input.end_line,
        start_column: input.start_column,
        end_column: input.end_column,
        annotation_level: AnnotationLevel::from_rest(&input.annotation_level)
            .ok_or_else(|| invalid("annotation_level"))?,
        message: input.message,
        title: input.title,
        raw_details: input.raw_details,
    })
}

fn validate(input: &RunInput, creating: bool) -> Result<(), ApiError> {
    if let Some(output) = &input.output {
        if output.summary.is_none() {
            return Err(missing("summary"));
        }
        if creating && output.title.is_none() {
            return Err(missing("title"));
        }
        if output.annotations.len() > MAX_ANNOTATIONS {
            return Err(invalid("annotations"));
        }
    }
    if let Some(actions) = &input.actions {
        if actions.len() > MAX_ACTIONS {
            return Err(invalid("actions"));
        }
        for action in actions {
            if action.label.chars().count() > 20
                || action.description.chars().count() > 40
                || action.identifier.chars().count() > 20
            {
                return Err(invalid("actions"));
            }
        }
    }
    Ok(())
}

fn apply(run: &mut CheckRun, input: RunInput, now: Timestamp) -> Result<(), ApiError> {
    if let Some(name) = input.name {
        run.name = name;
    }
    if let Some(url) = input.details_url {
        run.details_url = Some(url);
    }
    if let Some(id) = input.external_id {
        run.external_id = Some(id);
    }
    if let Some(at) = input.started_at {
        run.started_at = Some(at);
    }
    if let Some(status) = input.status {
        run.status = RunStatus::from_rest(&status).ok_or_else(|| invalid("status"))?;
        if run.status != RunStatus::Completed {
            run.conclusion = None;
            run.completed_at = None;
        }
    }
    if let Some(conclusion) = input.conclusion {
        run.conclusion =
            Some(Conclusion::from_rest(&conclusion).ok_or_else(|| invalid("conclusion"))?);
        run.status = RunStatus::Completed;
    }
    if run.status == RunStatus::Completed {
        if run.conclusion.is_none() {
            return Err(missing("conclusion"));
        }
        run.completed_at = input.completed_at.or(run.completed_at).or(Some(now));
    }
    if let Some(output) = input.output {
        if output.title.is_some() {
            run.output.title = output.title;
        }
        run.output.summary = output.summary;
        if output.text.is_some() {
            run.output.text = output.text;
        }
        for input in output.annotations {
            run.output.annotations.push(annotation(input)?);
        }
    }
    if let Some(actions) = input.actions {
        run.actions = actions
            .into_iter()
            .map(|a| CheckAction {
                label: a.label,
                description: a.description,
                identifier: a.identifier,
            })
            .collect();
    }
    Ok(())
}

/// Creates a run in the newest suite of `app` on its head, making the suite when the App has
/// none there, and emits `check_run.created`, `check_run.completed` and `check_suite.completed`
/// as the run's state calls for.
pub fn create_run(
    tx: &mut TxMut<'_>,
    git: &Git,
    (repo, app, sender): (&Repo, i64, i64),
    input: RunInput,
    id: Option<i64>,
    now: Timestamp,
) -> Result<Arc<CheckRun>, ApiError> {
    validate(&input, true)?;
    let mut input = input;
    let name = input
        .name
        .take()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| missing("name"))?;
    let head = input.head_sha.take().ok_or_else(|| missing("head_sha"))?;
    let sha = view::commit(tx, git, repo.id, &head)
        .ok_or_else(|| no_commit(&head))?
        .to_string();
    let homepage = tx
        .table::<Apps>()
        .get(app)
        .map(|a| a.external_url.clone())
        .ok_or_else(|| ApiError::unprocessable("No such App"))?;
    let suite = suites::for_run(tx, git, repo, app, &sha, now)?;
    let id = id.unwrap_or_else(|| tx.next_id());
    let mut run = CheckRun {
        id,
        node_id: node_id::entity(node_id::CHECK_RUN, id),
        repo_id: repo.id,
        check_suite_id: suite,
        app_id: app,
        head_sha: sha,
        name,
        external_id: Some(String::new()),
        details_url: Some(homepage),
        status: RunStatus::Queued,
        conclusion: None,
        started_at: Some(now),
        completed_at: None,
        output: hubdouble_model::CheckOutput::default(),
        actions: Vec::new(),
    };
    apply(&mut run, input, now)?;
    let run = tx.table_mut::<CheckRuns>().insert(run)?;
    events::check_run(tx, git, &run, ("created", sender, To::Subscribers), None)?;
    if run.status == RunStatus::Completed {
        events::check_run(tx, git, &run, ("completed", sender, To::Subscribers), None)?;
    }
    suites::refresh(tx, git, suite, sender, now)?;
    Ok(run)
}

fn find(ctx: &Ctx<'_>) -> Result<Arc<CheckRun>, ApiError> {
    let id = ctx.param_i64("check_run_id")?;
    let repo = ctx.repo()?.id;
    ctx.tx()
        .table::<CheckRuns>()
        .get(id)
        .filter(|run| run.repo_id == repo)
        .cloned()
        .ok_or_else(ApiError::not_found)
}

fn rest_view<'c>(ctx: &'c Ctx<'_>) -> Result<View<'c>, ApiError> {
    View::new(ctx.tx(), ctx.git(), ctx.urls(), ctx.repo()?)
}

pub fn create(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let (app, bot) = app_actor(ctx.actor())?;
    let input: RunInput = ctx.body()?;
    if let Some(head) = &input.head_sha
        && head.parse::<ObjectId>().is_err()
    {
        return Err(no_commit(head));
    }
    let repo = ctx.repo()?.clone();
    let now = ctx.now();
    let (tx, git) = ctx.tx_mut_and_git()?;
    let run = create_run(tx, git, (&repo, app, bot), input, None, now)?;
    Ok(Reply::json(&rest_view(ctx)?.run(&run))?.status(StatusCode::CREATED))
}

pub fn get(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let run = find(ctx)?;
    Reply::json(&rest_view(ctx)?.run(&run))
}

pub fn update(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let run = find(ctx)?;
    let sender = modifier(ctx.actor(), &run)?;
    let input: RunInput = ctx.body()?;
    validate(&input, false)?;
    let now = ctx.now();
    let mut changed = (*run).clone();
    apply(&mut changed, input, now)?;
    let changed = Arc::new(changed);
    let (tx, git) = ctx.tx_mut_and_git()?;
    tx.table_mut::<CheckRuns>().replace(changed.clone())?;
    if changed.status == RunStatus::Completed && run.status != RunStatus::Completed {
        events::check_run(
            tx,
            git,
            &changed,
            ("completed", sender, To::Subscribers),
            None,
        )?;
    }
    suites::refresh(tx, git, changed.check_suite_id, sender, now)?;
    Reply::json(&rest_view(ctx)?.run(&changed))
}

fn parse_app_id(text: &str) -> Result<i64, ApiError> {
    text.parse().map_err(|_| invalid("app_id"))
}

pub fn list_for_ref(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?.id;
    let reference = ctx.param("ref")?;
    let sha = view::commit(ctx.tx(), ctx.git(), repo, reference)
        .ok_or_else(|| no_commit(reference))?
        .to_string();
    let query = ctx.query();
    let name = query.get("check_name");
    let status = query
        .get("status")
        .map(|s| RunStatus::from_rest(s).ok_or_else(|| invalid("status")))
        .transpose()?;
    let app = query.get("app_id").map(parse_app_id).transpose()?;
    let all = match query.get("filter") {
        None | Some("latest") => false,
        Some("all") => true,
        Some(_) => return Err(invalid("filter")),
    };
    let matching = view::runs_on(ctx.tx(), repo, &sha)
        .filter(|run| name.is_none_or(|n| run.name == n) && app.is_none_or(|a| run.app_id == a));
    let mut runs: Vec<_> = if all {
        matching.collect()
    } else {
        latest(matching)
    };
    runs.retain(|run| status.is_none_or(|s| run.status == s));
    runs.sort_unstable_by_key(|run| std::cmp::Reverse(run.id));
    let view = rest_view(ctx)?;
    let pulls = view.pulls(&sha);
    let mut apps = std::collections::BTreeMap::new();
    page_object(ctx, "check_runs", &runs, |run| {
        let app = apps
            .entry(run.app_id)
            .or_insert_with(|| view.app(run.app_id))
            .clone();
        view.run_with(run, app, pulls.clone())
    })
}

pub fn list_annotations(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let run = find(ctx)?;
    let view = rest_view(ctx)?;
    let blob = format!("{}/{}/blob/{}", ctx.urls().web, view.name, run.head_sha);
    ctx.page(&run.output.annotations, |a| -> Value {
        json!({
            "path": a.path,
            "start_line": a.start_line,
            "end_line": a.end_line,
            "start_column": a.start_column,
            "end_column": a.end_column,
            "annotation_level": a.annotation_level.as_str(),
            "title": a.title,
            "message": a.message,
            "raw_details": a.raw_details,
            "blob_href": format!("{blob}/{}", a.path),
        })
    })
}

pub fn rerequest(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let run = find(ctx)?;
    let sender = owner(ctx.actor(), &run)?;
    let now = ctx.now();
    let (tx, git) = ctx.tx_mut_and_git()?;
    events::check_run(
        tx,
        git,
        &run,
        ("rerequested", sender, To::Owner(run.app_id)),
        None,
    )?;
    suites::reset(tx, run.check_suite_id, now)?;
    Ok(Reply::json(&json!({}))?.status(StatusCode::CREATED))
}

/// Emits `check_run.requested_action` to the run's App, as GitHub does when a person presses one
/// of the run's action buttons.
pub fn requested_action(
    tx: &mut TxMut<'_>,
    git: &Git,
    run: &CheckRun,
    sender: i64,
    identifier: &str,
) -> Result<(), ApiError> {
    if !run.actions.iter().any(|a| a.identifier == identifier) {
        return Err(invalid("identifier"));
    }
    events::check_run(
        tx,
        git,
        run,
        ("requested_action", sender, To::Owner(run.app_id)),
        Some(identifier),
    )
}
