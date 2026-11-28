use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use http::{StatusCode, header::LINK};
use hubdouble_model::{ApiError, Delivery};
use hubdouble_render::{HookDelivery, HookDeliveryItem};
use hubdouble_rest::{Ctx, Registry, Reply};
use hubdouble_store::Deliveries;
use sha2::Sha256;

use crate::{APP, app};

pub(crate) fn register(registry: &mut Registry) {
    registry.handle("apps/list-webhook-deliveries", APP, list);
    registry.handle("apps/get-webhook-delivery", APP, get);
    registry.handle("apps/redeliver-webhook-delivery", APP, redeliver);
}

/// One webhook's delivery log: an App's (`hook_id` is the App's ID) or a repository or
/// organization webhook's.
pub struct DeliveryLog<'a> {
    pub hook_id: i64,
    /// The key that signs the log's page cursors, so a cursor of one log fails on another.
    pub key: &'a [u8],
    /// The log's API path, `app/hook/deliveries` or `repos/{owner}/{repo}/hooks/{id}/deliveries`.
    pub path: String,
}

#[derive(Clone, Copy)]
enum Filter {
    All,
    Success,
    Failure,
}

impl Filter {
    fn parse(value: Option<&str>) -> Result<Self, ApiError> {
        match value {
            None => Ok(Self::All),
            Some("success") => Ok(Self::Success),
            Some("failure") => Ok(Self::Failure),
            Some(_) => Err(ApiError::unprocessable("Invalid delivery status")),
        }
    }

    fn matches(self, code: i64) -> bool {
        match self {
            Self::All => true,
            Self::Success => (200..400).contains(&code),
            Self::Failure => (400..600).contains(&code),
        }
    }

    fn query(self) -> &'static str {
        match self {
            Self::All => "",
            Self::Success => "&status=success",
            Self::Failure => "&status=failure",
        }
    }
}

fn cursor_mac(log: &DeliveryLog<'_>, id: i64, filter: Filter) -> Result<Hmac<Sha256>, ApiError> {
    let mut mac = Hmac::new_from_slice(log.key).map_err(|e| ApiError::internal(e.to_string()))?;
    mac.update(b"hubdouble:hook:delivery-page");
    mac.update(&log.hook_id.to_be_bytes());
    mac.update(&id.to_be_bytes());
    mac.update(filter.query().as_bytes());
    Ok(mac)
}

const RETENTION_SECONDS: i64 = 3 * 86_400;

fn kept(ctx: &Ctx<'_>) -> impl Fn(&Delivery) -> bool + use<> {
    let since = ctx.now().plus_seconds(-RETENTION_SECONDS);
    move |d: &Delivery| d.delivered_at > since
}

fn invalid_cursor() -> ApiError {
    ApiError::unprocessable("Invalid delivery cursor")
}

fn decode_cursor(cursor: &str, log: &DeliveryLog<'_>, filter: Filter) -> Result<i64, ApiError> {
    let (id, tag) = cursor.split_once('.').ok_or_else(invalid_cursor)?;
    if id.starts_with('0') || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_cursor());
    }
    let id = id.parse().map_err(|_| invalid_cursor())?;
    let mut bytes = [0; 32];
    let len = URL_SAFE_NO_PAD
        .decode_slice(tag, &mut bytes)
        .map_err(|_| invalid_cursor())?;
    cursor_mac(log, id, filter)?
        .verify_slice(&bytes[..len])
        .map_err(|_| invalid_cursor())?;
    Ok(id)
}

/// A page of `log`, newest first, of the deliveries of the last three days, filtered by the
/// `status` query and paged by its signed `cursor`.
///
/// # Errors
///
/// `422` for an unknown `status` or a cursor of another log.
pub fn list_deliveries(ctx: &Ctx<'_>, log: &DeliveryLog<'_>) -> Result<Reply, ApiError> {
    let filter = Filter::parse(ctx.query().get("status"))?;
    let per_page = ctx.per_page();
    let kept = kept(ctx);
    let rows = ctx.tx().table::<Deliveries>();
    let before = ctx
        .query()
        .get("cursor")
        .map(|cursor| {
            let id = decode_cursor(cursor, log, filter)?;
            rows.get(id)
                .filter(|d| d.hook_id == log.hook_id)
                .map(|d| d.id)
                .ok_or_else(invalid_cursor)
        })
        .transpose()?;
    let mut items = rows
        .children(log.hook_id)
        .rev()
        .skip_while(|d| before.is_some_and(|id| d.id >= id))
        .filter(|d| filter.matches(d.status_code) && kept(d));
    let page: Vec<_> = items
        .by_ref()
        .take(per_page)
        .map(|d| HookDeliveryItem::new(d))
        .collect();
    let reply = Reply::json(&page)?;
    if let Some(last) = page.last()
        && items.next().is_some()
    {
        let id = last.id;
        let tag = cursor_mac(log, id, filter)?.finalize().into_bytes();
        let cursor = format!("{id}.{}", URL_SAFE_NO_PAD.encode(tag));
        let link = format!(
            "<{}/{}?per_page={per_page}&cursor={cursor}{}>; rel=\"next\"",
            ctx.urls().api,
            log.path,
            filter.query(),
        );
        return reply.header(LINK, &link);
    }
    Ok(reply)
}

fn retained<'a>(ctx: &'a Ctx<'_>, hook_id: i64) -> Result<&'a Delivery, ApiError> {
    let kept = kept(ctx);
    ctx.tx()
        .table::<Deliveries>()
        .get(ctx.param_i64("delivery_id")?)
        .map(AsRef::as_ref)
        .filter(|d| d.hook_id == hook_id && !d.request.payload.is_empty() && kept(d))
        .ok_or_else(ApiError::not_found)
}

/// The `{delivery_id}` delivery of the hook's log, with its request and response.
///
/// # Errors
///
/// `404` for a delivery of another hook, one older than three days, or one that lost its body.
pub fn get_delivery(ctx: &Ctx<'_>, hook_id: i64) -> Result<Reply, ApiError> {
    Reply::json(&HookDelivery::new(retained(ctx, hook_id)?))
}

/// Queues a redelivery of the `{delivery_id}` delivery of the hook's log and answers `202`.
///
/// # Errors
///
/// As [`get_delivery`].
pub fn redeliver_delivery(ctx: &mut Ctx<'_>, hook_id: i64) -> Result<Reply, ApiError> {
    let id = retained(ctx, hook_id)?.id;
    hubdouble_hooks::redeliver(ctx.tx_mut()?, hook_id, id)?;
    Ok(Reply::json(&serde_json::json!({}))?.status(StatusCode::ACCEPTED))
}

fn app_log<'a>(ctx: &'a Ctx<'_>) -> Result<DeliveryLog<'a>, ApiError> {
    let app = app(ctx)?;
    Ok(DeliveryLog {
        hook_id: app.id,
        key: app.client_secret.as_bytes(),
        path: "app/hook/deliveries".to_owned(),
    })
}

fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    list_deliveries(ctx, &app_log(ctx)?)
}

fn get(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    get_delivery(ctx, app(ctx)?.id)
}

fn redeliver(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let id = app(ctx)?.id;
    redeliver_delivery(ctx, id)
}
