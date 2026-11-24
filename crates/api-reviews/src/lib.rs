use hubdouble_admin::Registrar;
use hubdouble_model::{ApiError, FieldError, Grant, Requirement, Scope};

mod anchors;
mod comments;
mod events;
mod graphql;
mod reviewers;
mod reviews;
mod seed;
mod stale;
mod view;

const READ: Requirement = Requirement {
    any_of: &[Grant::read("pull_requests")],
    scope: Scope::Repo,
    scopes: &["repo", "public_repo"],
};
const WRITE: Requirement = Requirement {
    any_of: &[Grant::write("pull_requests")],
    ..READ
};

pub fn register(r: &mut Registrar<'_>) {
    reviews::register(r.rest);
    comments::register(r.rest);
    reviewers::register(r.rest);
    graphql::register(r.gql);
    r.git.on_ref_update(anchors::ref_changed);
    r.git.on_ref_update(stale::ref_changed);
    seed::register(r.admin);
}

fn invalid(resource: &str, field: &str, message: &str) -> ApiError {
    ApiError::unprocessable(message).with_error(FieldError {
        resource: resource.to_owned(),
        field: field.to_owned(),
        code: "invalid".to_owned(),
        message: None,
        value: None,
    })
}
