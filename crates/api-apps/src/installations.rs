use std::collections::BTreeMap;
use std::fmt::Write as _;

use http::{StatusCode, header::LINK};
use hubdouble_auth::{Actor, INSTALLATION_TOKEN_TTL, NarrowRequest, narrow};
use hubdouble_model::{
    Access, Account, AccountKind, ApiError, Installation, Repo, RepositorySelection, Requirement,
    Scope, Token, TokenKind,
};
use hubdouble_render::{self as render, Urls};
use hubdouble_rest::{Ctx, Page, Registry, Reply};
use hubdouble_store::{Accounts, Apps, Counters, Installations, Repos, Tokens, Tx};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};

use crate::{APP, app};

const INSTALLATION: Requirement = Requirement {
    scope: Scope::Installation,
    any_of: &[],
    scopes: &[],
};

pub(crate) fn register(registry: &mut Registry) {
    registry.handle("apps/list-installations", APP, list);
    registry.handle("apps/get-org-installation", APP, for_org);
    registry.handle("apps/get-repo-installation", APP, for_repo);
    registry.handle("apps/create-installation-access-token", APP, mint);
    registry.handle(
        "apps/list-repos-accessible-to-installation",
        INSTALLATION,
        repositories,
    );
    registry.handle(
        "apps/revoke-installation-access-token",
        INSTALLATION,
        revoke,
    );
}

pub(crate) fn render(
    tx: &Tx,
    urls: &Urls,
    installation: &Installation,
) -> Result<render::Installation, ApiError> {
    let app = tx
        .table::<Apps>()
        .get(installation.app_id)
        .ok_or_else(ApiError::not_found)?;
    let accounts = tx.table::<Accounts>();
    let account = accounts
        .get(installation.account_id)
        .ok_or_else(ApiError::not_found)?;
    let suspended_by = installation.suspended_by.and_then(|id| accounts.get(id));
    Ok(render::Installation::new(
        urls,
        installation,
        app,
        account,
        suspended_by.map(AsRef::as_ref),
    ))
}

fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let app = app(ctx)?;
    let since = ctx.since(
        "https://docs.github.com/rest/apps/apps#list-installations-for-the-authenticated-app",
    )?;
    let items = ctx
        .tx()
        .table::<Installations>()
        .children(app.id)
        .filter(|i| since.is_none_or(|s| s.admits(i.updated_at)));
    let params = ctx.page_params()?;
    let (items, total) = page(params, items, |i| render(ctx.tx(), ctx.urls(), i))?;
    paged_reply(ctx, params, total, &items)
}

fn for_org(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let app = app(ctx)?;
    let account = ctx
        .tx()
        .table::<Accounts>()
        .by_login(ctx.param("org")?)
        .filter(|a| a.kind == AccountKind::Organization)
        .ok_or_else(ApiError::not_found)?;
    let installation = ctx
        .tx()
        .table::<Installations>()
        .of(app.id, account.id)
        .ok_or_else(ApiError::not_found)?;
    Reply::json(&render(ctx.tx(), ctx.urls(), installation)?)
}

fn for_repo(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let app = app(ctx)?;
    let repo = ctx.repo()?;
    let installation = ctx
        .tx()
        .table::<Installations>()
        .of(app.id, repo.owner_id)
        .filter(|i| covers(i, repo))
        .ok_or_else(ApiError::not_found)?;
    Reply::json(&render(ctx.tx(), ctx.urls(), installation)?)
}

#[derive(Deserialize)]
struct TokenRequest {
    #[serde(default)]
    repositories: Vec<String>,
    #[serde(default)]
    repository_ids: Vec<i64>,
    permissions: Option<BTreeMap<String, Access>>,
}

fn mint(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let app = app(ctx)?;
    let id = ctx.param_i64("installation_id")?;
    let installation = ctx
        .tx()
        .table::<Installations>()
        .get(id)
        .filter(|i| i.app_id == app.id)
        .ok_or_else(ApiError::not_found)?;
    if installation.suspended_at.is_some() {
        return Err(ApiError::forbidden("This installation has been suspended"));
    }
    let body: TokenRequest = ctx.body()?;
    let mut narrowed = narrow(
        ctx.tx(),
        installation,
        &NarrowRequest {
            repositories: &body.repositories,
            repository_ids: &body.repository_ids,
            permissions: body.permissions.as_ref(),
        },
    )?;
    narrowed
        .permissions
        .entry("metadata".into())
        .or_insert(Access::Read);
    let now = ctx.now();
    let expires_at = now.plus_seconds(INSTALLATION_TOKEN_TTL);
    let value = secret()?;
    let selection = selection(installation, narrowed.repo_ids.as_deref());
    let repositories = (selection == RepositorySelection::Selected)
        .then(|| render_repos(ctx, installation, narrowed.repo_ids.as_deref()))
        .transpose()?;
    let answer = render::InstallationToken {
        token: value.clone(),
        expires_at,
        permissions: narrowed.permissions.clone(),
        repository_selection: selection.as_str(),
        repositories,
    };
    let app_id = app.id;
    let tx = ctx.tx_mut()?;
    let owner_seeded = tx.table::<Counters>().is_seeded(id);
    let token_id = tx.next_id();
    tx.table_mut::<Tokens>().insert(Token {
        id: token_id,
        token: value,
        kind: TokenKind::Installation,
        app_id: Some(app_id),
        installation_id: Some(id),
        user_id: None,
        repo_ids: narrowed.repo_ids,
        permissions: narrowed.permissions,
        scopes: Vec::new(),
        expires_at: Some(expires_at),
        revoked: false,
        owner_seeded,
        created_at: now,
    })?;
    Ok(Reply::json(&answer)?.status(StatusCode::CREATED))
}

fn secret() -> Result<String, ApiError> {
    let mut bytes = [0_u8; 18];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let mut value = String::with_capacity(40);
    value.push_str("ghs_");
    for byte in bytes {
        let _ = write!(value, "{byte:02x}");
    }
    Ok(value)
}

fn revoke(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let Actor::Installation { token, .. } = ctx.actor() else {
        return Err(ApiError::unauthorized());
    };
    let token = Token {
        revoked: true,
        ..(**token).clone()
    };
    ctx.tx_mut()?.table_mut::<Tokens>().replace(token)?;
    Ok(Reply::empty())
}

#[derive(Serialize)]
struct RepositoryPage {
    total_count: usize,
    repository_selection: &'static str,
    repositories: Vec<render::Repository>,
}

fn repositories(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let Actor::Installation {
        installation,
        token,
        ..
    } = ctx.actor()
    else {
        return Err(ApiError::unauthorized());
    };
    let owner = ctx
        .tx()
        .table::<Accounts>()
        .get(installation.account_id)
        .ok_or_else(ApiError::not_found)?;
    let ids = token.repo_ids.as_deref();
    let selection = selection(installation, ids);
    let items = repos(ctx.tx(), installation, ids);
    let params = ctx.page_params()?;
    let (repositories, total_count) = page(params, items, |repo| {
        Ok(render::Repository::new(ctx.urls(), repo, owner))
    })?;
    paged_reply(
        ctx,
        params,
        total_count,
        &RepositoryPage {
            total_count,
            repository_selection: selection.as_str(),
            repositories,
        },
    )
}

pub(crate) fn repos<'a>(
    tx: &'a Tx,
    installation: &'a Installation,
    ids: Option<&'a [i64]>,
) -> impl Iterator<Item = &'a Repo> {
    let repos = tx.table::<Repos>();
    let narrowed = ids.into_iter().flatten().filter_map(|id| repos.get(*id));
    let all = ids
        .is_none()
        .then(|| repos.children(installation.account_id));
    narrowed
        .chain(all.into_iter().flatten())
        .map(AsRef::as_ref)
        .filter(move |repo| covers(installation, repo))
}

fn covers(installation: &Installation, repo: &Repo) -> bool {
    installation.account_id == repo.owner_id
        && (installation.repository_selection == RepositorySelection::All
            || installation.repo_ids.contains(&repo.id))
}

fn selection(installation: &Installation, ids: Option<&[i64]>) -> RepositorySelection {
    if ids.is_some() {
        RepositorySelection::Selected
    } else {
        installation.repository_selection
    }
}

fn render_repos(
    ctx: &Ctx<'_>,
    installation: &Installation,
    ids: Option<&[i64]>,
) -> Result<Vec<render::Repository>, ApiError> {
    let owner: &Account = ctx
        .tx()
        .table::<Accounts>()
        .get(installation.account_id)
        .ok_or_else(ApiError::not_found)?;
    Ok(repos(ctx.tx(), installation, ids)
        .map(|repo| render::Repository::new(ctx.urls(), repo, owner))
        .collect())
}

fn page<T, R>(
    Page { page, per_page }: Page,
    items: impl Iterator<Item = T>,
    mut render: impl FnMut(T) -> Result<R, ApiError>,
) -> Result<(Vec<R>, usize), ApiError> {
    let start = (page - 1).saturating_mul(per_page);
    let end = start.saturating_add(per_page);
    let mut rendered = Vec::new();
    let mut total = 0;
    for (index, item) in items.enumerate() {
        total += 1;
        if (start..end).contains(&index) {
            rendered.push(render(item)?);
        }
    }
    Ok((rendered, total))
}

fn paged_reply(
    ctx: &Ctx<'_>,
    Page { page, per_page }: Page,
    total: usize,
    body: &impl Serialize,
) -> Result<Reply, ApiError> {
    let reply = Reply::json(body)?;
    if let Some(link) = ctx.link_header(page, total.div_ceil(per_page).max(1)) {
        return reply.header(LINK, &link);
    }
    Ok(reply)
}
