use http::header::{ACCEPT, CONTENT_TYPE};
use hubdouble_model::ApiError;
use hubdouble_render::escape;
use hubdouble_rest::{Ctx, Query, Reply};
use serde_json::{Map, Value};

pub(crate) const FORM: &str = "application/x-www-form-urlencoded; charset=utf-8";
pub(crate) const HTML: &str = "text/html; charset=utf-8";

pub(crate) struct Form(Vec<(String, String)>);

impl Form {
    pub(crate) fn read(ctx: &Ctx<'_>) -> Result<Form, ApiError> {
        let mut pairs: Vec<(String, String)> = ctx
            .query()
            .iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let body = ctx.raw_body();
        let json = ctx
            .header(CONTENT_TYPE.as_str())
            .is_some_and(|t| t.contains("json"));
        if json && !body.is_empty() {
            let map: Map<String, Value> = serde_json::from_slice(body)
                .map_err(|_| ApiError::bad_request("Problems parsing JSON"))?;
            for (k, v) in map {
                match v {
                    Value::String(s) => pairs.push((k, s)),
                    Value::Number(n) => pairs.push((k, n.to_string())),
                    Value::Bool(b) => pairs.push((k, b.to_string())),
                    _ => {}
                }
            }
        } else {
            let text = std::str::from_utf8(body)
                .map_err(|_| ApiError::bad_request("the body is not UTF-8"))?;
            pairs.extend(
                Query::parse(text)
                    .iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned())),
            );
        }
        Ok(Form(pairs))
    }

    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }
}

pub(crate) fn wants_json(ctx: &Ctx<'_>) -> bool {
    ctx.header(ACCEPT.as_str())
        .is_some_and(|a| a.contains("application/json"))
}

pub(crate) fn answer(ctx: &Ctx<'_>, fields: &[(&str, Value)]) -> Result<Reply, ApiError> {
    if wants_json(ctx) {
        let map: Map<String, Value> = fields
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect();
        return Reply::json(&map);
    }
    let mut out = String::new();
    for (k, v) in fields {
        if !out.is_empty() {
            out.push('&');
        }
        let text = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        out.push_str(&escape(k));
        out.push('=');
        out.push_str(&escape(&text));
    }
    Ok(Reply::raw(FORM, out))
}

pub(crate) fn error(ctx: &Ctx<'_>, code: &str, extra: &[(&str, Value)]) -> Result<Reply, ApiError> {
    let mut fields = vec![
        ("error", Value::from(code)),
        ("error_description", Value::from(description(code))),
        (
            "error_uri",
            Value::from(format!(
                "https://docs.github.com/apps/managing-oauth-apps/troubleshooting-oauth-app-access-token-request-errors/#{}",
                code.replace('_', "-")
            )),
        ),
    ];
    fields.extend_from_slice(extra);
    answer(ctx, &fields)
}

fn description(code: &str) -> &'static str {
    match code {
        "bad_verification_code" => "The code passed is incorrect or expired.",
        "incorrect_client_credentials" => {
            "The client_id and/or client_secret passed are incorrect."
        }
        "redirect_uri_mismatch" => {
            "The redirect_uri MUST match the registered callback URL for this application."
        }
        "bad_refresh_token" => "The refresh token passed is incorrect or expired.",
        "unverified_user_email" => {
            "The user must have a verified primary email to authorize this application."
        }
        "authorization_pending" => "The authorization request is still pending.",
        "slow_down" => "Too many requests have been made in the same timeframe.",
        "expired_token" => "The device code has expired.",
        "access_denied" => "The authorization request was denied.",
        "incorrect_device_code" => "The device_code provided is not valid.",
        "device_flow_disabled" => "Device Flow must be explicitly enabled for this App",
        _ => "The grant type is not supported.",
    }
}

pub(crate) fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn page(title: &str, body: &str) -> Reply {
    Reply::raw(
        HTML,
        format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title></head><body>{body}</body></html>",
            html_escape(title)
        ),
    )
}

pub(crate) fn hidden(name: &str, value: Option<&str>) -> String {
    value.map_or_else(String::new, |v| {
        format!(
            "<input type=\"hidden\" name=\"{name}\" value=\"{}\">",
            html_escape(v)
        )
    })
}
