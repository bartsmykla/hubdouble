mod admin;
mod configuration;
mod lifecycle;
mod oidc;
mod runs;
mod views;
mod workflows;

use http::header::LINK;
use hubdouble_admin::Registrar;
use hubdouble_model::{ApiError, FieldError, Grant, Requirement, Scope};
use hubdouble_rest::{Ctx, Page, Reply};
use serde::Serialize;
use serde::ser::SerializeMap as _;

const ACTIONS_READ: Requirement = repository(&[Grant::read("actions")]);
const ACTIONS_WRITE: Requirement = repository(&[Grant::write("actions")]);

const fn repository(any_of: &'static [Grant]) -> Requirement {
    Requirement {
        scope: Scope::Repo,
        any_of,
        scopes: &["repo", "public_repo"],
    }
}

pub fn register(r: &mut Registrar<'_>) {
    r.store
        .register_table::<workflows::Workflows>()
        .register_table::<runs::RunNumbers>()
        .register_table::<runs::RunContexts>()
        .register_table::<runs::Attempts>()
        .register_table::<configuration::Settings>()
        .register_table::<configuration::Entries>()
        .register_table::<oidc::SigningKey>();
    r.git.on_ref_update(workflows::ref_updated);
    workflows::register(r.rest);
    runs::register(r.rest);
    configuration::register(r);
    oidc::register(r);
    admin::register(r.admin);
}

fn invalid(resource: &str, field: &str, message: impl Into<String>) -> ApiError {
    ApiError::unprocessable(message).with_error(FieldError {
        resource: resource.to_owned(),
        field: field.to_owned(),
        code: "invalid".to_owned(),
        message: None,
        value: None,
    })
}

fn page<T: Serialize>(
    ctx: &Ctx<'_>,
    key: &str,
    count: usize,
    values: &[T],
) -> Result<Reply, ApiError> {
    let body = Collection { key, count, values };
    let reply = Reply::json(&body)?;
    let Page { page, per_page } = ctx.page_params();
    match ctx.link_header(page, count.div_ceil(per_page).max(1)) {
        Some(link) => reply.header(LINK, &link),
        None => Ok(reply),
    }
}

struct Collection<'a, T> {
    key: &'a str,
    count: usize,
    values: &'a [T],
}

impl<T: Serialize> Serialize for Collection<'_, T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("total_count", &self.count)?;
        map.serialize_entry(self.key, self.values)?;
        map.end()
    }
}

fn window(ctx: &Ctx<'_>) -> (usize, usize) {
    let Page { page, per_page } = ctx.page_params();
    ((page - 1).saturating_mul(per_page), per_page)
}
