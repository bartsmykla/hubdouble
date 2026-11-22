use std::sync::Arc;

use http::HeaderMap;
use hubdouble_model::{ApiError, App, Timestamp, TokenKind};
use hubdouble_store::{Accounts, Apps, Installations, Tokens, Tx};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;

use crate::contract::Actor;
use crate::error::AuthError;

/// Seconds an installation token (`ghs_`) lives.
pub const INSTALLATION_TOKEN_TTL: i64 = 3600;
/// Seconds a user token (`ghu_`) lives.
pub const USER_TOKEN_TTL: i64 = 8 * 3600;
/// Seconds a refresh token (`ghr_`) lives, GitHub's `refresh_token_expires_in` of 15 897 600.
pub const REFRESH_TOKEN_TTL: i64 = 184 * 86_400;
/// The most an App JWT's `exp` may lie after the server's now.
pub const JWT_MAX_LIFETIME: i64 = 600;
/// The most an App JWT's `iat` may lie after the server's now.
pub const JWT_IAT_SKEW: i64 = 60;

pub(crate) fn credential(value: &[u8]) -> Option<&str> {
    let value = std::str::from_utf8(value).ok()?.trim();
    let (scheme, rest) = value.split_once(' ')?;
    let rest = rest.trim_start();
    let known = scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("token");
    (known && !rest.is_empty()).then_some(rest)
}

pub(crate) fn authenticate(
    tx: &Tx,
    now: Timestamp,
    headers: &HeaderMap,
) -> Result<Actor, ApiError> {
    let Some(value) = headers.get(http::header::AUTHORIZATION) else {
        return Ok(Actor::Anonymous);
    };
    let credential = credential(value.as_bytes()).ok_or_else(ApiError::unauthorized)?;
    if credential.bytes().filter(|&b| b == b'.').count() == 2 {
        return Ok(Actor::App {
            app: verify_app_jwt(tx, now, credential)?,
        });
    }
    token(tx, now, credential)
}

fn token(tx: &Tx, now: Timestamp, credential: &str) -> Result<Actor, ApiError> {
    let token = tx
        .table::<Tokens>()
        .by_value(credential)
        .ok_or_else(ApiError::unauthorized)?;
    if token.revoked || token.expires_at.is_some_and(|at| at <= now) {
        return Err(ApiError::unauthorized());
    }
    let accounts = tx.table::<Accounts>();
    match token.kind {
        TokenKind::Installation => {
            let installation = token
                .installation_id
                .and_then(|id| tx.table::<Installations>().get(id))
                .ok_or_else(ApiError::unauthorized)?;
            let app = tx
                .table::<Apps>()
                .get(installation.app_id)
                .ok_or_else(ApiError::unauthorized)?;
            let bot = accounts
                .get(app.bot_id)
                .ok_or_else(ApiError::unauthorized)?;
            if installation.suspended_at.is_some() {
                return Err(ApiError::forbidden("This installation has been suspended"));
            }
            Ok(Actor::Installation {
                token: token.clone(),
                installation: installation.clone(),
                app: app.clone(),
                bot: bot.clone(),
            })
        }
        TokenKind::User | TokenKind::Personal => {
            let user = token
                .user_id
                .and_then(|id| accounts.get(id))
                .ok_or_else(ApiError::unauthorized)?;
            Ok(Actor::User {
                token: token.clone(),
                user: user.clone(),
            })
        }
        TokenKind::Refresh => Err(ApiError::unauthorized()),
    }
}

#[derive(Deserialize)]
struct Claims {
    iss: Option<Value>,
    iat: Option<Value>,
    exp: Option<Value>,
}

/// Verifies an App JWT: RS256 with the App's public key, `iss` the App ID (number or string) or
/// its client ID, `exp` in the future and at most [`JWT_MAX_LIFETIME`] after `now`, and `iat`
/// at most [`JWT_IAT_SKEW`] after `now`.
///
/// # Errors
///
/// The `Jwt*` variant of [`AuthError`] that names the broken rule.
pub fn verify_app_jwt(tx: &Tx, now: Timestamp, jwt: &str) -> Result<Arc<App>, AuthError> {
    let header = jsonwebtoken::decode_header(jwt).map_err(|_| AuthError::JwtMalformed)?;
    if header.alg != Algorithm::RS256 {
        return Err(AuthError::JwtAlgorithm);
    }
    let claims: Claims = jsonwebtoken::dangerous::insecure_decode_claims(jwt)
        .map_err(|_| AuthError::JwtMalformed)?;
    let apps = tx.table::<Apps>();
    let app = match claims.iss {
        Some(Value::Number(n)) => n.as_i64().and_then(|id| apps.get(id)),
        Some(Value::String(s)) => match s.parse::<i64>() {
            Ok(id) => apps.get(id),
            Err(_) => apps.by_client_id(&s),
        },
        _ => None,
    }
    .ok_or(AuthError::JwtUnknownIssuer)?;
    let key =
        DecodingKey::from_rsa_pem(app.public_key_pem.as_bytes()).map_err(|_| AuthError::JwtKey)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.validate_exp = false;
    validation.validate_aud = false;
    validation.required_spec_claims.clear();
    jsonwebtoken::decode::<IgnoredAny>(jwt, &key, &validation)
        .map_err(|_| AuthError::JwtSignature)?;
    let now = now.unix();
    let exp = claims
        .exp
        .as_ref()
        .and_then(Value::as_i64)
        .ok_or(AuthError::JwtMissingExp)?;
    let iat = claims
        .iat
        .as_ref()
        .and_then(Value::as_i64)
        .ok_or(AuthError::JwtMissingIat)?;
    if exp <= now {
        return Err(AuthError::JwtExpired);
    }
    if exp > now + JWT_MAX_LIFETIME {
        return Err(AuthError::JwtExpTooFar);
    }
    if iat > now + JWT_IAT_SKEW {
        return Err(AuthError::JwtIatInFuture);
    }
    Ok(app.clone())
}
