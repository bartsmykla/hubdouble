use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use http::Method;
use http::header::{COOKIE, SET_COOKIE};
use hubdouble_auth::{REFRESH_TOKEN_TTL, USER_TOKEN_TTL};
use hubdouble_model::{ApiError, App, Requirement, Timestamp, Token, TokenKind};
use hubdouble_render::escape;
use hubdouble_rest::{Ctx, Registry, Reply};
use hubdouble_store::{Accounts, Apps, Tokens, TxMut};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::params::{self, Form};
use crate::state::{
    self, Decision, DeviceCode, DeviceCodes, Issued, LOGIN_COOKIE, SLOW_DOWN_STEP, SpentCode,
    SpentCodes, device_key,
};

pub(crate) const CODE_TTL: i64 = 600;
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

pub(crate) fn register(registry: &mut Registry) {
    registry.extra(
        Method::GET,
        "/login/oauth/authorize",
        "oauth/authorize",
        Requirement::public(),
        authorize,
    );
    registry.extra(
        Method::POST,
        "/login/oauth/access_token",
        "oauth/access-token",
        Requirement::public(),
        access_token,
    );
}

#[derive(Serialize, Deserialize)]
struct CodeClaims {
    a: i64,
    u: i64,
    r: String,
    c: Option<String>,
    e: i64,
    n: String,
}

fn mac(app: &App) -> Result<Hmac<Sha256>, ApiError> {
    Hmac::new_from_slice(app.client_secret.as_bytes())
        .map_err(|e| ApiError::internal(e.to_string()))
}

fn seal(app: &App, claims: &CodeClaims) -> Result<String, ApiError> {
    let json = serde_json::to_vec(claims).map_err(|e| ApiError::internal(e.to_string()))?;
    let payload = URL_SAFE_NO_PAD.encode(json);
    let mut mac = mac(app)?;
    mac.update(payload.as_bytes());
    let tag = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Ok(format!("{payload}.{tag}"))
}

fn open(app: &App, code: &str) -> Option<CodeClaims> {
    let (payload, tag) = code.split_once('.')?;
    let tag = URL_SAFE_NO_PAD.decode(tag).ok()?;
    let mut mac = mac(app).ok()?;
    mac.update(payload.as_bytes());
    mac.verify_slice(&tag).ok()?;
    let claims: CodeClaims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    (claims.a == app.id).then_some(claims)
}

fn pkce(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub(crate) fn login_cookie(login: &str) -> String {
    format!(
        "{LOGIN_COOKIE}={}; Path=/; HttpOnly; SameSite=Lax",
        escape(login)
    )
}

fn authorize(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let query = ctx.query();
    let app = query
        .get("client_id")
        .and_then(|id| ctx.tx().table::<Apps>().by_client_id(id))
        .ok_or_else(ApiError::not_found)?;
    let redirect = query
        .get("redirect_uri")
        .filter(|r| r.contains("://"))
        .ok_or_else(|| ApiError::unprocessable("redirect_uri is missing or not absolute"))?;
    let challenge = query.get("code_challenge").filter(|c| !c.is_empty());
    if challenge.is_some() && query.get("code_challenge_method") != Some("S256") {
        return Err(ApiError::unprocessable(
            "code_challenge_method must be S256",
        ));
    }
    let login = query.get("login").filter(|l| !l.is_empty());
    let user = state::signed_in(ctx.tx(), login, ctx.header(COOKIE.as_str()))?;
    let code = seal(
        app,
        &CodeClaims {
            a: app.id,
            u: user.id,
            r: redirect.to_owned(),
            c: challenge.map(str::to_owned),
            e: ctx.now().plus_seconds(CODE_TTL).unix(),
            n: state::random_hex(10)?,
        },
    )?;
    let mut location = redirect.to_owned();
    location.push(if redirect.contains('?') { '&' } else { '?' });
    location.push_str("code=");
    location.push_str(&escape(&code));
    if let Some(state) = query.get("state") {
        location.push_str("&state=");
        location.push_str(&escape(state));
    }
    let reply = Reply::redirect(&location)?;
    match login {
        Some(_) => reply.header(SET_COOKIE, &login_cookie(&user.login)),
        None => Ok(reply),
    }
}

fn access_token(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let form = Form::read(ctx)?;
    let Some(app) = form
        .get("client_id")
        .and_then(|id| ctx.tx().table::<Apps>().by_client_id(id))
        .cloned()
    else {
        return params::error(ctx, "incorrect_client_credentials", &[]);
    };
    match form.get("grant_type") {
        Some(DEVICE_GRANT) => device_grant(ctx, &app, &form),
        Some("refresh_token") => refresh_grant(ctx, &app, &form),
        None | Some("authorization_code") => code_grant(ctx, &app, &form),
        Some(_) => params::error(ctx, "unsupported_grant_type", &[]),
    }
}

fn secret_matches(app: &App, form: &Form) -> bool {
    form.get("client_secret").is_some_and(|given| {
        hubdouble_auth::equal_in_constant_time(given.as_bytes(), app.client_secret.as_bytes())
    })
}

fn tokens(ctx: &Ctx<'_>, issued: &Issued) -> Result<Reply, ApiError> {
    params::answer(
        ctx,
        &[
            ("access_token", Value::from(issued.access.as_str())),
            ("expires_in", Value::from(USER_TOKEN_TTL)),
            ("refresh_token", Value::from(issued.refresh.as_str())),
            ("refresh_token_expires_in", Value::from(REFRESH_TOKEN_TTL)),
            ("scope", Value::from("")),
            ("token_type", Value::from("bearer")),
        ],
    )
}

fn verified(ctx: &Ctx<'_>, user_id: i64) -> bool {
    ctx.tx()
        .table::<Accounts>()
        .get(user_id)
        .is_some_and(|u| u.verified_email)
}

fn code_grant(ctx: &mut Ctx<'_>, app: &App, form: &Form) -> Result<Reply, ApiError> {
    if !secret_matches(app, form) {
        return params::error(ctx, "incorrect_client_credentials", &[]);
    }
    let now = ctx.now();
    let claims = form
        .get("code")
        .and_then(|code| open(app, code))
        .filter(|c| c.e > now.unix())
        .filter(|c| ctx.tx().table::<SpentCodes>().by_key(&c.n).is_none());
    let Some(claims) = claims else {
        return params::error(ctx, "bad_verification_code", &[]);
    };
    if form.get("redirect_uri").is_some_and(|r| r != claims.r) {
        return params::error(ctx, "redirect_uri_mismatch", &[]);
    }
    if let Some(challenge) = &claims.c
        && form.get("code_verifier").map(pkce).as_ref() != Some(challenge)
    {
        return params::error(ctx, "bad_verification_code", &[]);
    }
    if !verified(ctx, claims.u) {
        return params::error(ctx, "unverified_user_email", &[]);
    }
    let tx = ctx.tx_mut()?;
    let id = tx.next_id();
    tx.table_mut::<SpentCodes>().insert(SpentCode {
        id,
        nonce: claims.n,
    })?;
    let issued = state::issue(tx, app.id, claims.u, now)?;
    tokens(ctx, &issued)
}

fn device_grant(ctx: &mut Ctx<'_>, app: &App, form: &Form) -> Result<Reply, ApiError> {
    let row = form
        .get("device_code")
        .and_then(|code| ctx.tx().table::<DeviceCodes>().by_key(&device_key(code)))
        .filter(|row| row.app_id == app.id)
        .cloned();
    let Some(row) = row else {
        return params::error(ctx, "incorrect_device_code", &[]);
    };
    if !app.device_flow {
        return params::error(ctx, "device_flow_disabled", &[]);
    }
    let now = ctx.now();
    if row.expires_at <= now {
        return params::error(ctx, "expired_token", &[]);
    }
    match row.decision {
        Some(Decision::Denied) => params::error(ctx, "access_denied", &[]),
        Some(Decision::Approved(user)) => {
            if !verified(ctx, user) {
                return params::error(ctx, "unverified_user_email", &[]);
            }
            let tx = ctx.tx_mut()?;
            tx.table_mut::<DeviceCodes>().remove(row.id);
            let issued = state::issue(tx, app.id, user, now)?;
            tokens(ctx, &issued)
        }
        None => poll(ctx, &row, now),
    }
}

fn poll(ctx: &mut Ctx<'_>, row: &DeviceCode, now: Timestamp) -> Result<Reply, ApiError> {
    let early = row
        .last_poll
        .is_some_and(|last| now < last.plus_seconds(row.interval));
    let interval = if early {
        row.interval + SLOW_DOWN_STEP
    } else {
        row.interval
    };
    update(
        ctx.tx_mut()?,
        DeviceCode {
            interval,
            last_poll: Some(now),
            ..row.clone()
        },
    )?;
    if early {
        params::error(ctx, "slow_down", &[("interval", Value::from(interval))])
    } else {
        params::error(ctx, "authorization_pending", &[])
    }
}

fn update(tx: &mut TxMut<'_>, row: DeviceCode) -> Result<(), ApiError> {
    tx.table_mut::<DeviceCodes>().replace(row)?;
    Ok(())
}

fn refresh_grant(ctx: &mut Ctx<'_>, app: &App, form: &Form) -> Result<Reply, ApiError> {
    if !secret_matches(app, form) {
        return params::error(ctx, "incorrect_client_credentials", &[]);
    }
    let now = ctx.now();
    let old = form
        .get("refresh_token")
        .and_then(|t| ctx.tx().table::<Tokens>().by_value(t))
        .filter(|t| {
            t.kind == TokenKind::Refresh
                && t.app_id == Some(app.id)
                && !t.revoked
                && t.expires_at.is_none_or(|at| at > now)
        })
        .cloned();
    let Some((old, user)) = old.and_then(|t| Some((t.clone(), t.user_id?))) else {
        return params::error(ctx, "bad_refresh_token", &[]);
    };
    let tx = ctx.tx_mut()?;
    tx.table_mut::<Tokens>().replace(Token {
        revoked: true,
        ..(*old).clone()
    })?;
    let issued = state::issue(tx, app.id, user, now)?;
    tokens(ctx, &issued)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: i64, secret: &str) -> App {
        App {
            id,
            node_id: String::new(),
            slug: String::new(),
            name: String::new(),
            owner_id: 1,
            bot_id: 1,
            description: None,
            external_url: String::new(),
            client_id: String::new(),
            client_secret: secret.into(),
            public_key_pem: String::new(),
            private_key_pem: None,
            webhook: None,
            permissions: std::collections::BTreeMap::new(),
            events: vec![],
            device_flow: false,
            created_at: Timestamp::SEED,
            updated_at: Timestamp::SEED,
        }
    }

    #[test]
    fn a_code_opens_only_for_the_app_that_sealed_it_and_unchanged() {
        // Given
        let owner = app(1, "secret");
        let claims = CodeClaims {
            a: 1,
            u: 2,
            r: "https://example.com/cb".into(),
            c: None,
            e: 5,
            n: "n".into(),
        };
        let code = seal(&owner, &claims).expect("seal");
        let (payload, tag) = code.split_once('.').expect("dot");
        let forged = seal(&app(1, "other"), &claims).expect("seal");

        // Then
        assert_eq!(open(&owner, &code).map(|c| c.u), Some(2));
        assert!(open(&app(1, "other"), &code).is_none());
        assert!(open(&app(9, "secret"), &code).is_none());
        assert!(open(&owner, &forged).is_none());
        assert!(open(&owner, &format!("{payload}x.{tag}")).is_none());
    }

    #[test]
    fn pkce_matches_the_rfc_7636_example() {
        assert_eq!(
            pkce("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
