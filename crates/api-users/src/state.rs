use std::fmt::Write as _;
use std::sync::Arc;

use hubdouble_auth::{REFRESH_TOKEN_TTL, USER_TOKEN_TTL};
use hubdouble_model::{Account, AccountKind, ApiError, Timestamp, Token, TokenKind};
use hubdouble_store::{Accounts, Counters, Deleted, Entity, Rows, StoreBuilder, Tokens, Tx, TxMut};
use rand::{RngCore, rngs::OsRng};

pub(crate) const DEVICE_CODE_TTL: i64 = 900;
pub(crate) const DEVICE_INTERVAL: i64 = 5;
pub(crate) const SLOW_DOWN_STEP: i64 = 5;
pub(crate) const LOGIN_COOKIE: &str = "hubdouble_login";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Approved(i64),
    Denied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceCode {
    pub(crate) id: i64,
    pub(crate) app_id: i64,
    pub(crate) secret: String,
    pub(crate) user_code: String,
    pub(crate) expires_at: Timestamp,
    pub(crate) interval: i64,
    pub(crate) last_poll: Option<Timestamp>,
    pub(crate) decision: Option<Decision>,
}

impl Entity for DeviceCode {
    fn id(&self) -> i64 {
        self.id
    }
    fn keys(&self) -> Vec<String> {
        vec![device_key(&self.secret), user_key(&self.user_code)]
    }
}

pub(crate) type DeviceCodes = Rows<DeviceCode>;

pub(crate) fn device_key(device_code: &str) -> String {
    format!("d:{device_code}")
}

pub(crate) fn user_key(user_code: &str) -> String {
    format!("u:{}", user_code.trim().to_ascii_uppercase())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpentCode {
    pub(crate) id: i64,
    pub(crate) nonce: String,
}

impl Entity for SpentCode {
    fn id(&self) -> i64 {
        self.id
    }
    fn keys(&self) -> Vec<String> {
        vec![self.nonce.clone()]
    }
    fn keep_on_reset(&self) -> bool {
        true
    }
}

pub(crate) type SpentCodes = Rows<SpentCode>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DefaultUser {
    pub(crate) id: i64,
    pub(crate) user_id: i64,
}

const DEFAULT_KEY: &str = "default";

impl Entity for DefaultUser {
    fn id(&self) -> i64 {
        self.id
    }
    fn keys(&self) -> Vec<String> {
        vec![DEFAULT_KEY.to_owned()]
    }
}

pub(crate) type DefaultUsers = Rows<DefaultUser>;

pub(crate) fn register_tables(b: &mut StoreBuilder) {
    b.register_table::<DeviceCodes>();
    b.register_table::<SpentCodes>();
    b.register_table::<DefaultUsers>().on_delete(forget);
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "a deletion subscriber returns a Result"
)]
fn forget(tx: &mut TxMut<'_>, deleted: &Deleted) -> Result<(), ApiError> {
    if let Deleted::Account(account) = deleted {
        let id = account.id;
        tx.table_mut::<DefaultUsers>()
            .remove_where(|d| d.user_id == id);
        tx.table_mut::<DeviceCodes>()
            .remove_where(|c| c.decision == Some(Decision::Approved(id)));
    }
    Ok(())
}

pub(crate) fn set_default_user(tx: &mut TxMut<'_>, user_id: i64) -> Result<(), ApiError> {
    let id = match tx.table::<DefaultUsers>().by_key(DEFAULT_KEY) {
        Some(row) => row.id,
        None => tx.next_id(),
    };
    tx.table_mut::<DefaultUsers>()
        .replace(DefaultUser { id, user_id })?;
    Ok(())
}

pub(crate) fn person<'t>(tx: &'t Tx, login: &str) -> Result<&'t Arc<Account>, ApiError> {
    tx.table::<Accounts>()
        .by_login(login)
        .filter(|a| a.kind == AccountKind::User)
        .ok_or_else(|| ApiError::unprocessable(format!("no user {login}")))
}

pub(crate) fn signed_in<'t>(
    tx: &'t Tx,
    login: Option<&str>,
    cookie: Option<&str>,
) -> Result<&'t Arc<Account>, ApiError> {
    if let Some(login) = login.or_else(|| cookie.and_then(login_cookie)) {
        return person(tx, login);
    }
    tx.table::<DefaultUsers>()
        .by_key(DEFAULT_KEY)
        .and_then(|d| tx.table::<Accounts>().get(d.user_id))
        .ok_or_else(|| ApiError::unprocessable("no signed-in user and no default OAuth user"))
}

fn login_cookie(header: &str) -> Option<&str> {
    header
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == LOGIN_COOKIE)
        .map(|(_, value)| value)
        .filter(|value| !value.is_empty())
}

pub(crate) fn random_hex(bytes: usize) -> Result<String, ApiError> {
    let mut buf = vec![0_u8; bytes];
    OsRng
        .try_fill_bytes(&mut buf)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let mut out = String::with_capacity(bytes * 2);
    for byte in buf {
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

pub(crate) fn user_code() -> Result<String, ApiError> {
    const LETTERS: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";
    let mut buf = [0_u8; 8];
    OsRng
        .try_fill_bytes(&mut buf)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let mut out = String::with_capacity(9);
    for (i, byte) in buf.iter().enumerate() {
        if i == 4 {
            out.push('-');
        }
        out.push(char::from(LETTERS[usize::from(*byte) % LETTERS.len()]));
    }
    Ok(out)
}

pub(crate) struct Issued {
    pub(crate) access: String,
    pub(crate) refresh: String,
}

pub(crate) fn issue(
    tx: &mut TxMut<'_>,
    app_id: i64,
    user_id: i64,
    now: Timestamp,
) -> Result<Issued, ApiError> {
    let owner_seeded = tx.table::<Counters>().is_seeded(user_id);
    let issued = Issued {
        access: format!("ghu_{}", random_hex(18)?),
        refresh: format!("ghr_{}", random_hex(18)?),
    };
    for (kind, value, ttl) in [
        (TokenKind::User, &issued.access, USER_TOKEN_TTL),
        (TokenKind::Refresh, &issued.refresh, REFRESH_TOKEN_TTL),
    ] {
        let id = tx.next_id();
        tx.table_mut::<Tokens>().insert(Token {
            id,
            token: value.clone(),
            kind,
            app_id: Some(app_id),
            installation_id: None,
            user_id: Some(user_id),
            repo_ids: None,
            permissions: std::collections::BTreeMap::new(),
            scopes: Vec::new(),
            expires_at: Some(now.plus_seconds(ttl)),
            revoked: false,
            owner_seeded,
            created_at: now,
        })?;
    }
    Ok(issued)
}

pub(crate) fn decide(
    tx: &mut TxMut<'_>,
    user_code: &str,
    decision: Decision,
    now: Timestamp,
) -> Result<(), ApiError> {
    let row = tx
        .table::<DeviceCodes>()
        .by_key(&user_key(user_code))
        .ok_or_else(ApiError::not_found)?;
    if row.expires_at <= now {
        return Err(ApiError::unprocessable("the device code has expired"));
    }
    if row.decision.is_some() {
        return Err(ApiError::unprocessable(
            "the device code is already answered",
        ));
    }
    let row = DeviceCode {
        decision: Some(decision),
        ..(**row).clone()
    };
    tx.table_mut::<DeviceCodes>().replace(row)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_login_cookie_is_found_among_other_cookies() {
        assert_eq!(login_cookie("a=1; hubdouble_login=bob; c=3"), Some("bob"));
        assert_eq!(login_cookie("a=1"), None);
        assert_eq!(login_cookie("hubdouble_login="), None);
    }

    #[test]
    fn user_codes_have_github_shape() {
        let code = user_code().expect("code");
        assert_eq!(code.len(), 9);
        assert_eq!(code.as_bytes()[4], b'-');
        assert!(code.chars().all(|c| c == '-' || c.is_ascii_uppercase()));
        assert_eq!(user_key(&code.to_ascii_lowercase()), user_key(&code));
    }
}
