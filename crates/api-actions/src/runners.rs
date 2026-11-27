use hubdouble_admin::{Registrar, SeedCtx};
use hubdouble_model::{ApiError, Grant, Requirement, Scope};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Entity, Rows};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::configuration::find_org;

const ORG_RUNNERS: Requirement = Requirement {
    scope: Scope::Org,
    any_of: &[Grant::read("organization_self_hosted_runners")],
    scopes: &["admin:org"],
};

type OrgRunners = Rows<OrgRunner>;

struct OrgRunner {
    org_id: i64,
    runner: Runner,
}

impl Entity for OrgRunner {
    fn id(&self) -> i64 {
        self.runner.id
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Runner {
    id: i64,
    name: String,
    os: String,
    status: String,
    busy: bool,
    labels: Vec<Label>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runner_group_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ephemeral: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Label {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<LabelKind>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum LabelKind {
    ReadOnly,
    Custom,
}

pub(crate) fn register(r: &mut Registrar<'_>) {
    r.store.register_table::<OrgRunners>();
    r.admin.kind("actions_runner", seed);
    r.rest
        .handle("actions/get-self-hosted-runner-for-org", ORG_RUNNERS, get);
}

fn seed(ctx: &mut SeedCtx<'_>, mut body: Value) -> Result<(), ApiError> {
    let org = body
        .as_object_mut()
        .and_then(|fields| fields.remove("organization"))
        .and_then(|name| match name {
            Value::String(name) => Some(name),
            _ => None,
        })
        .ok_or_else(|| ApiError::unprocessable("An Actions runner needs an organization"))?;
    let org_id = find_org(&ctx.tx, &org)?;
    let runner =
        serde_json::from_value(body).map_err(|err| ApiError::unprocessable(err.to_string()))?;
    ctx.tx
        .table_mut::<OrgRunners>()
        .insert(OrgRunner { org_id, runner })?;
    Ok(())
}

fn get(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let org_id = find_org(ctx.tx(), ctx.param("org")?)?;
    let runner = ctx
        .tx()
        .table::<OrgRunners>()
        .get(ctx.param_i64("runner_id")?)
        .filter(|entry| entry.org_id == org_id)
        .ok_or_else(ApiError::not_found)?;
    Reply::json(&runner.runner)
}
