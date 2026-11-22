use http::HeaderMap;
use hubdouble_model::ApiError;

use crate::credential::credential;

/// Checks the admin API's `Authorization: Bearer <HUBDOUBLE_ADMIN_TOKEN>`.
///
/// # Errors
///
/// `401` when the header is missing or holds another value.
pub fn authenticate_admin(headers: &HeaderMap, admin_token: &str) -> Result<(), ApiError> {
    let given = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| credential(v.as_bytes()))
        .ok_or_else(ApiError::unauthorized)?;
    if equal_in_constant_time(given.as_bytes(), admin_token.as_bytes()) {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}

fn equal_in_constant_time(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
