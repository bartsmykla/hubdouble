use hubdouble_model::{Requirement, Scope};

use crate::error::AuthError;

/// Every GitHub App permission name: the properties of the `app-permissions` schema in the pinned
/// `OpenAPI` bundle, plus `actions_variables`, which GitHub grants and names in
/// `X-Accepted-GitHub-Permissions` but leaves out of that schema.
pub const APP_PERMISSIONS: &[&str] = &[
    "actions",
    "actions_variables",
    "administration",
    "artifact_metadata",
    "attestations",
    "checks",
    "code_quality",
    "codespaces",
    "contents",
    "custom_properties_for_organizations",
    "dependabot_secrets",
    "deployments",
    "discussions",
    "email_addresses",
    "enterprise_custom_properties_for_organizations",
    "environments",
    "followers",
    "git_ssh_keys",
    "gpg_keys",
    "interaction_limits",
    "issues",
    "members",
    "merge_queues",
    "metadata",
    "organization_administration",
    "organization_announcement_banners",
    "organization_copilot_agent_settings",
    "organization_copilot_requests",
    "organization_copilot_seat_management",
    "organization_custom_org_roles",
    "organization_custom_properties",
    "organization_custom_roles",
    "organization_events",
    "organization_external_properties_for_repos",
    "organization_hooks",
    "organization_packages",
    "organization_personal_access_token_requests",
    "organization_personal_access_tokens",
    "organization_plan",
    "organization_projects",
    "organization_secrets",
    "organization_self_hosted_runners",
    "organization_user_blocking",
    "packages",
    "pages",
    "profile",
    "pull_requests",
    "repository_custom_properties",
    "repository_hooks",
    "repository_projects",
    "secret_scanning_alerts",
    "secrets",
    "security_events",
    "single_file",
    "starring",
    "statuses",
    "vulnerability_alerts",
    "workflows",
];

/// Checks that a registered operation's requirement can deny access: a repository or org
/// requirement names at least one permission and its path names the target the choke point
/// checks, a public requirement names none, and every permission is a GitHub App permission.
/// The registry walk test calls it for every registered operation.
///
/// # Errors
///
/// [`AuthError::NoGrant`], [`AuthError::NoTarget`], [`AuthError::PublicWithGrant`] or
/// [`AuthError::UnknownPermission`].
pub fn check_requirement(path: &str, req: &Requirement) -> Result<(), AuthError> {
    if let Some(g) = req
        .any_of
        .iter()
        .find(|g| !APP_PERMISSIONS.contains(&g.permission))
    {
        return Err(AuthError::UnknownPermission(g.permission));
    }
    let names_target = match req.scope {
        Scope::Repo => {
            path.contains("{owner}") && path.contains("{repo}") || path.contains("{repository_id}")
        }
        Scope::Org => path.contains("{org}"),
        Scope::Public if !req.any_of.is_empty() => return Err(AuthError::PublicWithGrant),
        Scope::Public | Scope::App | Scope::User | Scope::Installation => return Ok(()),
    };
    if !names_target {
        return Err(AuthError::NoTarget);
    }
    if req.any_of.is_empty() {
        return Err(AuthError::NoGrant);
    }
    Ok(())
}
