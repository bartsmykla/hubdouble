use hubdouble_admin::Registrar;

/// Registers P0-F's kinds first, then every track in the order of plan section 5, which is also
/// the order seed kinds apply in.
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
}
