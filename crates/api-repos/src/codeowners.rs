use hubdouble_admin::Registrar;
use hubdouble_api_gitdata::codeowners::{OwnersError, RepoOwners, check, file_at};
use hubdouble_model::ApiError;
use hubdouble_rest::{Ctx, Reply};
use serde::Serialize;

use crate::CONTENTS;

pub(crate) fn register(r: &mut Registrar<'_>) {
    r.rest.handle("repos/codeowners-errors", CONTENTS, errors);
}

#[derive(Serialize)]
struct Errors {
    errors: Vec<OwnersError>,
}

fn errors(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    let tx = ctx.tx();
    let reference = ctx.query().get("ref").unwrap_or(&repo.default_branch);
    let Some((path, text)) = file_at(tx, ctx.git(), repo.id, reference)? else {
        return Err(ApiError::not_found());
    };
    let errors = check(path, &text, &RepoOwners { tx, repo });
    Reply::json(&Errors { errors })
}
