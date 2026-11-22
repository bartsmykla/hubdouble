use std::borrow::Cow;
use std::sync::Arc;

use bytes::Bytes;
use http::header::CONTENT_TYPE;
use http::{HeaderValue, Method, Request, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use hubdouble_model::{ApiError, FieldError};
use hubdouble_rest::{Params, Query};
use hubdouble_store::{Counters, Sequence};
use hyper::body::Incoming;
use serde::Serialize;
use serde_json::{Value, json};

use crate::faults::FaultSpec;
use crate::log::Gone;
use crate::{Admin, AdminHandler, AdminRequest, AdminResponse, SeedCtx, Services, seed};

pub(crate) fn respond(status: StatusCode, body: Bytes) -> AdminResponse {
    let json = !body.is_empty();
    let mut response = hyper::Response::new(Full::new(body));
    *response.status_mut() = status;
    if json {
        response.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
    }
    response
}

/// `value` as JSON with `status`.
pub fn json_response<T: Serialize + ?Sized>(status: StatusCode, value: &T) -> AdminResponse {
    match serde_json::to_vec(value) {
        Ok(body) => respond(status, body.into()),
        Err(e) => error_response(&ApiError::internal(e.to_string())),
    }
}

/// An [`ApiError`] as GitHub writes it.
#[must_use]
pub fn error_response(e: &ApiError) -> AdminResponse {
    #[derive(Serialize)]
    struct Body<'a> {
        message: &'a str,
        #[serde(skip_serializing_if = "<[_]>::is_empty")]
        errors: &'a [FieldError],
        documentation_url: &'a str,
        status: String,
    }
    let body = serde_json::to_vec(&Body {
        message: &e.message,
        errors: &e.errors,
        documentation_url: &e.documentation_url,
        status: e.status.to_string(),
    })
    .unwrap_or_default();
    respond(
        StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        body.into(),
    )
}

fn empty() -> AdminResponse {
    respond(StatusCode::NO_CONTENT, Bytes::new())
}

fn gone() -> AdminResponse {
    error_response(&ApiError::new(410, "The cursor is older than the log"))
}

/// Reads a request body as JSON, `{}` when it is empty.
///
/// # Errors
///
/// `400` for a body that is not JSON, `413` for one over `limit`.
pub async fn read_json(request: Request<Incoming>, limit: usize) -> Result<Value, ApiError> {
    let bytes = Limited::new(request.into_body(), limit)
        .collect()
        .await
        .map_err(|_| ApiError::new(413, "Payload Too Large"))?
        .to_bytes();
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes).map_err(|_| ApiError::bad_request("Problems parsing JSON"))
}

fn segments(path: &str) -> Vec<String> {
    path.strip_prefix("/_hubdouble")
        .unwrap_or(path)
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8_lossy()
                .into_owned()
        })
        .collect()
}

fn route_params(template: &str, segs: &[String]) -> Option<Params> {
    let want: Vec<&str> = template
        .strip_prefix("/_hubdouble")
        .unwrap_or(template)
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if want.len() != segs.len() {
        return None;
    }
    let mut pairs = Vec::new();
    for (w, got) in want.iter().zip(segs) {
        match w.strip_prefix('{').and_then(|w| w.strip_suffix('}')) {
            Some(name) => pairs.push((Cow::Owned(name.to_owned()), got.clone())),
            None if w == got => {}
            None => return None,
        }
    }
    Some(Params::new(pairs))
}

impl Admin {
    pub(crate) async fn dispatch(
        &self,
        request: Request<Incoming>,
        services: Arc<Services>,
    ) -> AdminResponse {
        if let Err(e) =
            hubdouble_auth::authenticate_admin(request.headers(), &services.config.admin_token)
        {
            return error_response(&e);
        }
        let segs = segments(request.uri().path());
        let query = Query::parse(request.uri().query().unwrap_or(""));
        let method = request.method().clone();
        let parts: Vec<&str> = segs.iter().map(String::as_str).collect();
        match (&method, parts.as_slice()) {
            (&Method::POST, ["reset"]) => self.reset(&services).await,
            (&Method::POST, ["hooks", "drain"]) => match services.hooks.drain().await {
                Ok(()) => empty(),
                Err(e) => error_response(&e.into()),
            },
            (&Method::GET, ["log"]) => self.log_since(&query),
            (&Method::GET, ["log", "wait"]) => self.log_wait(&query).await,
            (&Method::GET, ["faults"]) => json_response(StatusCode::OK, &self.faults.list()),
            (&Method::DELETE, ["faults"]) => {
                self.faults.clear();
                empty()
            }
            (&Method::POST, ["faults"]) => {
                let limit = services.config.max_body_bytes;
                let added = async {
                    let spec: FaultSpec = serde_json::from_value(read_json(request, limit).await?)
                        .map_err(|e| ApiError::unprocessable(e.to_string()))?;
                    self.faults.add(spec)
                };
                match added.await {
                    Ok(id) => json_response(StatusCode::CREATED, &json!({ "id": id })),
                    Err(e) => error_response(&e),
                }
            }
            (&Method::POST, [kind]) if self.kinds.iter().any(|(k, _)| k == kind) => {
                let name = (*kind).to_owned();
                self.create(&name, request, &services).await
            }
            _ => {
                let found = self
                    .routes
                    .iter()
                    .filter(|(m, _, _)| *m == method)
                    .find_map(|(_, t, h)| Some((route_params(t, &segs)?, h)));
                match found {
                    Some((params, h)) => call(h, request, params, services).await,
                    None => error_response(&ApiError::not_found()),
                }
            }
        }
    }

    async fn reset(&self, services: &Services) -> AdminResponse {
        match self.reset_state(services).await {
            Ok(()) => empty(),
            Err(e) => error_response(&e),
        }
    }

    pub(crate) async fn reset_state(&self, services: &Services) -> Result<(), ApiError> {
        let snapshot = self.snapshot.lock().clone().ok_or_else(|| {
            ApiError::conflict("No seed is loaded, so there is nothing to reset to")
        })?;
        services.hooks.reset(&snapshot).await?;
        self.log.reset();
        Ok(())
    }

    fn log_since(&self, query: &Query) -> AdminResponse {
        let after = match cursor(query) {
            Ok(a) => a,
            Err(e) => return error_response(&e),
        };
        match self.log.since(after) {
            Ok((entries, cursor)) => json_response(
                StatusCode::OK,
                &json!({ "entries": entries, "cursor": cursor }),
            ),
            Err(Gone) => gone(),
        }
    }

    async fn log_wait(&self, query: &Query) -> AdminResponse {
        let after = match cursor(query) {
            Ok(a) => a,
            Err(e) => return error_response(&e),
        };
        let patterns: Vec<String> = query.all("match").map(str::to_owned).collect();
        match self.log.wait(after, &patterns).await {
            Ok(entry) => json_response(StatusCode::OK, &entry),
            Err(Gone) => gone(),
        }
    }

    async fn create(
        &self,
        kind: &str,
        request: Request<Incoming>,
        services: &Services,
    ) -> AdminResponse {
        let Some(&(_, apply)) = self.kinds.iter().find(|(k, _)| *k == kind) else {
            return error_response(&ApiError::not_found());
        };
        let body = match read_json(request, services.config.max_body_bytes).await {
            Ok(Value::Object(body)) => Value::Object(body),
            Ok(_) => return error_response(&ApiError::unprocessable("The body is a JSON object")),
            Err(e) => return error_response(&e),
        };
        let mut ids = Vec::new();
        seed::given_ids(&body, &mut ids);
        let result = services.store.update(|tx| {
            let current = tx.table::<Counters>().current(Sequence::Id);
            if let Some(&id) = ids.iter().find(|id| **id <= current) {
                return Err(
                    ApiError::unprocessable("Validation Failed").with_error(FieldError {
                        resource: kind.to_owned(),
                        field: "id".to_owned(),
                        code: "invalid".to_owned(),
                        message: Some(format!("{id} is taken or at or below the ID sequence")),
                        value: None,
                    }),
                );
            }
            if let Some(&max) = ids.iter().max() {
                tx.table_mut::<Counters>().bump(Sequence::Id, max);
            }
            apply(
                &mut SeedCtx {
                    tx: tx.reborrow(),
                    git: &services.git,
                    clock: services.clock.as_ref(),
                    env: &|_| None,
                },
                body,
            )
        });
        match result {
            Ok(()) => json_response(StatusCode::CREATED, &json!({})),
            Err(e) => error_response(&e),
        }
    }
}

fn cursor(query: &Query) -> Result<Option<u64>, ApiError> {
    query
        .get("after")
        .map(|a| {
            a.parse::<u64>()
                .map_err(|_| ApiError::unprocessable("after is not a log cursor"))
        })
        .transpose()
}

async fn call(
    h: &AdminHandler,
    request: Request<Incoming>,
    params: Params,
    services: Arc<Services>,
) -> AdminResponse {
    h(AdminRequest {
        request,
        params,
        services,
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_match_segments_and_decode_params() {
        let segs = segments("/_hubdouble/user/a%5Bbot%5D");
        assert_eq!(segs, ["user", "a[bot]"]);
        let p = route_params("/user/{login}", &segs).unwrap();
        assert_eq!(p.get("login"), Some("a[bot]"));
        assert!(route_params("/_hubdouble/user/{login}", &segs).is_some());
        assert!(route_params("/app/{slug}", &segs).is_none());
        assert!(route_params("/user/{login}/tokens", &segs).is_none());
    }
}
