use hubdouble_admin::Registrar;

/// Registers the core kinds first, then every API crate in the order below, which is also the
/// order seed kinds apply in.
pub fn register_all(r: &mut Registrar<'_>) {
    hubdouble_admin::register(r);
    hubdouble_api_apps::register(r);
    hubdouble_api_users::register(r);
    hubdouble_api_orgs::register(r);
    hubdouble_api_gitdata::register(r);
    hubdouble_api_pulls::register(r);
    hubdouble_api_reviews::register(r);
    hubdouble_api_issues::register(r);
    hubdouble_api_checks::register(r);
    hubdouble_api_actions::register(r);
    hubdouble_api_repos::register(r);
    hubdouble_api_search::register(r);
    hubdouble_api_deployments::register(r);
    hubdouble_api_activity::register(r);
    hubdouble_api_meta::register(r);
    hubdouble_api_security::register(r);
    hubdouble_api_packages::register(r);
    hubdouble_api_projects::register(r);
    hubdouble_api_gists::register(r);
    hubdouble_api_migrations::register(r);
    hubdouble_api_codespaces::register(r);
    hubdouble_api_copilot::register(r);
    hubdouble_api_billing::register(r);
}
