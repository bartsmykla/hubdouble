use http::Method;
use http::header::COOKIE;
use hubdouble_model::{ApiError, Requirement};
use hubdouble_rest::{Ctx, Registry, Reply};
use hubdouble_store::Apps;
use serde_json::Value;

use crate::params::{self, Form, hidden, html_escape, page};
use crate::state::{
    self, DEVICE_CODE_TTL, DEVICE_INTERVAL, Decision, DeviceCode, DeviceCodes, random_hex,
};

pub(crate) fn register(registry: &mut Registry) {
    registry.extra(
        Method::POST,
        "/login/device/code",
        "oauth/device-code",
        Requirement::public(),
        device_code,
    );
    registry.extra(
        Method::GET,
        "/login/device",
        "oauth/device-page",
        Requirement::public(),
        device_page,
    );
    registry.extra(
        Method::POST,
        "/login/device",
        "oauth/device-approve",
        Requirement::public(),
        device_approve,
    );
}

fn device_code(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let form = Form::read(ctx)?;
    let Some(app) = form
        .get("client_id")
        .and_then(|id| ctx.tx().table::<Apps>().by_client_id(id))
        .cloned()
    else {
        return params::error(ctx, "incorrect_client_credentials", &[]);
    };
    if !app.device_flow {
        return params::error(ctx, "device_flow_disabled", &[]);
    }
    let now = ctx.now();
    let device_code = random_hex(20)?;
    let user_code = state::user_code()?;
    let tx = ctx.tx_mut()?;
    let id = tx.next_id();
    tx.table_mut::<DeviceCodes>().insert(DeviceCode {
        id,
        app_id: app.id,
        secret: device_code.clone(),
        user_code: user_code.clone(),
        expires_at: now.plus_seconds(DEVICE_CODE_TTL),
        interval: DEVICE_INTERVAL,
        last_poll: None,
        decision: None,
    })?;
    let verification_uri = format!("{}/login/device", ctx.urls().base);
    params::answer(
        ctx,
        &[
            ("device_code", Value::from(device_code)),
            ("user_code", Value::from(user_code)),
            ("verification_uri", Value::from(verification_uri)),
            ("expires_in", Value::from(DEVICE_CODE_TTL)),
            ("interval", Value::from(DEVICE_INTERVAL)),
        ],
    )
}

#[expect(clippy::unnecessary_wraps, reason = "a REST handler returns a Result")]
fn device_page(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let query = ctx.query();
    let code = query.get("user_code").unwrap_or("");
    let body = format!(
        "<form method=\"post\" action=\"{}/login/device\">\
         <label>Code <input name=\"user_code\" value=\"{}\"></label>{}\
         <button name=\"approve\" value=\"true\">Authorize</button>\
         <button name=\"approve\" value=\"false\">Cancel</button></form>",
        html_escape(&ctx.urls().base),
        html_escape(code),
        hidden("login", query.get("login")),
    );
    Ok(page("Device activation", &body))
}

fn device_approve(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let form = Form::read(ctx)?;
    let code = form
        .get("user_code")
        .ok_or_else(|| ApiError::unprocessable("user_code is missing"))?;
    let decision = if form.get("approve") == Some("false") {
        Decision::Denied
    } else {
        let user = state::signed_in(ctx.tx(), form.get("login"), ctx.header(COOKIE.as_str()))?;
        Decision::Approved(user.id)
    };
    let now = ctx.now();
    state::decide(ctx.tx_mut()?, code, decision, now)?;
    let text = match decision {
        Decision::Approved(_) => "Your device is now connected.",
        Decision::Denied => "The device was not authorized.",
    };
    Ok(page("Device activation", text))
}
