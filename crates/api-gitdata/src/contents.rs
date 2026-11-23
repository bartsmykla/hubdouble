mod submodules;

use std::sync::Arc;

use hubdouble_git::{EntryMode, Git, ObjectData, ObjectId, ObjectKind, TreeEntry};
use hubdouble_model::{ApiError, FieldError};
use hubdouble_render::Urls;
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::Tx;
use serde::Serialize;

use self::submodules::SubmoduleUrls;
use crate::common::{Links, accepted_media, accepts, commit_at, wrapped_base64};
use crate::data::RAW;

const MAX_SYMLINKS: usize = 40;
const MAX_DIRECTORY_ENTRIES: usize = 1_000;
const MAX_INLINE_BYTES: usize = 1024 * 1024;
const MAX_FILE_BYTES: usize = 100 * 1024 * 1024;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    submodule_git_url: Option<String>,
    #[serde(rename = "_links")]
    links: ContentLinks,
}

#[derive(Serialize)]
struct Directory {
    #[serde(flatten)]
    content: Content,
    entries: Vec<Content>,
}

struct At<'a> {
    links: &'a Links,
    urls: &'a Urls,
    reference: &'a str,
    lists_submodule_type: bool,
}

impl At<'_> {
    fn self_url(&self, path: &str) -> String {
        format!("{}/contents/{path}?ref={}", self.links.api, self.reference)
    }

    fn content(
        &self,
        kind: &'static str,
        path: &str,
        sha: ObjectId,
        object: ObjectKind,
        size: usize,
        content: Option<(&'static str, String)>,
    ) -> Content {
        let name = path.rsplit('/').next().unwrap_or(path).to_owned();
        let own = self.self_url(path);
        let view = if kind == "dir" { "tree" } else { "blob" };
        let html = format!("{}/{view}/{}/{path}", self.links.web, self.reference);
        let git = Some(self.links.object(object, sha));
        let (encoding, data) = content.map_or((None, None), |(encoding, data)| {
            (Some(encoding), Some(data))
        });
        Content {
            kind,
            encoding,
            size,
            name,
            path: path.to_owned(),
            data,
            sha,
            url: own.clone(),
            git_url: git.clone(),
            html_url: Some(html.clone()),
            download_url: (kind == "file")
                .then(|| format!("{}/raw/{}/{path}", self.links.web, self.reference)),
            submodule_git_url: None,
            links: ContentLinks {
                own,
                git,
                html: Some(html),
            },
        }
    }

    fn submodule(&self, path: &str, sha: ObjectId, url: Option<&str>, listed: bool) -> Content {
        let own = self.self_url(path);
        let (git, html) = url
            .and_then(|url| submodules::github_repository(url, &self.links.web))
            .map_or((None, None), |repo| {
                (
                    Some(format!("{}/repos/{repo}/git/trees/{sha}", self.urls.api)),
                    Some(format!("{}/{repo}/tree/{sha}", self.urls.web)),
                )
            });
        Content {
            kind: if listed && !self.lists_submodule_type {
                "file"
            } else {
                "submodule"
            },
            encoding: None,
            size: 0,
            name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            path: path.to_owned(),
            data: None,
            sha,
            url: own.clone(),
            git_url: git.clone(),
            html_url: html.clone(),
            download_url: None,
            submodule_git_url: url.filter(|_| !listed).map(str::to_owned),
            links: ContentLinks { own, git, html },
        }
    }
}

#[derive(Serialize)]
struct Symlink {
    #[serde(flatten)]
    content: Content,
    target: String,
}

struct ContentTree<'a> {
    git: &'a Git,
    tx: &'a Tx,
    repo: i64,
    root: ObjectId,
}

impl ContentTree<'_> {
    fn entry(&self, path: &str) -> Option<(EntryMode, ObjectId)> {
        let mut found = (EntryMode::Tree, self.root);
        for name in path.split('/').filter(|part| !part.is_empty()) {
            if found.0 != EntryMode::Tree {
                return None;
            }
            let ObjectData::Tree(entries) = self.git.object(self.tx, self.repo, found.1).ok()?.data
            else {
                return None;
            };
            let entry = entries.iter().find(|entry| entry.name == name)?;
            found = (entry.mode, entry.id);
        }
        Some(found)
    }

    fn file_target(&self, link: &str, target: &[u8]) -> Option<Arc<[u8]>> {
        let mut path = target_path(link, target)?;
        for _ in 0..MAX_SYMLINKS {
            let (mode, id) = self.entry(&path)?;
            if !matches!(
                mode,
                EntryMode::File | EntryMode::Executable | EntryMode::Symlink
            ) {
                return None;
            }
            let ObjectData::Blob(data) = self.git.object(self.tx, self.repo, id).ok()?.data else {
                return None;
            };
            if mode != EntryMode::Symlink {
                return Some(data);
            }
            path = target_path(&path, &data)?;
        }
        None
    }

    fn submodule_urls(&self) -> Option<SubmoduleUrls> {
        let (mode, id) = self.entry(".gitmodules")?;
        if !matches!(mode, EntryMode::File | EntryMode::Executable) {
            return None;
        }
        let ObjectData::Blob(bytes) = self.git.object(self.tx, self.repo, id).ok()?.data else {
            return None;
        };
        submodules::parse(&bytes)
    }
}

fn target_path(link: &str, target: &[u8]) -> Option<String> {
    let target = std::str::from_utf8(target).ok()?;
    if target.is_empty() || target.starts_with('/') || target.ends_with('/') {
        return None;
    }
    let dir = link.rsplit_once('/').map_or("", |(dir, _)| dir);
    let mut parts: Vec<_> = dir.split('/').filter(|part| !part.is_empty()).collect();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            name => parts.push(name),
        }
    }
    Some(parts.join("/"))
}

pub(crate) fn get_content(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    let path = ctx.param("path")?.trim_matches('/');
    let reference = ctx
        .query()
        .get("ref")
        .filter(|r| !r.is_empty())
        .unwrap_or(&repo.default_branch);
    let (git, tx) = (ctx.git(), ctx.tx());
    let (_, commit) = commit_at(git, tx, repo.id, reference)
        .ok_or_else(|| ApiError::new(404, format!("No commit found for the ref {reference}")))?;
    let tree = ContentTree {
        git,
        tx,
        repo: repo.id,
        root: commit.tree,
    };
    let (mode, id) = tree.entry(path).ok_or_else(ApiError::not_found)?;
    let links = Links::of(ctx)?;
    let at = At {
        links: &links,
        urls: ctx.urls(),
        reference,
        lists_submodule_type: ctx.header("x-github-api-version") == Some("2026-03-10"),
    };
    if mode == EntryMode::Submodule {
        let urls = tree.submodule_urls().ok_or_else(ApiError::not_found)?;
        let url = urls.get(path).ok_or_else(ApiError::not_found)?;
        return Reply::json(&at.submodule(path, id, Some(url), false));
    }
    let data = git
        .object(tx, repo.id, id)
        .map_err(|_| ApiError::not_found())?
        .data;
    match data {
        ObjectData::Blob(data) if mode == EntryMode::Symlink => {
            if let Some(target) = tree.file_target(path, &data) {
                return file(ctx, &at, path, id, &target);
            }
            Reply::json(&Symlink {
                content: at.content("symlink", path, id, ObjectKind::Blob, data.len(), None),
                target: String::from_utf8_lossy(&data).into_owned(),
            })
        }
        ObjectData::Blob(data) => file(ctx, &at, path, id, &data),
        ObjectData::Tree(entries) => {
            let entries = entries.iter().take(MAX_DIRECTORY_ENTRIES);
            let urls = if entries.clone().any(|e| e.mode == EntryMode::Submodule) {
                tree.submodule_urls()
            } else {
                None
            };
            let listing = entries
                .map(|e| entry_content(&tree, &at, path, e, urls.as_ref()))
                .collect::<Result<Vec<_>, _>>()?;
            if accepts(ctx, "object") {
                let content = at.content("dir", path, id, ObjectKind::Tree, 0, None);
                Reply::json(&Directory {
                    content,
                    entries: listing,
                })
            } else {
                Reply::json(&listing)
            }
        }
        ObjectData::Commit(_) | ObjectData::Tag(_) => Err(ApiError::not_found()),
    }
}

fn file(
    ctx: &Ctx<'_>,
    at: &At<'_>,
    path: &str,
    id: ObjectId,
    data: &[u8],
) -> Result<Reply, ApiError> {
    if data.len() > MAX_FILE_BYTES {
        return Err(ApiError::forbidden(concat!(
            "This endpoint can only return blobs smaller than 100 MB in size. ",
            "The requested blob is too large to fetch via the API, ",
            "but you can always clone the repository via Git to obtain it."
        ))
        .with_docs("https://docs.github.com/rest/repos/contents#get-repository-content")
        .with_error(FieldError {
            resource: "Blob".to_owned(),
            field: "data".to_owned(),
            code: "too_large".to_owned(),
            message: None,
            value: None,
        }));
    }
    if let Some(media) = accepted_media(ctx, "raw") {
        let content_type = match media {
            "application/vnd.github.raw+json" => "application/vnd.github.raw+json; charset=utf-8",
            "application/vnd.github.v3.raw" => "application/vnd.github.v3.raw; charset=utf-8",
            "application/vnd.github.v3.raw+json" => {
                "application/vnd.github.v3.raw+json; charset=utf-8"
            }
            _ => RAW,
        };
        return Ok(Reply::raw(content_type, data.to_vec()));
    }
    let content = if data.len() > MAX_INLINE_BYTES {
        ("none", String::new())
    } else {
        ("base64", wrapped_base64(data))
    };
    Reply::json(&at.content(
        "file",
        path,
        id,
        ObjectKind::Blob,
        data.len(),
        Some(content),
    ))
}

fn entry_content(
    tree: &ContentTree<'_>,
    at: &At<'_>,
    dir: &str,
    e: &TreeEntry,
    submodule_urls: Option<&SubmoduleUrls>,
) -> Result<Content, ApiError> {
    let path = if dir.is_empty() {
        e.name.clone()
    } else {
        format!("{dir}/{}", e.name)
    };
    let (kind, object, size) = match e.mode {
        EntryMode::Tree => ("dir", ObjectKind::Tree, 0),
        EntryMode::Submodule => {
            let url = submodule_urls.and_then(|urls| urls.get(&path).map(String::as_str));
            return Ok(at.submodule(&path, e.id, url, true));
        }
        _ => {
            let ObjectData::Blob(data) = tree.git.object(tree.tx, tree.repo, e.id)?.data else {
                return Err(ApiError::internal("a file entry is not a blob"));
            };
            let kind = if e.mode == EntryMode::Symlink && tree.file_target(&path, &data).is_none() {
                "symlink"
            } else {
                "file"
            };
            (kind, ObjectKind::Blob, data.len())
        }
    };
    Ok(at.content(kind, &path, e.id, object, size, None))
}
