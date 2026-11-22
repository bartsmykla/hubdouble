use std::sync::Arc;

use hubdouble_admin::{Registrar, SeedCtx};
use hubdouble_auth::{Actor, repo_role};
use hubdouble_model::{
    AccountKind, ApiError, BypassActor, Enforcement, MembershipState, OrgRole, Repo, RepoRole,
    Rule, Ruleset, RulesetSource, RulesetTarget, node_id,
};
use hubdouble_render as render;
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Accounts, Memberships, Repos, Rulesets, Teams, Tx};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::METADATA;

pub(crate) fn register(r: &mut Registrar<'_>) {
    r.rest
        .handle("repos/get-repo-ruleset", METADATA, get_ruleset);
    r.rest
        .handle("repos/get-branch-rules", METADATA, branch_rules);
    r.admin.kind("ruleset", ruleset_kind);
}

fn source_name(tx: &Tx, r: &Ruleset) -> Result<String, ApiError> {
    let accounts = tx.table::<Accounts>();
    let missing = || ApiError::internal("the ruleset's source is missing");
    match r.source_type {
        RulesetSource::Organization => {
            Ok(accounts.get(r.source_id).ok_or_else(missing)?.login.clone())
        }
        RulesetSource::Repository => {
            let repo = tx.table::<Repos>().get(r.source_id).ok_or_else(missing)?;
            crate::full_name(tx, repo)
        }
    }
}

const ROLE_IDS: [(i64, RepoRole); 5] = [
    (1, RepoRole::Read),
    (2, RepoRole::Maintain),
    (3, RepoRole::Triage),
    (4, RepoRole::Write),
    (5, RepoRole::Admin),
];

fn bypasses(tx: &Tx, actor: &Actor, repo: &Repo, b: &BypassActor) -> bool {
    match (b.actor_type.as_str(), actor) {
        ("Integration", Actor::Installation { app, .. } | Actor::App { app }) => {
            b.actor_id.is_none_or(|id| id == app.id)
        }
        ("OrganizationAdmin", Actor::User { user, .. }) => tx
            .table::<Memberships>()
            .of(repo.owner_id, user.id)
            .is_some_and(|m| m.role == OrgRole::Admin && m.state == MembershipState::Active),
        ("RepositoryRole", Actor::User { user, .. }) => {
            let wanted = ROLE_IDS
                .iter()
                .find(|(id, _)| Some(*id) == b.actor_id)
                .map(|(_, role)| *role);
            wanted.is_some_and(|w| repo_role(tx, user.id, repo) >= Some(w))
        }
        ("Team", Actor::User { user, .. }) => b
            .actor_id
            .and_then(|id| tx.table::<Teams>().get(id))
            .is_some_and(|t| t.members.iter().any(|m| m.user_id == user.id)),
        _ => false,
    }
}

/// GitHub's `current_user_can_bypass`: the widest bypass any of the actor's matches grants.
fn can_bypass(tx: &Tx, actor: &Actor, repo: &Repo, r: &Ruleset) -> &'static str {
    let modes: Vec<&str> = r
        .bypass_actors
        .iter()
        .filter(|b| bypasses(tx, actor, repo, b))
        .map(|b| b.bypass_mode.as_str())
        .collect();
    if modes.contains(&"always") {
        "always"
    } else if modes.contains(&"exempt") {
        "exempt"
    } else if modes.contains(&"pull_request") {
        "pull_requests_only"
    } else {
        "never"
    }
}

fn get_ruleset(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    let tx = ctx.tx();
    let id = ctx.param_i64("ruleset_id")?;
    let parents = ctx.query().get("includes_parents") != Some("false");
    let r = tx
        .table::<Rulesets>()
        .get(id)
        .filter(|r| match r.source_type {
            RulesetSource::Repository => r.repo_id == Some(repo.id),
            RulesetSource::Organization => parents && r.governs(repo),
        })
        .ok_or_else(ApiError::not_found)?;
    let mut out = render::Ruleset::new(ctx.urls(), r, &source_name(tx, r)?);
    out.current_user_can_bypass = Some(can_bypass(tx, ctx.actor(), repo, r));
    Reply::json(&out)
}

/// GitHub's `repository-rule-detailed`.
#[derive(Serialize)]
struct DetailedRule<'a> {
    #[serde(rename = "type")]
    rule_type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<&'a Value>,
    ruleset_source_type: &'static str,
    ruleset_source: Arc<str>,
    ruleset_id: i64,
}

fn branch_rules(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    let tx = ctx.tx();
    let branch = ctx.param("branch")?;
    let mut rules: Vec<(&Rule, &Ruleset, Arc<str>)> = Vec::new();
    for r in tx
        .table::<Rulesets>()
        .iter()
        .filter(|r| r.applies_to_branch(repo, branch))
    {
        let source: Arc<str> = source_name(tx, r)?.into();
        rules.extend(
            r.rules
                .iter()
                .map(|rule| (rule, r.as_ref(), source.clone())),
        );
    }
    ctx.page(&rules, |(rule, r, source)| DetailedRule {
        rule_type: &rule.rule_type,
        parameters: rule.parameters.as_ref(),
        ruleset_source_type: r.source_type.as_str(),
        ruleset_source: source.clone(),
        ruleset_id: r.id,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BypassSeed {
    #[serde(default)]
    actor_id: Option<i64>,
    actor_type: String,
    #[serde(default = "always")]
    bypass_mode: String,
}

fn always() -> String {
    "always".to_owned()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleSeed {
    #[serde(rename = "type")]
    rule_type: String,
    #[serde(default)]
    parameters: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesetSeed {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    org: Option<String>,
    name: String,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    enforcement: Option<String>,
    #[serde(default)]
    bypass_actors: Vec<BypassSeed>,
    #[serde(default)]
    conditions: Option<Value>,
    #[serde(default)]
    rules: Vec<RuleSeed>,
}

const BYPASS_TYPES: [&str; 5] = [
    "Integration",
    "OrganizationAdmin",
    "RepositoryRole",
    "Team",
    "DeployKey",
];
const BYPASS_MODES: [&str; 3] = ["always", "pull_request", "exempt"];

fn ruleset_kind(ctx: &mut SeedCtx<'_>, v: Value) -> Result<(), ApiError> {
    let s: RulesetSeed = crate::admin::decode(v)?;
    let (source_type, source_id, repo_id) = match (&s.repository, &s.org) {
        (Some(full), None) => {
            let repo = crate::admin::repo_by_full_name(&ctx.tx, full)?;
            (RulesetSource::Repository, repo.id, Some(repo.id))
        }
        (None, Some(login)) => {
            let org = ctx
                .tx
                .table::<Accounts>()
                .by_login(login)
                .filter(|a| a.kind == AccountKind::Organization)
                .ok_or_else(|| ApiError::unprocessable(format!("no organization {login}")))?;
            (RulesetSource::Organization, org.id, None)
        }
        _ => {
            return Err(ApiError::unprocessable(
                "a ruleset names exactly one of repository and org",
            ));
        }
    };
    let target = match s.target.as_deref() {
        None => RulesetTarget::Branch,
        Some(t) => RulesetTarget::from_rest(t).ok_or_else(|| {
            ApiError::unprocessable(format!("target {t} is not branch, tag or push"))
        })?,
    };
    let enforcement = match s.enforcement.as_deref() {
        None => Enforcement::Active,
        Some(e) => Enforcement::from_rest(e).ok_or_else(|| {
            ApiError::unprocessable(format!(
                "enforcement {e} is not disabled, active or evaluate"
            ))
        })?,
    };
    for b in &s.bypass_actors {
        if !BYPASS_TYPES.contains(&b.actor_type.as_str())
            || !BYPASS_MODES.contains(&b.bypass_mode.as_str())
        {
            return Err(ApiError::unprocessable(format!(
                "bypass actor {} {} is not valid",
                b.actor_type, b.bypass_mode
            )));
        }
    }
    let id = s.id.unwrap_or_else(|| ctx.tx.next_id());
    let now = ctx.clock.now();
    ctx.tx.table_mut::<Rulesets>().insert(Ruleset {
        id,
        node_id: node_id::entity(node_id::RULESET, id),
        source_type,
        source_id,
        repo_id,
        name: s.name,
        target,
        enforcement,
        bypass_actors: s
            .bypass_actors
            .into_iter()
            .map(|b| BypassActor {
                actor_id: b.actor_id,
                actor_type: b.actor_type,
                bypass_mode: b.bypass_mode,
            })
            .collect(),
        conditions: s.conditions,
        rules: s
            .rules
            .into_iter()
            .map(|r| Rule {
                rule_type: r.rule_type,
                parameters: r.parameters,
            })
            .collect(),
        created_at: now,
        updated_at: now,
    })?;
    Ok(())
}
