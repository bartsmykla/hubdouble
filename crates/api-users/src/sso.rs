use http::Method;
use http::header::{COOKIE, SET_COOKIE};
use hubdouble_auth::record_sso;
use hubdouble_model::{AccountKind, ApiError, Requirement};
use hubdouble_rest::{Ctx, Registry, Reply};
use hubdouble_store::Accounts;

use crate::oauth::login_cookie;
use crate::params::{Form, hidden, html_escape, page};
use crate::state;

pub(crate) fn register(registry: &mut Registry) {
    registry.extra(
        Method::GET,
        "/orgs/{org}/sso",
        "oauth/sso-page",
        Requirement::public(),
        sso_page,
    );
    registry.extra(
        Method::POST,
        "/orgs/{org}/sso",
        "oauth/sso",
        Requirement::public(),
        sso,
    );
}

fn org(ctx: &Ctx<'_>) -> Result<(i64, String), ApiError> {
    let org = ctx
        .tx()
        .table::<Accounts>()
        .by_login(ctx.param("org")?)
        .filter(|a| a.kind == AccountKind::Organization)
        .ok_or_else(ApiError::not_found)?;
    Ok((org.id, org.login.clone()))
}

fn sso_page(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let (_, org) = org(ctx)?;
    let query = ctx.query();
    let action = format!(
        "{}/orgs/{}/sso",
        ctx.urls().base,
        hubdouble_render::escape(&org)
    );
    let body = format!(
        "<form id=\"sso\" method=\"post\" action=\"{}\">{}{}\
         <button>Continue</button></form>\
         <script>document.getElementById(\"sso\").submit()</script>",
        html_escape(&action),
        hidden("return_to", query.get("return_to")),
        hidden("login", query.get("login")),
    );
    Ok(page(&format!("Single sign-on to {org}"), &body))
}

fn sso(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let form = Form::read(ctx)?;
    let (org, _) = org(ctx)?;
    let base = ctx.urls().base.clone();
    let target = match form.get("return_to") {
        None => format!("{base}/"),
        Some(path) if path.starts_with('/') && !path.starts_with("//") => format!("{base}{path}"),
        Some(url) if url == base || url.starts_with(&format!("{base}/")) => url.to_owned(),
        Some(_) => {
            return Err(ApiError::unprocessable("return_to must be on this origin"));
        }
    };
    let login = form.get("login");
    let user = state::signed_in(ctx.tx(), login, ctx.header(COOKIE.as_str()))?;
    let (user_id, user_login) = (user.id, user.login.clone());
    record_sso(ctx.tx_mut()?, user_id, org)?;
    let reply = Reply::redirect(&target)?;
    match login {
        Some(_) => reply.header(SET_COOKIE, &login_cookie(&user_login)),
        None => Ok(reply),
    }
}
