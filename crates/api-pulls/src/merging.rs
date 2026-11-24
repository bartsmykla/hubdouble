use std::sync::Arc;

use hubdouble_git::{Commit, Git, ObjectId, Signature};
use hubdouble_model::{
    Account, ApiError, Issue, Repo, SquashMergeCommitMessage, SquashMergeCommitTitle, Timestamp,
};
use hubdouble_store::{Accounts, Tx};

use crate::state;

pub(crate) fn cannot_merge() -> ApiError {
    ApiError::new(405, "Pull request cannot be merged")
}

fn cannot_rebase() -> ApiError {
    ApiError::new(405, "This branch can't be rebased")
}

pub(crate) fn squash(
    tx: &Tx,
    git: &Git,
    (repo, issue): (&Repo, &Issue),
    (sender, email): (i64, Option<&str>),
    (base, head): (ObjectId, ObjectId),
    (title, message): (Option<String>, Option<String>),
) -> Result<ObjectId, ApiError> {
    let commits = pull_commits(tx, git, repo.id, (base, head))?;
    let merged = git
        .test_merge(tx, repo.id, issue.user_id, base, head)?
        .ok_or_else(cannot_merge)?;
    let now = git.now();
    let email = email.filter(|_| sender == issue.user_id);
    let author = merger_signature(tx, (issue.user_id, email), now)?;
    let message = squash_message(repo, issue, &commits, &author.email, (title, message));
    let mut commit = Commit {
        tree: state::commit_node(git, tx, repo.id, merged)?.commit.tree,
        parents: vec![base],
        author,
        committer: Signature::web_flow(now),
        message,
        gpg_signature: None,
    };
    git.sign(&mut commit)?;
    Ok(git.write_commit(tx, repo.id, &commit)?)
}

pub(crate) fn rebase(
    tx: &Tx,
    git: &Git,
    (repo, (merger, email)): (i64, (i64, Option<&str>)),
    (base, head): (ObjectId, ObjectId),
) -> Result<ObjectId, ApiError> {
    let commits = pull_commits(tx, git, repo, (base, head))?;
    let test_merge = git
        .test_merge(tx, repo, merger, base, head)?
        .ok_or_else(cannot_merge)?;
    let committer = merger_signature(tx, (merger, email), git.now())?;
    let mut tip = base;
    let tree = replay(tx, git, (repo, merger), base, &commits, |tree, c| {
        tip = git.write_commit(
            tx,
            repo,
            &Commit {
                tree,
                parents: vec![tip],
                author: c.author.clone(),
                committer: committer.clone(),
                message: c.message.clone(),
                gpg_signature: None,
            },
        )?;
        Ok(())
    })?;
    if tree != Some(state::commit_node(git, tx, repo, test_merge)?.commit.tree) {
        return Err(cannot_rebase());
    }
    Ok(tip)
}

pub(crate) fn rebaseable(
    tx: &Tx,
    git: &Git,
    (repo, author): (i64, i64),
    (base, head): (ObjectId, ObjectId),
    test_merge: ObjectId,
) -> Result<bool, ApiError> {
    let commits = git.compare(tx, repo, base, head)?.commits;
    let tree = replay(tx, git, (repo, author), base, &commits, |_, _| Ok(()))?;
    Ok(tree == Some(state::commit_node(git, tx, repo, test_merge)?.commit.tree))
}

fn replay(
    tx: &Tx,
    git: &Git,
    (repo, author): (i64, i64),
    base: ObjectId,
    commits: &[(ObjectId, Arc<Commit>)],
    mut picked: impl FnMut(ObjectId, &Commit) -> Result<(), ApiError>,
) -> Result<Option<ObjectId>, ApiError> {
    let mut tree = state::commit_node(git, tx, repo, base)?.commit.tree;
    for (id, c) in commits.iter().filter(|(_, c)| c.parents.len() < 2) {
        if let Some(parent) = c.parents.first()
            && state::commit_node(git, tx, repo, *parent)?.commit.tree == c.tree
        {
            continue;
        }
        let stand_in = git.write_commit(
            tx,
            repo,
            &Commit {
                tree,
                parents: c.parents.clone(),
                author: c.author.clone(),
                committer: c.committer.clone(),
                message: String::new(),
                gpg_signature: None,
            },
        )?;
        let Some(merged) = git.test_merge(tx, repo, author, stand_in, *id)? else {
            return Ok(None);
        };
        tree = state::commit_node(git, tx, repo, merged)?.commit.tree;
        picked(tree, c)?;
    }
    Ok(Some(tree))
}

fn pull_commits(
    tx: &Tx,
    git: &Git,
    repo: i64,
    (base, head): (ObjectId, ObjectId),
) -> Result<Vec<(ObjectId, Arc<Commit>)>, ApiError> {
    if git.is_ancestor(tx, repo, head, base)? {
        return Err(cannot_merge());
    }
    Ok(git.compare(tx, repo, base, head)?.commits)
}

fn signature(tx: &Tx, account: i64, time: Timestamp) -> Result<Signature, ApiError> {
    let account = tx
        .table::<Accounts>()
        .get(account)
        .ok_or_else(ApiError::not_found)?;
    Ok(Signature {
        name: account
            .name
            .clone()
            .unwrap_or_else(|| account.login.clone()),
        email: account.email.clone().unwrap_or_else(|| noreply(account)),
        time,
        offset_minutes: 0,
    })
}

fn noreply(account: &Account) -> String {
    format!("{}+{}@users.noreply.github.com", account.id, account.login)
}

pub(crate) fn owns_email(tx: &Tx, account: i64, email: &str) -> bool {
    tx.table::<Accounts>().get(account).is_some_and(|a| {
        a.email
            .iter()
            .cloned()
            .chain([noreply(a)])
            .any(|own| own.eq_ignore_ascii_case(email))
    })
}

fn merger_signature(
    tx: &Tx,
    (merger, email): (i64, Option<&str>),
    time: Timestamp,
) -> Result<Signature, ApiError> {
    let mut signature = signature(tx, merger, time)?;
    if let Some(email) = email {
        email.clone_into(&mut signature.email);
    }
    Ok(signature)
}

pub(crate) fn merge_commit(
    tx: &Tx,
    git: &Git,
    (repo, merger): (i64, (i64, Option<&str>)),
    (base, head): (ObjectId, ObjectId),
    message: String,
) -> Result<ObjectId, ApiError> {
    pull_commits(tx, git, repo, (base, head))?;
    let tested = git
        .test_merge(tx, repo, merger.0, base, head)?
        .ok_or_else(cannot_merge)?;
    let now = git.now();
    let mut commit = Commit {
        tree: state::commit_node(git, tx, repo, tested)?.commit.tree,
        parents: vec![base, head],
        author: merger_signature(tx, merger, now)?,
        committer: Signature::web_flow(now),
        message,
        gpg_signature: None,
    };
    git.sign(&mut commit)?;
    Ok(git.write_commit(tx, repo, &commit)?)
}

fn squash_message(
    repo: &Repo,
    issue: &Issue,
    commits: &[(ObjectId, Arc<Commit>)],
    author_email: &str,
    (title, message): (Option<String>, Option<String>),
) -> String {
    let squashed: Vec<&Commit> = commits
        .iter()
        .map(|(_, c)| c.as_ref())
        .filter(|c| c.parents.len() < 2)
        .collect();
    let single = match squashed.as_slice() {
        [only] => Some(*only),
        _ => None,
    };
    let title = title.unwrap_or_else(|| {
        let text = match (repo.squash_merge_commit_title, single) {
            (SquashMergeCommitTitle::CommitOrPrTitle, Some(c)) => {
                c.message.lines().next().unwrap_or_default()
            }
            _ => &issue.title,
        };
        format!("{text} (#{})", issue.number)
    });
    let body = message.unwrap_or_else(|| {
        let body = match (repo.squash_merge_commit_message, single) {
            (SquashMergeCommitMessage::CommitMessages, Some(c)) => c
                .message
                .split_once('\n')
                .map_or("", |(_, rest)| rest)
                .trim()
                .to_owned(),
            (SquashMergeCommitMessage::CommitMessages, None) => squashed
                .iter()
                .map(|c| format!("* {}", c.message.trim_end()))
                .collect::<Vec<_>>()
                .join("\n\n"),
            (SquashMergeCommitMessage::PrBody, _) => issue.body.clone().unwrap_or_default(),
            (SquashMergeCommitMessage::Blank, _) => String::new(),
        };
        with_co_authors(body, commits, author_email)
    });
    if body.is_empty() {
        title
    } else {
        format!("{title}\n\n{body}")
    }
}

fn with_co_authors(body: String, commits: &[(ObjectId, Arc<Commit>)], author: &str) -> String {
    let mut seen = vec![author.to_ascii_lowercase()];
    let mut trailers = Vec::new();
    for (_, c) in commits {
        let email = c.author.email.to_ascii_lowercase();
        if seen.contains(&email) {
            continue;
        }
        seen.push(email);
        let line = format!("Co-authored-by: {} <{}>", c.author.name, c.author.email);
        if !body.contains(&line) {
            trailers.push(line);
        }
    }
    if trailers.is_empty() {
        return body;
    }
    let trailers = trailers.join("\n");
    if body.is_empty() {
        trailers
    } else if ends_with_trailers(&body) {
        format!("{body}\n{trailers}")
    } else {
        format!("{body}\n\n{trailers}")
    }
}

fn ends_with_trailers(body: &str) -> bool {
    body.rsplit("\n\n").next().is_some_and(|paragraph| {
        paragraph.lines().all(|line| {
            line.split_once(": ").is_some_and(|(key, _)| {
                !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            })
        })
    })
}
