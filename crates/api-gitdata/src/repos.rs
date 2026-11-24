mod compare;

use http::StatusCode;
use http::header::{ETAG, LINK};
use hubdouble_git::{
    Commit, EntryMode, FileDiff, FileStatus, Git, GitError, ObjectData, ObjectId, ObjectKind,
    Signature,
};
use hubdouble_model::{ApiError, Repo, Timestamp};
use hubdouble_render::{SimpleUser, Urls};
use hubdouble_rest::{Ctx, Page, Reply};
use hubdouble_store::{Accounts, Tx};
use serde::{Deserialize, Serialize};

use crate::branch_rules::{Guard, Via, protected, refusal};
use crate::common::{
    GitPerson, Links, ObjectLink, ParentLink, Verification, accepted_media, account_by_email,
    commit_at, object_node_id, sender, unprocessable, user_of,
};
use crate::refs::{denied_workflow, workflow_refusal};

pub(crate) use compare::compare;

const COMMIT_FILES: usize = 300;
const MAX_COMMIT_FILES: usize = 3_000;
const SHA_MEDIA: &str = "application/vnd.github.sha; charset=utf-8";
const V3_SHA_MEDIA: &str = "application/vnd.github.v3.sha; charset=utf-8";
const MERGE_DOCS: &str = "https://docs.github.com/rest/branches/branches#merge-a-branch";

#[derive(Serialize)]
struct CommitDetail {
    url: String,
    author: GitPerson,
    committer: GitPerson,
    message: String,
    comment_count: u32,
    tree: ObjectLink,
    verification: Verification,
}

#[derive(Serialize)]
struct Stats {
    additions: u64,
    deletions: u64,
    total: u64,
}

#[derive(Serialize)]
struct DiffEntry {
    sha: Option<ObjectId>,
    filename: String,
    status: &'static str,
    additions: u64,
    deletions: u64,
    changes: u64,
    blob_url: String,
    raw_url: String,
    contents_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    patch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_filename: Option<String>,
}

impl DiffEntry {
    fn new(links: &Links, head: ObjectId, f: &FileDiff) -> DiffEntry {
        DiffEntry {
            sha: Some(f.blob),
            filename: f.path.clone(),
            status: f.status.as_str(),
            additions: f.additions,
            deletions: f.deletions,
            changes: f.additions + f.deletions,
            blob_url: format!("{}/blob/{head}/{}", links.web, f.path),
            raw_url: format!("{}/raw/{head}/{}", links.web, f.path),
            contents_url: format!("{}/contents/{}?ref={head}", links.api, f.path),
            patch: f.patch.clone(),
            previous_filename: f.previous_path.clone(),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct RepoCommit {
    url: String,
    sha: ObjectId,
    node_id: String,
    html_url: String,
    comments_url: String,
    commit: CommitDetail,
    author: Option<SimpleUser>,
    committer: Option<SimpleUser>,
    parents: Vec<ParentLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stats: Option<Stats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<Vec<DiffEntry>>,
}

struct View<'a> {
    git: &'a Git,
    tx: &'a Tx,
    urls: &'a Urls,
    repo: &'a Repo,
    links: Links,
}

impl<'a> View<'a> {
    fn of(ctx: &'a Ctx<'_>) -> Result<View<'a>, ApiError> {
        Ok(View {
            git: ctx.git(),
            tx: ctx.tx(),
            urls: ctx.urls(),
            repo: ctx.repo()?,
            links: Links::of(ctx)?,
        })
    }

    fn commit(&self, id: ObjectId, c: &Commit) -> RepoCommit {
        let links = &self.links;
        RepoCommit {
            url: format!("{}/commits/{id}", links.api),
            sha: id,
            node_id: object_node_id(ObjectKind::Commit, self.repo.id, id),
            html_url: format!("{}/commit/{id}", links.web),
            comments_url: format!("{}/commits/{id}/comments", links.api),
            commit: CommitDetail {
                url: links.object(ObjectKind::Commit, id),
                author: (&c.author).into(),
                committer: (&c.committer).into(),
                message: c.message.clone(),
                comment_count: 0,
                tree: ObjectLink {
                    sha: c.tree,
                    url: links.object(ObjectKind::Tree, c.tree),
                },
                verification: Verification::of_commit(self.git, self.repo.id, id, c),
            },
            author: user_of(self.tx, self.urls, &c.author),
            committer: user_of(self.tx, self.urls, &c.committer),
            parents: c
                .parents
                .iter()
                .map(|p| ParentLink {
                    sha: *p,
                    url: format!("{}/commits/{p}", links.api),
                    html_url: format!("{}/commit/{p}", links.web),
                })
                .collect(),
            stats: None,
            files: None,
        }
    }

    fn with_files(
        &self,
        id: ObjectId,
        c: &Commit,
        all_files: &[FileDiff],
        shown: &[FileDiff],
    ) -> RepoCommit {
        let mut out = self.commit(id, c);
        let (additions, deletions) = all_files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
        out.stats = Some(Stats {
            additions,
            deletions,
            total: additions + deletions,
        });
        out.files = Some(
            shown
                .iter()
                .map(|f| DiffEntry::new(&self.links, id, f))
                .collect(),
        );
        out
    }

    fn files(&self, id: ObjectId, c: &Commit) -> Result<Vec<FileDiff>, ApiError> {
        match c.parents.first() {
            Some(parent) => Ok(self.git.compare(self.tx, self.repo.id, *parent, id)?.files),
            None => root_files(self.git, self.tx, self.repo.id, c.tree, ""),
        }
    }
}

fn root_files(
    git: &Git,
    tx: &Tx,
    repo: i64,
    tree: ObjectId,
    prefix: &str,
) -> Result<Vec<FileDiff>, ApiError> {
    let ObjectData::Tree(entries) = git.object(tx, repo, tree)?.data else {
        return Err(ApiError::internal("a commit's tree is not a tree"));
    };
    let mut out = Vec::new();
    for e in entries.iter() {
        let path = if prefix.is_empty() {
            e.name.clone()
        } else {
            format!("{prefix}/{}", e.name)
        };
        match e.mode {
            EntryMode::Tree => out.extend(root_files(git, tx, repo, e.id, &path)?),
            EntryMode::Submodule => {}
            _ => {
                let ObjectData::Blob(data) = git.object(tx, repo, e.id)?.data else {
                    continue;
                };
                let (additions, hunk) = added_patch(&data);
                out.push(FileDiff {
                    path,
                    previous_path: None,
                    status: FileStatus::Added,
                    blob: e.id,
                    additions,
                    deletions: 0,
                    patch: hunk,
                });
            }
        }
    }
    Ok(out)
}

fn added_patch(data: &[u8]) -> (u64, Option<String>) {
    if data.contains(&0) {
        return (0, None);
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return (0, None);
    };
    if text.is_empty() {
        return (0, None);
    }
    let lines: Vec<&str> = text.lines().collect();
    let count = u64::try_from(lines.len()).unwrap_or(u64::MAX);
    let mut patch = if count == 1 {
        "@@ -0,0 +1 @@".to_owned()
    } else {
        format!("@@ -0,0 +1,{count} @@")
    };
    for l in &lines {
        patch.push_str("\n+");
        patch.push_str(l);
    }
    if !text.ends_with('\n') {
        patch.push_str("\n\\ No newline at end of file");
    }
    (count, Some(patch))
}

fn explicit_page(ctx: &Ctx<'_>) -> Result<Option<Page>, ApiError> {
    let q = ctx.query();
    (q.get("page").is_some() || q.get("per_page").is_some())
        .then(|| ctx.page_params())
        .transpose()
}

fn slice<T>(items: &[T], page: Page) -> &[T] {
    let start = (page.page - 1)
        .saturating_mul(page.per_page)
        .min(items.len());
    let end = start.saturating_add(page.per_page).min(items.len());
    &items[start..end]
}

fn with_link(ctx: &Ctx<'_>, reply: Reply, page: Page, total: usize) -> Result<Reply, ApiError> {
    let last = total.div_ceil(page.per_page).max(1);
    match ctx.link_header(page.page, last) {
        Some(link) => reply.header(LINK, &link),
        None => Ok(reply),
    }
}

fn no_commit(reference: &str) -> ApiError {
    unprocessable(
        "Commit",
        "sha",
        &format!("No commit found for SHA: {reference}"),
    )
}

fn commit_page(ctx: &Ctx<'_>) -> Result<Page, ApiError> {
    ctx.page_params_sized(COMMIT_FILES, COMMIT_FILES)
}

pub(crate) fn get_commit(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let reference = ctx.param("ref")?.to_owned();
    let view = View::of(ctx)?;
    let (id, commit) = commit_at(view.git, view.tx, view.repo.id, &reference)
        .ok_or_else(|| no_commit(&reference))?;
    if let Some(media) = accepted_media(ctx, "sha") {
        let content_type = if media == "application/vnd.github.v3.sha" {
            V3_SHA_MEDIA
        } else {
            SHA_MEDIA
        };
        return Reply::raw(content_type, id.to_string()).header(ETAG, &format!("\"{id}\""));
    }
    let files = view.files(id, &commit)?;
    let pageable = &files[..files.len().min(MAX_COMMIT_FILES)];
    let page = commit_page(ctx)?;
    let body = view.with_files(id, &commit, &files, slice(pageable, page));
    with_link(ctx, Reply::json(&body)?, page, pageable.len())
}

fn touches(git: &Git, tx: &Tx, repo: i64, id: ObjectId, c: &Commit, path: &str) -> bool {
    let at = |commit: ObjectId| {
        git.expression(tx, repo, &format!("{commit}:{path}"))
            .ok()
            .map(|o| o.id)
    };
    let mine = at(id);
    if c.parents.is_empty() {
        return mine.is_some();
    }
    c.parents.iter().all(|p| at(*p) != mine)
}

pub(crate) fn list_commits(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let view = View::of(ctx)?;
    let reference = ctx
        .query()
        .get("sha")
        .filter(|s| !s.is_empty())
        .unwrap_or(&view.repo.default_branch)
        .to_owned();
    if view.git.refs(view.tx, view.repo.id, "").is_empty() {
        return Err(ApiError::conflict("Git Repository is empty."));
    }
    let (head, _) = commit_at(view.git, view.tx, view.repo.id, &reference)
        .ok_or_else(|| ApiError::new(404, format!("No commit found for SHA: {reference}")))?;
    let mut log = view
        .git
        .log(view.tx, view.repo.id, head, false, usize::MAX)?;
    if let Some(path) = ctx
        .query()
        .get("path")
        .map(|p| p.trim_matches('/'))
        .filter(|p| !p.is_empty())
    {
        log.retain(|(id, c)| touches(view.git, view.tx, view.repo.id, *id, c, path));
    }
    let bound = |name| {
        ctx.query()
            .get(name)
            .filter(|t| !t.is_empty())
            .map(|t| {
                Timestamp::parse_commit_date(t, ctx.now()).map_err(|_| {
                    ApiError::internal("Server Error")
                        .with_docs("https://docs.github.com/rest/commits/commits#list-commits")
                })
            })
            .transpose()
    };
    let (since, until) = (bound("since")?, bound("until")?);
    log.retain(|(_, c)| {
        since.is_none_or(|s| c.committer.time >= s) && until.is_none_or(|u| c.committer.time <= u)
    });
    for (name, signature) in [
        (
            "author",
            (|c: &Commit| &c.author) as fn(&Commit) -> &Signature,
        ),
        ("committer", |c: &Commit| &c.committer),
    ] {
        if let Some(who) = ctx.query().get(name) {
            let account = (!who.contains('@'))
                .then(|| view.tx.table::<Accounts>().by_login(who))
                .flatten()
                .map(|a| a.id);
            log.retain(|(_, c)| {
                let email = &signature(c).email;
                match account {
                    Some(id) => account_by_email(view.tx, email).is_some_and(|a| a.id == id),
                    None => who.contains('@') && email.eq_ignore_ascii_case(who),
                }
            });
        }
    }
    let page = ctx.page_params()?;
    let rendered: Vec<_> = slice(&log, page)
        .iter()
        .map(|(id, c)| view.commit(*id, c))
        .collect();
    let reply = Reply::json(&rendered)?;
    let Page { page, per_page } = page;
    let last = log.len().div_ceil(per_page).max(1);
    let mut rels = Vec::with_capacity(4);
    if rendered.len() == per_page {
        rels.extend([(page + 1, "next"), (last, "last")]);
    }
    if page > 1 {
        rels.extend([(1, "first"), (page - 1, "prev")]);
    }
    if rels.is_empty() {
        return Ok(reply);
    }
    reply.header(LINK, &ctx.links(&rels))
}

#[derive(Deserialize)]
struct MergeRequest {
    base: String,
    head: String,
    #[serde(default)]
    commit_message: Option<String>,
}

pub(crate) fn merge(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let body: MergeRequest = ctx.body()?;
    let repo = ctx.repo()?.id;
    let account = sender(ctx.actor())?.id;
    let (git, tx) = (ctx.git(), ctx.tx());
    let branch = format!("refs/heads/{}", body.base);
    let Ok(tip) = git.resolve_ref(tx, repo, &branch) else {
        return Err(ApiError::new(404, "Base does not exist"));
    };
    let Some((head, _)) = commit_at(git, tx, repo, &body.head) else {
        return Err(ApiError::new(404, "Head does not exist"));
    };
    let message = body
        .commit_message
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| format!("Merge {} into {}", body.head, body.base));
    let (tx, git) = ctx.tx_mut_and_git()?;
    let made = match git.merge(tx, repo, account, &body.base, &body.head, &message) {
        Ok(m) => m.commit,
        Err(GitError::NothingToMerge) => return Ok(Reply::empty()),
        Err(GitError::Conflict { .. }) => return Err(ApiError::conflict("Merge Conflict")),
        Err(e) => return Err(e.into()),
    };
    let (tx, git, actor, repo_row) = (ctx.tx(), ctx.git(), ctx.actor(), ctx.repo()?);
    if let Some(path) = denied_workflow(tx, git, actor, repo_row, Some(tip), made)? {
        return Err(ApiError::new(403, workflow_refusal(actor, &path)).with_docs(MERGE_DOCS));
    }
    let guard = Guard {
        tx,
        git,
        actor,
        repo: repo_row,
    };
    let violations = guard
        .check(&branch, (Some(tip), Some(made)), Via::Merge { head })
        .map_err(|e| e.with_docs(MERGE_DOCS))?;
    if !violations.is_empty() {
        return Err(refusal(409, &violations, MERGE_DOCS));
    }
    let view = View::of(ctx)?;
    let ObjectData::Commit(commit) = view.git.object(view.tx, repo, made)?.data else {
        return Err(ApiError::internal("a merge made no commit"));
    };
    let files = view.files(made, &commit)?;
    Reply::json(&view.with_files(made, &commit, &files, &files))
        .map(|r| r.status(StatusCode::CREATED))
}

#[derive(Serialize)]
struct BranchLinks {
    html: String,
    #[serde(rename = "self")]
    own: String,
}

#[derive(Serialize)]
struct StatusChecks {
    enforcement_level: &'static str,
    contexts: [&'static str; 0],
    checks: [&'static str; 0],
}

#[derive(Serialize)]
struct Protection {
    enabled: bool,
    required_status_checks: StatusChecks,
}

#[derive(Serialize)]
struct Branch {
    name: String,
    commit: RepoCommit,
    #[serde(rename = "_links")]
    links: BranchLinks,
    protected: bool,
    protection: Protection,
    protection_url: String,
}

pub(crate) fn get_branch(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let name = ctx.param("branch")?.to_owned();
    let view = View::of(ctx)?;
    let missing = || ApiError::new(404, "Branch not found");
    let target = view
        .git
        .resolve_ref(view.tx, view.repo.id, &format!("refs/heads/{name}"))
        .map_err(|_| missing())?;
    let (id, commit) =
        commit_at(view.git, view.tx, view.repo.id, &target.to_string()).ok_or_else(missing)?;
    let own = format!("{}/branches/{name}", view.links.api);
    Reply::json(&Branch {
        commit: view.commit(id, &commit),
        links: BranchLinks {
            html: format!("{}/tree/{name}", view.links.web),
            own: own.clone(),
        },
        protected: protected(view.tx, view.repo, &name),
        protection: Protection {
            enabled: false,
            required_status_checks: StatusChecks {
                enforcement_level: "off",
                contexts: [],
                checks: [],
            },
        },
        protection_url: format!("{own}/protection"),
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_added_file_patch_counts_every_line() {
        assert_eq!(
            added_patch(b"a\nb\n"),
            (2, Some("@@ -0,0 +1,2 @@\n+a\n+b".to_owned()))
        );
        assert_eq!(
            added_patch(b"a"),
            (
                1,
                Some("@@ -0,0 +1 @@\n+a\n\\ No newline at end of file".to_owned())
            )
        );
        assert_eq!(added_patch(b"\0bin"), (0, None));
    }
}
