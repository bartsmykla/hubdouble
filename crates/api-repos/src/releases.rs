use std::fmt::Write as _;

use bytes::Bytes;
use http::header::LINK;
use hubdouble_admin::{Registrar, SeedCtx};
use hubdouble_auth::{Actor, Target};
use hubdouble_git::{Git, GitError, ObjectId};
use hubdouble_model::{
    ApiError, AssetState, Grant, Release, ReleaseAsset, Repo, Requirement, Scope, node_id,
};
use hubdouble_render::{self as render, SimpleUser, Urls};
use hubdouble_rest::{Ctx, Page, Reply};
use hubdouble_store::{Accounts, Blob, Blobs, ReleaseAssets, Releases, Tx, TxMut};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::CONTENTS;

const CONTENTS_WRITE: Requirement = Requirement {
    any_of: &[Grant::write("contents")],
    scope: Scope::Repo,
    scopes: &[],
};

const OCTET_STREAM: &str = "application/octet-stream";

pub(crate) fn register(r: &mut Registrar<'_>) {
    r.rest.handle("repos/list-releases", CONTENTS, list);
    r.rest.handle("repos/get-release-by-tag", CONTENTS, by_tag);
    r.rest.handle("repos/get-release-asset", CONTENTS, asset);
    r.admin.kind("release", release_kind);
}

/// The release as REST and the `release` payload write it.
pub(crate) fn render(tx: &Tx, urls: &Urls, r: &Release) -> Result<render::Release, ApiError> {
    let repo = tx
        .table::<hubdouble_store::Repos>()
        .get(r.repo_id)
        .ok_or_else(ApiError::not_found)?;
    let full_name = crate::full_name(tx, repo)?;
    let accounts = tx.table::<Accounts>();
    let author = accounts
        .get(r.author_id)
        .ok_or_else(|| ApiError::internal("the release has no author"))?;
    let assets = tx
        .table::<ReleaseAssets>()
        .children(r.id)
        .map(|a| {
            let uploader = accounts
                .get(a.uploader_id)
                .map(|u| SimpleUser::new(urls, u));
            render::ReleaseAsset::new(urls, a, &full_name, &r.tag_name, uploader)
        })
        .collect();
    Ok(render::Release::new(
        urls,
        r,
        &full_name,
        SimpleUser::new(urls, author),
        assets,
    ))
}

/// The `release` payload's own part.
pub(crate) fn payload(tx: &Tx, r: &Release) -> Result<Map<String, Value>, ApiError> {
    let urls = Urls::new(tx.config(), "");
    let release = serde_json::to_value(render(tx, &urls, r)?)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Map::from_iter([("release".to_owned(), release)]))
}

fn visible(tx: &Tx, actor: &Actor, repo: &std::sync::Arc<Repo>, r: &Release) -> bool {
    !r.draft || hubdouble_auth::authorize(tx, actor, &CONTENTS_WRITE, Target::Repo(repo)).is_ok()
}

fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    let tx = ctx.tx();
    let mut releases: Vec<_> = tx
        .table::<Releases>()
        .in_repo(repo.id)
        .filter(|r| visible(tx, ctx.actor(), repo, r))
        .collect();
    releases.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    let Page { page, per_page } = ctx.page_params();
    let start = (page - 1).saturating_mul(per_page).min(releases.len());
    let end = start.saturating_add(per_page).min(releases.len());
    let rendered = releases[start..end]
        .iter()
        .map(|r| render(tx, ctx.urls(), r))
        .collect::<Result<Vec<_>, _>>()?;
    let reply = Reply::json(&rendered)?;
    let last = releases.len().div_ceil(per_page).max(1);
    match ctx.link_header(page, last) {
        Some(link) => reply.header(LINK, &link),
        None => Ok(reply),
    }
}

fn by_tag(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    let tx = ctx.tx();
    let r = tx
        .table::<Releases>()
        .by_tag(repo.id, ctx.param("tag")?)
        .filter(|r| !r.draft)
        .ok_or_else(ApiError::not_found)?;
    Reply::json(&render(tx, ctx.urls(), r)?)
}

fn wants_bytes(ctx: &Ctx<'_>) -> bool {
    ctx.header("accept").is_some_and(|a| {
        a.split(',')
            .filter_map(|r| r.split(';').next())
            .any(|r| r.trim().eq_ignore_ascii_case(OCTET_STREAM))
    })
}

fn asset(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    let tx = ctx.tx();
    let a = tx
        .table::<ReleaseAssets>()
        .get(ctx.param_i64("asset_id")?)
        .filter(|a| a.repo_id == repo.id)
        .ok_or_else(ApiError::not_found)?;
    let release = tx
        .table::<Releases>()
        .get(a.release_id)
        .filter(|r| visible(tx, ctx.actor(), repo, r))
        .ok_or_else(ApiError::not_found)?;
    if wants_bytes(ctx) {
        return Reply::redirect(&format!("{}/_blobs/{}", ctx.urls().base, a.blob_id));
    }
    let accounts = tx.table::<Accounts>();
    let uploader = accounts
        .get(a.uploader_id)
        .map(|u| SimpleUser::new(ctx.urls(), u));
    Reply::json(&render::ReleaseAsset::new(
        ctx.urls(),
        a,
        &crate::full_name(tx, repo)?,
        &release.tag_name,
        uploader,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssetSeed {
    #[serde(default)]
    id: Option<i64>,
    name: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseSeed {
    #[serde(default)]
    id: Option<i64>,
    repository: String,
    tag_name: String,
    #[serde(default)]
    target_commitish: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    assets: Vec<AssetSeed>,
}

fn sha256(data: &[u8]) -> String {
    let mut out = String::with_capacity(71);
    out.push_str("sha256:");
    for b in Sha256::digest(data) {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Creates `refs/tags/<tag>` at the commit `target` names when the tag is missing, as GitHub
/// does when it publishes a release.
pub(crate) fn ensure_tag(
    tx: &mut TxMut<'_>,
    git: &Git,
    repo: &Repo,
    sender: i64,
    tag: &str,
    target: &str,
) -> Result<(), ApiError> {
    let name = format!("refs/tags/{tag}");
    match git.resolve_ref(tx, repo.id, &name) {
        Ok(_) => return Ok(()),
        Err(GitError::NotFound) => {}
        Err(e) => return Err(e.into()),
    }
    let bad = || ApiError::unprocessable(format!("target_commitish {target} is not a commit"));
    let id = match target.parse::<ObjectId>() {
        Ok(id) => id,
        Err(_) => git.resolve_ref(tx, repo.id, target).map_err(|_| bad())?,
    };
    let commit = git.peel_to_commit(tx, repo.id, id).map_err(|_| bad())?;
    git.update_ref(tx, repo.id, sender, &name, None, Some(commit), false)?;
    Ok(())
}

fn release_kind(ctx: &mut SeedCtx<'_>, v: Value) -> Result<(), ApiError> {
    let s: ReleaseSeed = crate::admin::decode(v)?;
    let repo = crate::admin::repo_by_full_name(&ctx.tx, &s.repository)?;
    let author = match &s.author {
        Some(login) => {
            ctx.tx
                .table::<Accounts>()
                .by_login(login)
                .ok_or_else(|| ApiError::unprocessable(format!("no account {login}")))?
                .id
        }
        None => repo.owner_id,
    };
    if s.tag_name.is_empty() {
        return Err(ApiError::unprocessable("tag_name is empty"));
    }
    let target = s
        .target_commitish
        .unwrap_or_else(|| repo.default_branch.clone());
    if !s.draft {
        ensure_tag(&mut ctx.tx, ctx.git, &repo, author, &s.tag_name, &target)?;
    }
    let id = s.id.unwrap_or_else(|| ctx.tx.next_id());
    let now = ctx.clock.now();
    let release = Release {
        id,
        node_id: node_id::entity(node_id::RELEASE, id),
        repo_id: repo.id,
        tag_name: s.tag_name,
        target_commitish: target,
        name: s.name,
        body: s.body,
        draft: s.draft,
        prerelease: s.prerelease,
        author_id: author,
        created_at: now,
        published_at: (!s.draft).then_some(now),
        updated_at: now,
    };
    ctx.tx.table_mut::<Releases>().insert(release.clone())?;
    for a in s.assets {
        let asset_id = a.id.unwrap_or_else(|| ctx.tx.next_id());
        let data = Bytes::from(a.content.into_bytes());
        let content_type = a.content_type.unwrap_or_else(|| OCTET_STREAM.to_owned());
        let blob_id = uuid::Uuid::new_v4().to_string();
        ctx.tx.table_mut::<Blobs>().insert(
            blob_id.clone(),
            Blob {
                content_type: content_type.clone(),
                data: data.clone(),
            },
        );
        ctx.tx.table_mut::<ReleaseAssets>().insert(ReleaseAsset {
            id: asset_id,
            node_id: node_id::entity(node_id::RELEASE_ASSET, asset_id),
            repo_id: repo.id,
            release_id: id,
            name: a.name,
            label: a.label,
            content_type,
            size: i64::try_from(data.len()).unwrap_or(i64::MAX),
            digest: Some(sha256(&data)),
            download_count: 0,
            state: AssetState::Uploaded,
            uploader_id: author,
            blob_id,
            created_at: now,
            updated_at: now,
        })?;
    }
    if !release.draft {
        let p = payload(&ctx.tx, &release)?;
        crate::emit(&mut ctx.tx, "release", "published", repo.id, author, p);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::sha256;

    #[test]
    fn digests_are_githubs_sha256_form() {
        assert_eq!(
            sha256(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
