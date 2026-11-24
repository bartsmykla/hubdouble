use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use http::{StatusCode, header::LINK};
use hubdouble_model::{ApiError, App, Delivery};
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

fn cursor_mac(app: &App, id: i64, filter: Filter) -> Result<Hmac<Sha256>, ApiError> {
    let mut mac = Hmac::new_from_slice(app.client_secret.as_bytes())
        .map_err(|e| ApiError::internal(e.to_string()))?;
    mac.update(b"hubdouble:app:delivery-page");
    mac.update(&app.id.to_be_bytes());
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

fn decode_cursor(cursor: &str, app: &App, filter: Filter) -> Result<i64, ApiError> {
    let (id, tag) = cursor.split_once('.').ok_or_else(invalid_cursor)?;
    if id.starts_with('0') || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_cursor());
    }
    let id = id.parse().map_err(|_| invalid_cursor())?;
    let mut bytes = [0; 32];
    let len = URL_SAFE_NO_PAD
        .decode_slice(tag, &mut bytes)
        .map_err(|_| invalid_cursor())?;
    cursor_mac(app, id, filter)?
        .verify_slice(&bytes[..len])
        .map_err(|_| invalid_cursor())?;
    Ok(id)
}

fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let app = app(ctx)?;
    let filter = Filter::parse(ctx.query().get("status"))?;
    let per_page = ctx.per_page();
    let kept = kept(ctx);
    let rows = ctx.tx().table::<Deliveries>();
    let before = ctx
        .query()
        .get("cursor")
        .map(|cursor| {
            let id = decode_cursor(cursor, app, filter)?;
            rows.get(id)
                .filter(|d| d.app_id == app.id)
                .map(|d| d.id)
                .ok_or_else(invalid_cursor)
        })
        .transpose()?;
    let mut items = rows
        .children(app.id)
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
        let tag = cursor_mac(app, id, filter)?.finalize().into_bytes();
        let cursor = format!("{id}.{}", URL_SAFE_NO_PAD.encode(tag));
        let link = format!(
            "<{}/app/hook/deliveries?per_page={per_page}&cursor={cursor}{}>; rel=\"next\"",
            ctx.urls().api,
            filter.query(),
        );
        return reply.header(LINK, &link);
    }
    Ok(reply)
}

fn get(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let delivery = retained(ctx)?;
    Reply::json(&HookDelivery::new(delivery))
}

fn retained<'a>(ctx: &'a Ctx<'_>) -> Result<&'a Delivery, ApiError> {
    let app = app(ctx)?;
    let kept = kept(ctx);
    ctx.tx()
        .table::<Deliveries>()
        .get(ctx.param_i64("delivery_id")?)
        .map(AsRef::as_ref)
        .filter(|d| d.app_id == app.id && !d.request.payload.is_empty() && kept(d))
        .ok_or_else(ApiError::not_found)
}

fn redeliver(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let (app_id, id) = {
        let d = retained(ctx)?;
        (d.app_id, d.id)
    };
    hubdouble_hooks::redeliver(ctx.tx_mut()?, app_id, id)?;
    Ok(Reply::json(&serde_json::json!({}))?.status(StatusCode::ACCEPTED))
}
