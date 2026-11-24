use hubdouble_git::{Git, ObjectData, Signature};
use hubdouble_model::{AccountKind, ApiError, Event, PullRef, WorkflowJob, WorkflowRun};
use hubdouble_render::{
    CommitPerson, Job, PullRequestMinimal, PullRequestMinimalRef, PullRequestMinimalRepo, RepoUrls,
    Repository, SimpleCommit, SimpleUser, Urls,
};
use hubdouble_store::{Accounts, Issues, Repos, Tx, TxMut};
use serde_json::{Map, Value, json};

use crate::{runs::RunContexts, workflows};

pub(crate) fn run(
    tx: &Tx,
    git: &Git,
    urls: &Urls,
    run: &WorkflowRun,
    exclude_pulls: bool,
) -> Result<hubdouble_render::WorkflowRun, ApiError> {
    let repository = repository(tx, urls, run.repo_id)?;
    let head_repository = if run.head_repo_id == run.repo_id {
        repository.clone()
    } else {
        self::repository(tx, urls, run.head_repo_id)?
    };
    let mut view = hubdouble_render::WorkflowRun::new(urls, run, repository, head_repository);
    let accounts = tx.table::<Accounts>();
    view.actor = accounts
        .get(run.actor_id)
        .map(|account| SimpleUser::new(urls, account));
    view.triggering_actor = accounts
        .get(run.triggering_actor_id)
        .map(|account| SimpleUser::new(urls, account));
    view.run_started_at = Some(run.run_started_at.unwrap_or(run.created_at));
    view.head_commit = commit(tx, git, run);
    if !exclude_pulls {
        view.pull_requests = pulls(tx, urls, run);
    }
    if let Some(reusable) = tx
        .table::<RunContexts>()
        .get(run.id)
        .and_then(|ctx| ctx.reusable.as_ref())
    {
        let (path, git_ref) = reusable
            .workflow_ref
            .rsplit_once('@')
            .unwrap_or((&reusable.workflow_ref, &reusable.sha));
        view.referenced_workflows
            .push(json!({"path": path, "sha": reusable.sha, "ref": git_ref}));
    }
    Ok(view)
}

fn repository(tx: &Tx, urls: &Urls, id: i64) -> Result<Repository, ApiError> {
    let repo = tx
        .table::<Repos>()
        .get(id)
        .ok_or_else(ApiError::not_found)?;
    let owner = tx
        .table::<Accounts>()
        .get(repo.owner_id)
        .ok_or_else(ApiError::not_found)?;
    Ok(Repository::new(urls, repo, owner))
}

fn commit(tx: &Tx, git: &Git, run: &WorkflowRun) -> Option<SimpleCommit> {
    let id = run.head_sha.parse().ok()?;
    let ObjectData::Commit(commit) = git.object(tx, run.head_repo_id, id).ok()?.data else {
        return None;
    };
    let person = |value: &Signature| CommitPerson {
        name: value.name.clone(),
        email: value.email.clone(),
    };
    Some(SimpleCommit {
        id: run.head_sha.clone(),
        tree_id: commit.tree.to_string(),
        message: commit.message.clone(),
        timestamp: commit.author.time,
        author: Some(person(&commit.author)),
        committer: Some(person(&commit.committer)),
    })
}

fn pulls(tx: &Tx, urls: &Urls, run: &WorkflowRun) -> Vec<PullRequestMinimal> {
    run.pull_request_numbers
        .iter()
        .filter_map(|&number| {
            let pull = tx
                .table::<Issues>()
                .by_number(run.repo_id, number)?
                .pull
                .as_ref()?;
            let base = pull_ref(tx, urls, &pull.base)?;
            Some(PullRequestMinimal {
                id: pull.id,
                number,
                url: format!("{}/pulls/{number}", base.repo.url),
                head: pull_ref(tx, urls, &pull.head)?,
                base,
            })
        })
        .collect()
}

fn pull_ref(tx: &Tx, urls: &Urls, reference: &PullRef) -> Option<PullRequestMinimalRef> {
    let repo = tx.table::<Repos>().get(reference.repo_id?)?;
    let (full_name, _) = workflows::repo_name(tx, repo).ok()?;
    Some(PullRequestMinimalRef {
        ref_name: reference.ref_name.clone(),
        sha: reference.sha.clone(),
        repo: PullRequestMinimalRepo {
            id: repo.id,
            url: format!("{}/repos/{full_name}", urls.api),
            name: repo.name.clone(),
        },
    })
}

pub(crate) fn run_event(
    tx: &mut TxMut<'_>,
    git: &Git,
    run: &WorkflowRun,
    actor: i64,
    action: &'static str,
) -> Result<(), ApiError> {
    let urls = Urls::new(tx.config(), "");
    let view = self::run(tx, git, &urls, run, false)?;
    let workflow = workflows::view(
        &urls,
        &view.repository.full_name,
        run.workflow_id,
        &run.path,
        &run.name,
        workflows::times(tx, run.workflow_id).unwrap_or((run.created_at, run.updated_at)),
    );
    let mut payload = object(&view)?;
    payload.remove("head_repository_id");
    payload.insert("repository".to_owned(), lite(&view.repository)?);
    payload.insert("head_repository".to_owned(), lite(&view.head_repository)?);
    emit(
        tx,
        run.repo_id,
        actor,
        "workflow_run",
        Some(action),
        Map::from_iter([
            ("workflow_run".to_owned(), Value::Object(payload)),
            ("workflow".to_owned(), workflow),
        ]),
    );
    Ok(())
}

pub(crate) fn job_event(
    tx: &mut TxMut<'_>,
    job: &WorkflowJob,
    actor: i64,
    action: &'static str,
) -> Result<(), ApiError> {
    let repo = tx
        .table::<Repos>()
        .get(job.repo_id)
        .ok_or_else(ApiError::not_found)?;
    let (full_name, _) = workflows::repo_name(tx, repo)?;
    let view = Job::new(&Urls::new(tx.config(), ""), job, &full_name);
    let payload = Map::from_iter([("workflow_job".to_owned(), Value::Object(object(&view)?))]);
    emit(
        tx,
        job.repo_id,
        actor,
        "workflow_job",
        Some(action),
        payload,
    );
    Ok(())
}

fn object(value: &impl serde::Serialize) -> Result<Map<String, Value>, ApiError> {
    match serde_json::to_value(value) {
        Ok(Value::Object(value)) => Ok(value),
        _ => Err(ApiError::internal(
            "Could not render Actions webhook payload",
        )),
    }
}

fn lite(repo: &Repository) -> Result<Value, ApiError> {
    let urls: &RepoUrls = &repo.urls;
    let mut fields = object(urls)?;
    for key in ["clone_url", "git_url", "mirror_url", "ssh_url", "svn_url"] {
        fields.remove(key);
    }
    let Value::Object(repo) = json!({
        "id": repo.id, "node_id": repo.node_id, "name": repo.name, "full_name": repo.full_name,
        "owner": repo.owner, "private": repo.private, "description": repo.description,
        "fork": repo.fork, "html_url": repo.html_url,
    }) else {
        return Err(ApiError::internal("Could not render Actions repository"));
    };
    fields.extend(repo);
    Ok(Value::Object(fields))
}

pub(crate) fn dispatch_event(tx: &mut TxMut<'_>, run: &WorkflowRun, git_ref: &str) {
    let inputs = (!run.inputs.is_empty()).then(|| {
        run.inputs
            .iter()
            .map(|(key, value)| {
                let text = match value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                (key.clone(), Value::String(text))
            })
            .collect::<Map<_, _>>()
    });
    let payload = Map::from_iter([
        (
            "inputs".to_owned(),
            inputs.map_or(Value::Null, Value::Object),
        ),
        ("ref".to_owned(), Value::from(git_ref)),
        ("workflow".to_owned(), Value::from(run.path.as_str())),
    ]);
    emit(
        tx,
        run.repo_id,
        run.actor_id,
        "workflow_dispatch",
        None,
        payload,
    );
}

fn emit(
    tx: &mut TxMut<'_>,
    repo_id: i64,
    sender_id: i64,
    name: &'static str,
    action: Option<&'static str>,
    payload: Map<String, Value>,
) {
    let org_id = tx
        .table::<Repos>()
        .get(repo_id)
        .and_then(|repo| tx.table::<Accounts>().get(repo.owner_id))
        .filter(|owner| owner.kind == AccountKind::Organization)
        .map(|owner| owner.id);
    hubdouble_hooks::emit(
        tx,
        Event {
            name,
            action,
            repo_id: Some(repo_id),
            org_id,
            sender_id,
            app_id: None,
            installation_id: None,
            payload,
        },
    );
}
