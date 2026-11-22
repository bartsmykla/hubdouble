use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use hubdouble_git::{EntryMode, Git, Object, ObjectData, ObjectId, RefChange};
use hubdouble_model::{ApiError, Repo, Timestamp, node_id};
use hubdouble_rest::{Ctx, Registry, Reply};
use hubdouble_store::{Accounts, Entity, Repos, Rows, Tx, TxMut};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ACTIONS_READ, ACTIONS_WRITE};

const DIRECTORY: &str = ".github/workflows";
const DISPATCH: &str = "workflow_dispatch";

pub(crate) type Workflows = Rows<Workflow>;

pub(crate) struct Workflow {
    pub id: i64,
    repo_id: i64,
    pub path: String,
    blob: Option<ObjectId>,
    pub definition: Option<Arc<Definition>>,
    created: Timestamp,
    updated: Timestamp,
}

impl Entity for Workflow {
    fn id(&self) -> i64 {
        self.id
    }

    fn repo_id(&self) -> Option<i64> {
        Some(self.repo_id)
    }

    fn keys(&self) -> Vec<String> {
        vec![path_key(self.repo_id, &self.path)]
    }
}

fn path_key(repo: i64, path: &str) -> String {
    format!("{repo}:{path}")
}

#[derive(Deserialize)]
pub(crate) struct Definition {
    pub name: Option<String>,
    #[serde(rename = "on", default)]
    trigger: Value,
    #[serde(default)]
    pub jobs: BTreeMap<String, JobDefinition>,
}

#[derive(Deserialize)]
pub(crate) struct JobDefinition {
    pub name: Option<String>,
    pub uses: Option<String>,
    pub environment: Option<Value>,
}

pub(crate) fn register(rest: &mut Registry) {
    rest.handle("actions/get-workflow", ACTIONS_READ, get);
    rest.handle("actions/list-repo-workflows", ACTIONS_READ, list);
    rest.handle("actions/create-workflow-dispatch", ACTIONS_WRITE, dispatch);
}

pub(crate) fn ref_updated(
    tx: &mut TxMut<'_>,
    git: &Git,
    change: &RefChange,
) -> Result<(), ApiError> {
    let Some(repo) = tx.table::<Repos>().get(change.repo_id) else {
        return Ok(());
    };
    if change.name.strip_prefix("refs/heads/") != Some(&repo.default_branch) {
        return Ok(());
    }
    let directory = |id: ObjectId| {
        git.expression(tx, repo.id, &format!("{id}:{DIRECTORY}"))
            .ok()
    };
    let current_directory = change.new.and_then(directory);
    if current_directory.as_ref().map(|tree| tree.id)
        == change.old.and_then(directory).map(|tree| tree.id)
    {
        return Ok(());
    }
    let entries = match current_directory {
        Some(Object {
            data: ObjectData::Tree(entries),
            ..
        }) => Some(entries),
        _ => None,
    };
    let old: BTreeMap<_, _> = tx
        .table::<Workflows>()
        .in_repo(repo.id)
        .map(|w| (w.path.clone(), Arc::clone(w)))
        .collect();
    let mut current = BTreeSet::new();
    let now = git.now();
    for entry in entries.iter().flat_map(|entries| entries.iter()) {
        if !matches!(entry.mode, EntryMode::File | EntryMode::Executable)
            || !matches!(entry.name.rsplit_once('.'), Some((_, "yml" | "yaml")))
        {
            continue;
        }
        let path = format!("{DIRECTORY}/{}", entry.name);
        current.insert(path.clone());
        let previous = old.get(&path);
        if previous.is_some_and(|w| w.blob == Some(entry.id)) {
            continue;
        }
        let definition = git
            .object(tx, change.repo_id, entry.id)
            .ok()
            .and_then(|object| parse(&object.data));
        let workflow = Workflow {
            id: previous.map_or_else(|| tx.next_id(), |w| w.id),
            repo_id: change.repo_id,
            path,
            blob: Some(entry.id),
            definition,
            created: previous.map_or(now, |w| w.created),
            updated: now,
        };
        tx.table_mut::<Workflows>().replace(workflow)?;
    }
    for workflow in old
        .values()
        .filter(|w| w.blob.is_some() && !current.contains(&w.path))
    {
        tx.table_mut::<Workflows>().replace(Workflow {
            id: workflow.id,
            repo_id: change.repo_id,
            path: workflow.path.clone(),
            blob: None,
            definition: None,
            created: workflow.created,
            updated: now,
        })?;
    }
    Ok(())
}

fn parse(data: &ObjectData) -> Option<Arc<Definition>> {
    let ObjectData::Blob(content) = data else {
        return None;
    };
    serde_saphyr::from_str(std::str::from_utf8(content).ok()?)
        .ok()
        .map(Arc::new)
}

pub(crate) fn find<'a>(tx: &'a Tx, repo: i64, selector: &str) -> Result<&'a Workflow, ApiError> {
    let workflows = tx.table::<Workflows>();
    let found = if let Ok(id) = selector.parse::<i64>() {
        workflows.get(id)
    } else {
        workflows.by_key(&path_key(repo, &format!("{DIRECTORY}/{selector}")))
    };
    found
        .filter(|w| w.repo_id == repo && w.blob.is_some())
        .map(Arc::as_ref)
        .ok_or_else(ApiError::not_found)
}

fn name(workflow: &Workflow) -> &str {
    workflow
        .definition
        .as_ref()
        .and_then(|w| w.name.as_deref())
        .unwrap_or(&workflow.path)
}

pub(crate) fn view(
    urls: &hubdouble_render::Urls,
    repo: &str,
    id: i64,
    path: &str,
    name: &str,
    times: (Timestamp, Timestamp),
) -> Value {
    json!({
        "id": id,
        "node_id": node_id::entity("W", id),
        "name": name,
        "path": path,
        "state": "active",
        "created_at": times.0,
        "updated_at": times.1,
        "url": format!("{}/repos/{repo}/actions/workflows/{id}", urls.api),
        "html_url": format!("{}/{repo}/blob/HEAD/{path}", urls.web),
        "badge_url": format!("{}/{repo}/actions/workflows/{}/badge.svg", urls.web, path.strip_prefix(".github/workflows/").unwrap_or(path)),
    })
}

fn render(ctx: &Ctx<'_>, repo: &str, workflow: &Workflow) -> Value {
    view(
        ctx.urls(),
        repo,
        workflow.id,
        &workflow.path,
        name(workflow),
        (workflow.created, workflow.updated),
    )
}

pub(crate) fn times(tx: &Tx, id: i64) -> Option<(Timestamp, Timestamp)> {
    tx.table::<Workflows>()
        .get(id)
        .map(|w| (w.created, w.updated))
}

fn get(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    let workflow = find(ctx.tx(), repo.id, ctx.param("workflow_id")?)?;
    let (full_name, _) = repo_name(ctx.tx(), repo)?;
    Reply::json(&render(ctx, &full_name, workflow))
}

fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    let (full_name, _) = repo_name(ctx.tx(), repo)?;
    let (start, size) = crate::window(ctx);
    let workflows = ctx.tx().table::<Workflows>();
    let active = || workflows.in_repo(repo.id).filter(|w| w.blob.is_some());
    let rows: Vec<_> = active()
        .skip(start)
        .take(size)
        .map(|w| render(ctx, &full_name, w))
        .collect();
    crate::page(ctx, "workflows", active().count(), &rows)
}

pub(crate) fn repo_name<'a>(tx: &'a Tx, repo: &Repo) -> Result<(String, &'a str), ApiError> {
    let owner = tx
        .table::<Accounts>()
        .get(repo.owner_id)
        .ok_or_else(ApiError::not_found)?;
    Ok((format!("{}/{}", owner.login, repo.name), &owner.login))
}

#[derive(Deserialize)]
struct Dispatch {
    #[serde(rename = "ref")]
    git_ref: String,
    #[serde(default)]
    inputs: BTreeMap<String, Value>,
    #[serde(default)]
    return_run_details: bool,
}

pub(crate) fn resolve(git: &Git, tx: &Tx, repo: i64, supplied: &str) -> Option<(String, ObjectId)> {
    let names = [
        format!("refs/heads/{supplied}"),
        format!("refs/tags/{supplied}"),
    ];
    let candidates = if supplied.starts_with("refs/") {
        std::slice::from_ref(&supplied)
    } else {
        &[names[0].as_str(), names[1].as_str()]
    };
    candidates.iter().find_map(|name| {
        let target = git.resolve_ref(tx, repo, name).ok()?;
        let sha = git.peel_to_commit(tx, repo, target).ok()?;
        Some(((*name).to_owned(), sha))
    })
}

fn dispatch_inputs(
    definition: &Definition,
    mut inputs: BTreeMap<String, Value>,
) -> Result<BTreeMap<String, Value>, ApiError> {
    let trigger = &definition.trigger;
    let configured = match trigger {
        Value::String(name) if name == DISPATCH => Some(&Value::Null),
        Value::Array(names) if names.iter().any(|name| name == DISPATCH) => Some(&Value::Null),
        Value::Object(events) => events.get(DISPATCH),
        _ => None,
    }
    .ok_or_else(|| {
        crate::invalid(
            "Workflow",
            "ref",
            "Workflow does not have 'workflow_dispatch' trigger",
        )
    })?;
    let declared = configured.get("inputs").and_then(Value::as_object);
    let unexpected: Vec<_> = inputs
        .keys()
        .filter(|key| !declared.is_some_and(|fields| fields.contains_key(*key)))
        .map(|key| format!("{key:?}"))
        .collect();
    if !unexpected.is_empty() {
        return Err(crate::invalid(
            "Workflow",
            "inputs",
            format!("Unexpected inputs provided: [{}]", unexpected.join(", ")),
        ));
    }
    for (key, spec) in declared.into_iter().flatten() {
        if inputs.contains_key(key) {
            continue;
        }
        if let Some(default) = spec.get("default") {
            inputs.insert(key.clone(), default.clone());
        } else if spec.get("required") == Some(&Value::Bool(true)) {
            return Err(crate::invalid(
                "Workflow",
                "inputs",
                format!("Required input {key:?} not provided"),
            ));
        }
    }
    Ok(inputs)
}

fn dispatch(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let input: Dispatch = ctx.body()?;
    let repo = Arc::clone(ctx.repo()?);
    let actor = ctx.actor().account().ok_or_else(ApiError::unauthorized)?.id;
    let workflow = find(ctx.tx(), repo.id, ctx.param("workflow_id")?)?;
    let (git_ref, sha) =
        resolve(ctx.git(), ctx.tx(), repo.id, &input.git_ref).ok_or_else(|| {
            crate::invalid(
                "Workflow",
                "ref",
                format!("No ref found for: {}", input.git_ref),
            )
        })?;
    let object = ctx
        .git()
        .expression(ctx.tx(), repo.id, &format!("{sha}:{}", workflow.path))
        .map_err(|_| crate::invalid("Workflow", "ref", "Workflow does not exist at the ref"))?;
    let definition = if Some(object.id) == workflow.blob {
        workflow.definition.clone()
    } else {
        parse(&object.data)
    };
    let definition = definition
        .ok_or_else(|| crate::invalid("Workflow", "ref", "Invalid workflow definition"))?;
    let inputs = dispatch_inputs(&definition, input.inputs)?;
    let now = ctx.now();
    let urls = ctx.urls().clone();
    let details = crate::lifecycle::Dispatch {
        workflow_id: workflow.id,
        path: workflow.path.clone(),
        name: definition
            .name
            .clone()
            .unwrap_or_else(|| workflow.path.clone()),
        definition,
        actor,
        git_ref,
        sha,
        inputs,
    };
    let (tx, git) = ctx.tx_mut_and_git()?;
    let run = crate::lifecycle::create(tx, git, &repo, now, details)?;
    if !input.return_run_details {
        return Ok(Reply::empty());
    }
    let id = run.id;
    let (full_name, _) = repo_name(tx, &repo)?;
    Reply::json(&json!({
        "workflow_run_id": id,
        "run_url": format!("{}/repos/{full_name}/actions/runs/{id}", urls.api),
        "html_url": format!("{}/{full_name}/actions/runs/{id}", urls.web),
    }))
}
