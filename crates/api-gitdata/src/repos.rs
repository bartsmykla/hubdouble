use std::sync::Arc;

use http::StatusCode;
use http::header::{ETAG, LINK};
use hubdouble_git::{
    Commit, EntryMode, FileDiff, FileStatus, Git, GitError, ObjectData, ObjectId, ObjectKind,
    TreeEntry,
};
use hubdouble_model::{ApiError, Enforcement, Repo, RulesetSource, RulesetTarget};
use hubdouble_render::{SimpleUser, Urls};
use hubdouble_rest::{Ctx, Page, Reply};
use hubdouble_store::{Rulesets, Tx};
use serde::{Deserialize, Serialize};

use crate::common::{
    GitPerson, Links, ObjectLink, ParentLink, Verification, accepts, commit_at, network_repo,
    object_node_id, sender, unprocessable, user_of, wrapped_base64,
};
use crate::data::RAW;

const COMMIT_FILES: usize = 300;
const COMPARE_COMMITS: usize = 250;
const SHA_MEDIA: &str = "application/vnd.github.sha; charset=utf-8";

#[derive(Serialize)]
struct ContentLinks {
    #[serde(rename = "self")]
    own: String,
    git: Option<String>,
    html: Option<String>,
}

#[derive(Serialize)]
struct Content {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    encoding: Option<&'static str>,
    size: usize,
    name: String,
    path: String,
    #[serde(rename = "content", skip_serializing_if = "Option::is_none")]
    data: Option<String>,
    sha: ObjectId,
    url: String,
    git_url: Option<String>,
    html_url: Option<String>,
    download_url: Option<String>,
    #[serde(rename = "_links")]
    links: ContentLinks,
}

struct At<'a> {
    links: &'a Links,
    reference: &'a str,
}

impl At<'_> {
    fn content(
        &self,
        kind: &'static str,
        path: &str,
        sha: ObjectId,
        object: ObjectKind,
        size: usize,
        content: Option<String>,
    ) -> Content {
        let name = path.rsplit('/').next().unwrap_or(path).to_owned();
        let own = format!("{}/contents/{path}?ref={}", self.links.api, self.reference);
        let view = if kind == "dir" { "tree" } else { "blob" };
        let html = format!("{}/{view}/{}/{path}", self.links.web, self.reference);
        let git = (kind != "submodule").then(|| self.links.object(object, sha));
        Content {
            kind,
            encoding: content.is_some().then_some("base64"),
            size,
            name,
            path: path.to_owned(),
            data: content,
            sha,
            url: own.clone(),
            git_url: git.clone(),
            html_url: Some(html.clone()),
            download_url: (kind == "file")
                .then(|| format!("{}/raw/{}/{path}", self.links.web, self.reference)),
            links: ContentLinks {
                own,
                git,
                html: Some(html),
            },
        }
    }
}

fn listing_kind(mode: EntryMode) -> &'static str {
    match mode {
        EntryMode::Tree => "dir",
        EntryMode::Submodule => "submodule",
        EntryMode::Symlink => "symlink",
        EntryMode::File | EntryMode::Executable => "file",
    }
}

pub(crate) fn get_content(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    let path = ctx.param("path")?.trim_matches('/').to_owned();
    let reference = ctx
        .query()
        .get("ref")
        .filter(|r| !r.is_empty())
        .unwrap_or(&repo.default_branch)
        .to_owned();
    let (git, tx) = (ctx.git(), ctx.tx());
    let (commit, _) = commit_at(git, tx, repo.id, &reference)
        .ok_or_else(|| ApiError::new(404, format!("No commit found for the ref {reference}")))?;
    let object = git
        .expression(tx, repo.id, &format!("{commit}:{path}"))
        .map_err(|_| ApiError::not_found())?;
    let links = Links::of(ctx)?;
    let at = At {
        links: &links,
        reference: &reference,
    };
    match object.data {
        ObjectData::Blob(data) => {
            if accepts(ctx, "raw") {
                return Ok(Reply::raw(RAW, data.to_vec()));
            }
            Reply::json(&at.content(
                "file",
                &path,
                object.id,
                ObjectKind::Blob,
                data.len(),
                Some(wrapped_base64(&data)),
            ))
        }
        ObjectData::Tree(entries) => {
            let listing = entries
                .iter()
                .map(|e| entry_content(git, tx, repo.id, &at, &path, e))
                .collect::<Result<Vec<_>, _>>()?;
            Reply::json(&listing)
        }
        ObjectData::Commit(_) | ObjectData::Tag(_) => Err(ApiError::not_found()),
    }
}

fn entry_content(
    git: &Git,
    tx: &Tx,
    repo: i64,
    at: &At<'_>,
    dir: &str,
    e: &TreeEntry,
) -> Result<Content, ApiError> {
    let path = if dir.is_empty() {
        e.name.clone()
    } else {
        format!("{dir}/{}", e.name)
    };
    let kind = listing_kind(e.mode);
    let (object, size) = match e.mode {
        EntryMode::Tree => (ObjectKind::Tree, 0),
        EntryMode::Submodule => (ObjectKind::Commit, 0),
        _ => match git.object(tx, repo, e.id)?.data {
            ObjectData::Blob(d) => (ObjectKind::Blob, d.len()),
            _ => (ObjectKind::Blob, 0),
        },
    };
    Ok(at.content(kind, &path, e.id, object, size, None))
}

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

    fn with_files(&self, id: ObjectId, c: &Commit, files: &[FileDiff]) -> RepoCommit {
        let mut out = self.commit(id, c);
        let (additions, deletions) = files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
        out.stats = Some(Stats {
            additions,
            deletions,
            total: additions + deletions,
        });
        out.files = Some(
            files
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

fn explicit_page(ctx: &Ctx<'_>) -> Option<Page> {
    let q = ctx.query();
    (q.get("page").is_some() || q.get("per_page").is_some()).then(|| ctx.page_params())
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

pub(crate) fn get_commit(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let reference = ctx.param("ref")?.to_owned();
    let view = View::of(ctx)?;
    let (id, commit) = commit_at(view.git, view.tx, view.repo.id, &reference)
        .ok_or_else(|| no_commit(&reference))?;
    if accepts(ctx, "sha") {
        return Reply::raw(SHA_MEDIA, id.to_string()).header(ETAG, &format!("\"{id}\""));
    }
    let files = view.files(id, &commit)?;
    if let Some(page) = explicit_page(ctx) {
        let reply = Reply::json(&view.with_files(id, &commit, slice(&files, page)))?;
        return with_link(ctx, reply, page, files.len());
    }
    let shown = &files[..files.len().min(COMMIT_FILES)];
    Reply::json(&view.with_files(id, &commit, shown))
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
    ctx.page(&log, |(id, c)| view.commit(*id, c))
}

#[derive(Serialize)]
struct Comparison {
    url: String,
    html_url: String,
    permalink_url: String,
    diff_url: String,
    patch_url: String,
    base_commit: RepoCommit,
    merge_base_commit: RepoCommit,
    status: &'static str,
    ahead_by: u64,
    behind_by: u64,
    total_commits: u64,
    commits: Vec<RepoCommit>,
    files: Vec<DiffEntry>,
}

fn side(view: &View<'_>, text: &str) -> Option<ObjectId> {
    let (repo, rev) = match text.split_once(':') {
        Some((owner, rest)) => {
            let rest = rest.split_once(':').map_or(rest, |(_, r)| r);
            (network_repo(view.tx, view.repo, owner)?.id, rest)
        }
        None => (view.repo.id, text),
    };
    commit_at(view.git, view.tx, repo, rev).map(|(id, _)| id)
}

pub(crate) fn compare(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let basehead = ctx.param("basehead")?.to_owned();
    let (base, head) = basehead
        .split_once("...")
        .or_else(|| basehead.split_once(".."))
        .ok_or_else(ApiError::not_found)?;
    let view = View::of(ctx)?;
    let base_id = side(&view, base).ok_or_else(ApiError::not_found)?;
    let head_id = side(&view, head).ok_or_else(ApiError::not_found)?;
    let cmp = view.git.compare(view.tx, view.repo.id, base_id, head_id)?;
    let load = |id: ObjectId| -> Result<Arc<Commit>, ApiError> {
        match view.git.object(view.tx, view.repo.id, id)?.data {
            ObjectData::Commit(c) => Ok(c),
            _ => Err(ApiError::not_found()),
        }
    };
    let base_commit = load(base_id)?;
    let merge_base = cmp.merge_base.unwrap_or(base_id);
    let status = match (cmp.ahead_by, cmp.behind_by) {
        (0, 0) => "identical",
        (_, 0) => "ahead",
        (0, _) => "behind",
        _ => "diverged",
    };
    let page = explicit_page(ctx);
    let commits = match page {
        Some(p) => slice(&cmp.commits, p),
        None => &cmp.commits[..cmp.commits.len().min(COMPARE_COMMITS)],
    };
    let files = if page.is_none_or(|p| p.page == 1) {
        let shown = &cmp.files[..cmp.files.len().min(COMMIT_FILES)];
        shown
            .iter()
            .map(|f| DiffEntry::new(&view.links, head_id, f))
            .collect()
    } else {
        Vec::new()
    };
    let links = &view.links;
    let body = Comparison {
        url: format!("{}/compare/{basehead}", links.api),
        html_url: format!("{}/compare/{basehead}", links.web),
        permalink_url: format!("{}/compare/{base_id}...{head_id}", links.web),
        diff_url: format!("{}/compare/{basehead}.diff", links.web),
        patch_url: format!("{}/compare/{basehead}.patch", links.web),
        base_commit: view.commit(base_id, &base_commit),
        merge_base_commit: view.commit(merge_base, &*load(merge_base)?),
        status,
        ahead_by: cmp.ahead_by,
        behind_by: cmp.behind_by,
        total_commits: cmp.ahead_by,
        commits: commits.iter().map(|(id, c)| view.commit(*id, c)).collect(),
        files,
    };
    let reply = Reply::json(&body)?;
    match page {
        Some(p) => with_link(ctx, reply, p, cmp.commits.len()),
        None => Ok(reply),
    }
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
    if git
        .resolve_ref(tx, repo, &format!("refs/heads/{}", body.base))
        .is_err()
    {
        return Err(ApiError::new(404, "Base does not exist"));
    }
    if commit_at(git, tx, repo, &body.head).is_none() {
        return Err(ApiError::new(404, "Head does not exist"));
    }
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
    let view = View::of(ctx)?;
    let ObjectData::Commit(commit) = view.git.object(view.tx, repo, made)?.data else {
        return Err(ApiError::internal("a merge made no commit"));
    };
    let files = view.files(made, &commit)?;
    Reply::json(&view.with_files(made, &commit, &files)).map(|r| r.status(StatusCode::CREATED))
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

fn glob(pattern: &[u8], name: &[u8]) -> bool {
    match pattern {
        [] => name.is_empty(),
        [b'*', b'*', rest @ ..] => (0..=name.len()).any(|i| glob(rest, &name[i..])),
        [b'*', rest @ ..] => (0..=name.len())
            .take_while(|i| !name[..*i].contains(&b'/'))
            .any(|i| glob(rest, &name[i..])),
        [p, rest @ ..] => name.first() == Some(p) && glob(rest, &name[1..]),
    }
}

fn ref_matches(patterns: Option<&serde_json::Value>, full: &str, default: &str) -> bool {
    patterns
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .any(|p| match p {
            "~ALL" => true,
            "~DEFAULT_BRANCH" => full == default,
            p => glob(p.as_bytes(), full.as_bytes()),
        })
}

fn protected(tx: &Tx, repo: &Repo, branch: &str) -> bool {
    let full = format!("refs/heads/{branch}");
    let default = format!("refs/heads/{}", repo.default_branch);
    tx.table::<Rulesets>().iter().any(|r| {
        let applies = match r.source_type {
            RulesetSource::Repository => r.repo_id == Some(repo.id) || r.source_id == repo.id,
            RulesetSource::Organization => r.source_id == repo.owner_id,
        };
        let names = r.conditions.as_ref().and_then(|c| c.get("ref_name"));
        applies
            && r.target == RulesetTarget::Branch
            && r.enforcement == Enforcement::Active
            && ref_matches(names.and_then(|n| n.get("include")), &full, &default)
            && !ref_matches(names.and_then(|n| n.get("exclude")), &full, &default)
    })
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
    let is_protected = protected(view.tx, view.repo, &name);
    let own = format!("{}/branches/{name}", view.links.api);
    Reply::json(&Branch {
        commit: view.commit(id, &commit),
        links: BranchLinks {
            html: format!("{}/tree/{name}", view.links.web),
            own: own.clone(),
        },
        protected: is_protected,
        protection: Protection {
            enabled: is_protected,
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
    fn ruleset_globs_follow_fnmatch() {
        assert!(glob(b"refs/heads/release/*", b"refs/heads/release/v1"));
        assert!(!glob(
            b"refs/heads/release/*",
            b"refs/heads/release/v1/hotfix"
        ));
        assert!(glob(b"refs/heads/**", b"refs/heads/a/b"));
        assert!(glob(b"refs/heads/main", b"refs/heads/main"));
        assert!(!glob(b"refs/heads/main", b"refs/heads/mainline"));
    }

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
