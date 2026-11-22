use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use hubdouble_model::node_id;
use hubdouble_model::{
    Access, Account, AccountKind, ApiError, App, Collaborator, Event, Installation, Membership,
    MembershipState, OrgRole, Repo, RepoRole, RepositorySelection, Timestamp, Token, TokenKind,
    Visibility, Webhook,
};
use hubdouble_store::{Accounts, Apps, Counters, Installations, Memberships, Repos, Tokens, Tx};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::SeedCtx;
use crate::history::{self, CommitSeed, History, TagSeed, Vcs};

/// The classic scopes a seeded personal token gets when the seed names none.
pub const DEFAULT_SCOPES: [&str; 4] = ["repo", "workflow", "read:org", "user"];

fn decode<T: DeserializeOwned>(v: serde_json::Value) -> Result<T, ApiError> {
    serde_json::from_value(v).map_err(|e| ApiError::unprocessable(e.to_string()))
}

fn id_or_next(ctx: &mut SeedCtx<'_>, given: Option<i64>) -> i64 {
    given.unwrap_or_else(|| ctx.tx.next_id())
}

fn account<'t>(tx: &'t Tx, login: &str) -> Result<&'t Arc<Account>, ApiError> {
    tx.table::<Accounts>()
        .by_login(login)
        .ok_or_else(|| ApiError::unprocessable(format!("no account {login}")))
}

fn user(tx: &Tx, login: &str) -> Result<Arc<Account>, ApiError> {
    let a = account(tx, login)?;
    if a.kind == AccountKind::Organization {
        return Err(ApiError::unprocessable(format!(
            "{login} is an organization"
        )));
    }
    Ok(a.clone())
}

fn repo_by_full_name(tx: &Tx, full: &str) -> Result<Arc<Repo>, ApiError> {
    let unknown = || ApiError::unprocessable(format!("no repository {full}"));
    let (owner, name) = full.split_once('/').ok_or_else(unknown)?;
    let owner = tx.table::<Accounts>().by_login(owner).ok_or_else(unknown)?;
    tx.table::<Repos>()
        .by_name(owner.id, name)
        .cloned()
        .ok_or_else(unknown)
}

fn random_hex(len: usize) -> String {
    let mut out = String::with_capacity(len + 32);
    while out.len() < len {
        out.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    out.truncate(len);
    out
}

fn blank_account(
    id: i64,
    node_prefix: &str,
    login: String,
    kind: AccountKind,
    now: Timestamp,
) -> Account {
    Account {
        id,
        node_id: node_id::entity(node_prefix, id),
        login,
        kind,
        name: None,
        email: None,
        company: None,
        blog: None,
        location: None,
        bio: None,
        twitter_username: None,
        description: None,
        site_admin: false,
        verified_email: true,
        sso_enforced: false,
        app_id: None,
        created_at: now,
        updated_at: now,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenSeed {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default)]
    expires_at: Option<Timestamp>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserSeed {
    #[serde(default)]
    id: Option<i64>,
    login: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    company: Option<String>,
    #[serde(default)]
    blog: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    bio: Option<String>,
    #[serde(default)]
    twitter_username: Option<String>,
    #[serde(default)]
    site_admin: bool,
    #[serde(default = "yes")]
    verified_email: bool,
    #[serde(default)]
    tokens: Vec<TokenSeed>,
}

fn yes() -> bool {
    true
}

pub(crate) fn user_kind(ctx: &mut SeedCtx<'_>, v: serde_json::Value) -> Result<(), ApiError> {
    let s: UserSeed = decode(v)?;
    let kind = match s.kind.as_deref() {
        None | Some("User") => AccountKind::User,
        Some("Bot") => AccountKind::Bot,
        Some(other) => {
            return Err(ApiError::unprocessable(format!(
                "type {other} is not User or Bot"
            )));
        }
    };
    if s.login.is_empty() {
        return Err(ApiError::unprocessable("login is empty"));
    }
    let id = id_or_next(ctx, s.id);
    let prefix = if kind == AccountKind::Bot {
        node_id::BOT
    } else {
        node_id::USER
    };
    let mut a = blank_account(id, prefix, s.login, kind, ctx.clock.now());
    a.name = s.name;
    a.email = s.email;
    a.company = s.company;
    a.blog = s.blog;
    a.location = s.location;
    a.bio = s.bio;
    a.twitter_username = s.twitter_username;
    a.site_admin = s.site_admin;
    a.verified_email = s.verified_email;
    ctx.tx.table_mut::<Accounts>().insert(a)?;
    for t in s.tokens {
        personal_token(ctx, id, t)?;
    }
    Ok(())
}

fn personal_token(ctx: &mut SeedCtx<'_>, user_id: i64, t: TokenSeed) -> Result<(), ApiError> {
    let token = match t.token {
        Some(v) if v.is_empty() => return Err(ApiError::unprocessable("token is empty")),
        Some(v) => v,
        None => format!("ghp_{}", random_hex(36)),
    };
    let id = ctx.tx.next_id();
    let owner_seeded = ctx.tx.table::<Counters>().is_seeded(user_id);
    ctx.tx.table_mut::<Tokens>().insert(Token {
        id,
        token,
        kind: TokenKind::Personal,
        app_id: None,
        installation_id: None,
        user_id: Some(user_id),
        repo_ids: None,
        permissions: BTreeMap::new(),
        scopes: t
            .scopes
            .unwrap_or_else(|| DEFAULT_SCOPES.map(str::to_owned).to_vec()),
        expires_at: t.expires_at,
        revoked: false,
        owner_seeded,
        created_at: ctx.clock.now(),
    })?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenKindSeed {
    user: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default)]
    expires_at: Option<Timestamp>,
}

pub(crate) fn token_kind(ctx: &mut SeedCtx<'_>, v: serde_json::Value) -> Result<(), ApiError> {
    let s: TokenKindSeed = decode(v)?;
    let user_id = user(&ctx.tx, &s.user)?.id;
    personal_token(
        ctx,
        user_id,
        TokenSeed {
            token: s.token,
            scopes: s.scopes,
            expires_at: s.expires_at,
        },
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberSeed {
    login: String,
    #[serde(default)]
    role: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OrgSeed {
    #[serde(default)]
    id: Option<i64>,
    login: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    company: Option<String>,
    #[serde(default)]
    blog: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    twitter_username: Option<String>,
    #[serde(default)]
    sso_enforced: bool,
    #[serde(default)]
    members: Vec<MemberSeed>,
}

pub(crate) fn org_kind(ctx: &mut SeedCtx<'_>, v: serde_json::Value) -> Result<(), ApiError> {
    let s: OrgSeed = decode(v)?;
    let id = id_or_next(ctx, s.id);
    let mut a = blank_account(
        id,
        node_id::ORGANIZATION,
        s.login,
        AccountKind::Organization,
        ctx.clock.now(),
    );
    a.name = s.name;
    a.email = s.email;
    a.description = s.description;
    a.company = s.company;
    a.blog = s.blog;
    a.location = s.location;
    a.twitter_username = s.twitter_username;
    a.sso_enforced = s.sso_enforced;
    ctx.tx.table_mut::<Accounts>().insert(a)?;
    for m in s.members {
        let user_id = user(&ctx.tx, &m.login)?.id;
        let role = match m.role.as_deref() {
            None => OrgRole::Member,
            Some(r) => OrgRole::from_rest(r).ok_or_else(|| {
                ApiError::unprocessable(format!("role {r} is not admin or member"))
            })?,
        };
        let mid = ctx.tx.next_id();
        ctx.tx.table_mut::<Memberships>().insert(Membership {
            id: mid,
            org_id: id,
            user_id,
            role,
            state: MembershipState::Active,
        })?;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CollaboratorSeed {
    login: String,
    #[serde(default)]
    permission: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a GitHub repository setting"
)]
struct RepoSeed {
    #[serde(default)]
    id: Option<i64>,
    owner: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    private: Option<bool>,
    #[serde(default)]
    fork_of: Option<String>,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    is_template: bool,
    #[serde(default = "yes")]
    has_issues: bool,
    #[serde(default = "yes")]
    has_projects: bool,
    #[serde(default = "yes")]
    has_wiki: bool,
    #[serde(default)]
    has_discussions: bool,
    #[serde(default = "yes")]
    allow_merge_commit: bool,
    #[serde(default = "yes")]
    allow_squash_merge: bool,
    #[serde(default = "yes")]
    allow_rebase_merge: bool,
    #[serde(default)]
    allow_auto_merge: bool,
    #[serde(default)]
    delete_branch_on_merge: bool,
    #[serde(default)]
    collaborators: Vec<CollaboratorSeed>,
    #[serde(default)]
    files: BTreeMap<String, String>,
    #[serde(default)]
    from_dir: Option<String>,
    #[serde(default)]
    branches: Vec<String>,
    #[serde(default)]
    tags: Vec<TagSeed>,
    #[serde(default)]
    commits: Vec<CommitSeed>,
    #[serde(default)]
    sender: Option<String>,
}

pub(crate) fn repository_kind(ctx: &mut SeedCtx<'_>, v: serde_json::Value) -> Result<(), ApiError> {
    let git = ctx.git;
    apply_repository(ctx, git, v)
}

pub(crate) fn apply_repository(
    ctx: &mut SeedCtx<'_>,
    vcs: &impl Vcs,
    v: serde_json::Value,
) -> Result<(), ApiError> {
    let s: RepoSeed = decode(v)?;
    let owner = account(&ctx.tx, &s.owner)?.id;
    let sender = match &s.sender {
        Some(login) => user(&ctx.tx, login)?.id,
        None => owner,
    };
    let visibility = match (s.visibility.as_deref(), s.private) {
        (Some(v), _) => Visibility::from_rest(v).ok_or_else(|| {
            ApiError::unprocessable(format!("visibility {v} is not public, private or internal"))
        })?,
        (None, Some(true)) => Visibility::Private,
        (None, _) => Visibility::Public,
    };
    let fork_of = s
        .fork_of
        .as_deref()
        .map(|full| repo_by_full_name(&ctx.tx, full))
        .transpose()?;
    let mut collaborators = Vec::with_capacity(s.collaborators.len());
    for c in &s.collaborators {
        let role = match c.permission.as_deref() {
            None => RepoRole::Write,
            Some(p) => RepoRole::from_rest(p).ok_or_else(|| {
                ApiError::unprocessable(format!("permission {p} is not a repository role"))
            })?,
        };
        collaborators.push(Collaborator {
            user_id: user(&ctx.tx, &c.login)?.id,
            role,
        });
    }
    let mut files: BTreeMap<String, Arc<[u8]>> = BTreeMap::new();
    if let Some(dir) = &s.from_dir {
        read_dir(Path::new(dir), "", &mut files)?;
    }
    for (path, text) in s.files {
        let text = history::expand_sha(vcs, &ctx.tx, &text)?;
        files.insert(path, text.into_bytes().into());
    }
    let id = id_or_next(ctx, s.id);
    let now = ctx.clock.now();
    let default_branch = s
        .default_branch
        .or_else(|| fork_of.as_ref().map(|p| p.default_branch.clone()))
        .unwrap_or_else(|| "main".to_owned());
    let repo = Repo {
        id,
        node_id: node_id::entity(node_id::REPOSITORY, id),
        owner_id: owner,
        name: s.name,
        description: s.description,
        homepage: s.homepage,
        language: s.language,
        topics: s.topics,
        visibility,
        fork_of: fork_of.as_ref().map(|p| p.id),
        default_branch: default_branch.clone(),
        archived: s.archived,
        disabled: false,
        is_template: s.is_template,
        has_issues: s.has_issues,
        has_projects: s.has_projects,
        has_wiki: s.has_wiki,
        has_discussions: s.has_discussions,
        allow_merge_commit: s.allow_merge_commit,
        allow_squash_merge: s.allow_squash_merge,
        allow_rebase_merge: s.allow_rebase_merge,
        allow_auto_merge: s.allow_auto_merge,
        delete_branch_on_merge: s.delete_branch_on_merge,
        collaborators,
        created_at: now,
        updated_at: now,
        pushed_at: None,
    };
    ctx.tx.table_mut::<Repos>().insert(repo.clone())?;
    if let Some(parent) = &fork_of {
        history::copy_refs(vcs, &mut ctx.tx, parent.id, id)?;
    }
    let pushed = history::write(
        vcs,
        &mut ctx.tx,
        id,
        &default_branch,
        History {
            files,
            tags: s.tags,
            branches: s.branches,
            commits: s.commits,
        },
    )?;
    if pushed {
        ctx.tx.table_mut::<Repos>().replace(Repo {
            pushed_at: Some(now),
            ..repo
        })?;
    }
    hubdouble_hooks::emit(
        &mut ctx.tx,
        Event {
            name: "repository",
            action: Some("created"),
            repo_id: Some(id),
            org_id: None,
            sender_id: sender,
            app_id: None,
            installation_id: None,
            payload: serde_json::Map::new(),
        },
    );
    Ok(())
}

fn read_dir(
    dir: &Path,
    prefix: &str,
    out: &mut BTreeMap<String, Arc<[u8]>>,
) -> Result<(), ApiError> {
    let failed =
        |e: std::io::Error| ApiError::unprocessable(format!("from_dir {}: {e}", dir.display()));
    for entry in std::fs::read_dir(dir).map_err(failed)? {
        let entry = entry.map_err(failed)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = format!("{prefix}{name}");
        if entry.file_type().map_err(failed)?.is_dir() {
            read_dir(&entry.path(), &format!("{path}/"), out)?;
        } else {
            out.insert(path, std::fs::read(entry.path()).map_err(failed)?.into());
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookSeed {
    url: String,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default = "yes")]
    active: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppSeed {
    #[serde(default)]
    id: Option<i64>,
    slug: String,
    #[serde(default)]
    name: Option<String>,
    owner: String,
    #[serde(default)]
    bot_id: Option<i64>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    external_url: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
    public_key: String,
    #[serde(default)]
    private_key: Option<String>,
    #[serde(default)]
    webhook: Option<WebhookSeed>,
    #[serde(default)]
    permissions: BTreeMap<String, Access>,
    #[serde(default)]
    events: Vec<String>,
    #[serde(default)]
    device_flow: bool,
}

pub(crate) fn app_kind(ctx: &mut SeedCtx<'_>, v: serde_json::Value) -> Result<(), ApiError> {
    let s: AppSeed = decode(v)?;
    if s.slug.is_empty() {
        return Err(ApiError::unprocessable("slug is empty"));
    }
    let owner_id = account(&ctx.tx, &s.owner)?.id;
    let id = id_or_next(ctx, s.id);
    let bot_id = id_or_next(ctx, s.bot_id);
    let now = ctx.clock.now();
    let mut bot = blank_account(
        bot_id,
        node_id::BOT,
        format!("{}[bot]", s.slug),
        AccountKind::Bot,
        now,
    );
    bot.app_id = Some(id);
    ctx.tx.table_mut::<Accounts>().insert(bot)?;
    ctx.tx.table_mut::<Apps>().insert(App {
        id,
        node_id: node_id::entity(node_id::APP, id),
        name: s.name.unwrap_or_else(|| s.slug.clone()),
        external_url: s
            .external_url
            .unwrap_or_else(|| format!("https://github.com/apps/{}", s.slug)),
        slug: s.slug,
        owner_id,
        bot_id,
        description: s.description,
        client_id: s.client_id.unwrap_or_else(|| format!("Iv23{id:016x}")),
        client_secret: s.client_secret.unwrap_or_else(|| random_hex(40)),
        public_key_pem: s.public_key,
        private_key_pem: s.private_key,
        webhook: s.webhook.map(|w| Webhook {
            url: w.url,
            secret: w.secret,
            active: w.active,
        }),
        permissions: s.permissions,
        events: s.events,
        device_flow: s.device_flow,
        created_at: now,
        updated_at: now,
    })?;
    hubdouble_hooks::ping(&mut ctx.tx, id);
    Ok(())
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AppRef {
    Id(i64),
    Slug(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallationSeed {
    #[serde(default)]
    id: Option<i64>,
    app: AppRef,
    account: String,
    #[serde(default)]
    repository_selection: Option<String>,
    #[serde(default)]
    repositories: Vec<String>,
    #[serde(default)]
    permissions: Option<BTreeMap<String, Access>>,
    #[serde(default)]
    events: Option<Vec<String>>,
    #[serde(default)]
    suspended: bool,
}

pub(crate) fn installation_kind(
    ctx: &mut SeedCtx<'_>,
    v: serde_json::Value,
) -> Result<(), ApiError> {
    let s: InstallationSeed = decode(v)?;
    let app = match &s.app {
        AppRef::Id(id) => ctx.tx.table::<Apps>().get(*id),
        AppRef::Slug(slug) => ctx.tx.table::<Apps>().by_slug(slug),
    }
    .cloned()
    .ok_or_else(|| ApiError::unprocessable("no such app"))?;
    let account = account(&ctx.tx, &s.account)?.clone();
    let selection = match s.repository_selection.as_deref() {
        None if s.repositories.is_empty() => RepositorySelection::All,
        None => RepositorySelection::Selected,
        Some(v) => RepositorySelection::from_rest(v).ok_or_else(|| {
            ApiError::unprocessable(format!("repository_selection {v} is not all or selected"))
        })?,
    };
    if selection == RepositorySelection::All && !s.repositories.is_empty() {
        return Err(ApiError::unprocessable(
            "repositories need repository_selection selected",
        ));
    }
    let mut repo_ids = Vec::with_capacity(s.repositories.len());
    for name in &s.repositories {
        let full = if name.contains('/') {
            name.clone()
        } else {
            format!("{}/{name}", account.login)
        };
        let repo = repo_by_full_name(&ctx.tx, &full)?;
        if repo.owner_id != account.id {
            return Err(ApiError::unprocessable(format!(
                "{full} is not owned by {}",
                account.login
            )));
        }
        repo_ids.push(repo.id);
    }
    let permissions = s.permissions.unwrap_or_else(|| app.permissions.clone());
    for (name, access) in &permissions {
        if app
            .permissions
            .get(name)
            .is_none_or(|granted| granted < access)
        {
            return Err(ApiError::unprocessable(format!(
                "permission {name}: {} is beyond the app's grant",
                access.as_str()
            )));
        }
    }
    let id = id_or_next(ctx, s.id);
    let now = ctx.clock.now();
    let installation = ctx.tx.table_mut::<Installations>().insert(Installation {
        id,
        node_id: node_id::entity(node_id::INSTALLATION, id),
        app_id: app.id,
        account_id: account.id,
        repository_selection: selection,
        repo_ids,
        permissions,
        events: s.events.unwrap_or_else(|| app.events.clone()),
        suspended_at: s.suspended.then_some(now),
        suspended_by: None,
        created_at: now,
        updated_at: now,
    })?;
    crate::installation_hook::created(&mut ctx.tx, &installation, &app, &account)
}
