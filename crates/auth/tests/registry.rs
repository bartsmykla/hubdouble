use std::collections::HashMap;

use hubdouble_admin::Registrar;
use hubdouble_auth::{AuthError, check_requirement};
use hubdouble_model::{ApiError, Grant, Requirement, Scope};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_server::Registries;

fn walk(r: &Registries) -> Vec<(&'static str, AuthError)> {
    let paths: HashMap<&str, String> = r
        .rest
        .operations()
        .filter(|o| o.registered)
        .map(|o| (o.operation_id, o.path.to_owned()))
        .collect();
    r.rest
        .entries()
        .filter_map(|e| {
            let path = paths.get(e.operation_id).map_or("", String::as_str);
            check_requirement(path, &e.requirement)
                .err()
                .map(|err| (e.operation_id, err))
        })
        .collect()
}

fn handler(_: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    Err(ApiError::not_found())
}

const fn repo(any_of: &'static [Grant]) -> Requirement {
    Requirement {
        any_of,
        all_of: &[],
        scope: Scope::Repo,
        scopes: &[],
    }
}

const NO_GRANT: Requirement = repo(&[]);
const MISSPELT: Requirement = repo(&[Grant::read("content")]);
const NO_TARGET: Requirement = repo(&[Grant::read("metadata")]);
const PUBLIC_WITH_GRANT: Requirement = Requirement {
    any_of: &[Grant::read("metadata")],
    all_of: &[],
    scope: Scope::Public,
    scopes: &[],
};
const CREATE_COMMENT: Requirement = repo(&[Grant::write("issues"), Grant::write("pull_requests")]);

fn with_gaps(r: &mut Registrar<'_>) {
    r.rest.handle("repos/get", NO_GRANT, handler);
    r.rest.handle("repos/list-branches", MISSPELT, handler);
    r.rest.handle("users/get-authenticated", NO_TARGET, handler);
    r.rest.handle("meta/root", PUBLIC_WITH_GRANT, handler);
    r.rest
        .handle("meta/get-zen", Requirement::public(), handler);
    r.rest
        .handle("issues/create-comment", CREATE_COMMENT, handler);
}

#[test]
fn every_operation_register_all_registers_has_a_requirement_that_can_deny() {
    // Given
    let registries = Registries::new(hubdouble_api::register_all);

    // When
    let gaps = walk(&registries);

    // Then
    assert_eq!(gaps, vec![]);
}

#[test]
fn the_walk_fails_on_an_operation_without_a_usable_requirement() {
    // Given operations with no grant, a misspelt permission, no target and a public grant
    let registries = Registries::new(with_gaps);

    // When
    let mut gaps = walk(&registries);
    gaps.sort_by_key(|(op, _)| *op);

    // Then
    assert_eq!(
        gaps,
        vec![
            ("meta/root", AuthError::PublicWithGrant),
            ("repos/get", AuthError::NoGrant),
            (
                "repos/list-branches",
                AuthError::UnknownPermission("content")
            ),
            ("users/get-authenticated", AuthError::NoTarget),
        ]
    );
}
