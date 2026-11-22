use std::collections::BTreeMap;

use hubdouble_model as m;
use serde::Serialize;

use crate::{Repository, SimpleUser, Urls};

/// GitHub's `integration`: an App.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Integration {
    pub id: i64,
    pub slug: String,
    pub node_id: String,
    pub client_id: String,
    pub owner: SimpleUser,
    pub name: String,
    pub description: Option<String>,
    pub external_url: String,
    pub html_url: String,
    pub created_at: m::Timestamp,
    pub updated_at: m::Timestamp,
    pub permissions: BTreeMap<String, m::Access>,
    pub events: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installations_count: Option<i64>,
}

impl Integration {
    #[must_use]
    pub fn new(urls: &Urls, app: &m::App, owner: &m::Account) -> Integration {
        Integration {
            id: app.id,
            slug: app.slug.clone(),
            node_id: app.node_id.clone(),
            client_id: app.client_id.clone(),
            owner: SimpleUser::new(urls, owner),
            name: app.name.clone(),
            description: app.description.clone(),
            external_url: app.external_url.clone(),
            html_url: format!("{}/apps/{}", urls.web, app.slug),
            created_at: app.created_at,
            updated_at: app.updated_at,
            permissions: app.permissions.clone(),
            events: app.events.clone(),
            installations_count: None,
        }
    }
}

/// GitHub's `installation`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Installation {
    pub id: i64,
    pub client_id: String,
    pub account: Option<SimpleUser>,
    pub repository_selection: &'static str,
    pub access_tokens_url: String,
    pub repositories_url: String,
    pub html_url: String,
    pub app_id: i64,
    pub app_slug: String,
    pub target_id: i64,
    pub target_type: &'static str,
    pub permissions: BTreeMap<String, m::Access>,
    pub events: Vec<String>,
    pub created_at: m::Timestamp,
    pub updated_at: m::Timestamp,
    pub single_file_name: Option<String>,
    pub has_multiple_single_files: bool,
    pub single_file_paths: Vec<String>,
    pub suspended_by: Option<SimpleUser>,
    pub suspended_at: Option<m::Timestamp>,
}

impl Installation {
    #[must_use]
    pub fn new(
        urls: &Urls,
        i: &m::Installation,
        app: &m::App,
        account: &m::Account,
        suspended_by: Option<&m::Account>,
    ) -> Installation {
        let html_url = match account.kind {
            m::AccountKind::Organization => format!(
                "{}/organizations/{}/settings/installations/{}",
                urls.web, account.login, i.id
            ),
            _ => format!("{}/settings/installations/{}", urls.web, i.id),
        };
        Installation {
            id: i.id,
            client_id: app.client_id.clone(),
            account: Some(SimpleUser::new(urls, account)),
            repository_selection: i.repository_selection.as_str(),
            access_tokens_url: format!("{}/app/installations/{}/access_tokens", urls.api, i.id),
            repositories_url: format!("{}/installation/repositories", urls.api),
            html_url,
            app_id: app.id,
            app_slug: app.slug.clone(),
            target_id: account.id,
            target_type: account.kind.as_str(),
            permissions: i.permissions.clone(),
            events: i.events.clone(),
            created_at: i.created_at,
            updated_at: i.updated_at,
            single_file_name: None,
            has_multiple_single_files: false,
            single_file_paths: Vec::new(),
            suspended_by: suspended_by.map(|a| SimpleUser::new(urls, a)),
            suspended_at: i.suspended_at,
        }
    }
}

/// GitHub's `installation-token`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InstallationToken {
    pub token: String,
    pub expires_at: m::Timestamp,
    pub permissions: BTreeMap<String, m::Access>,
    pub repository_selection: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repositories: Option<Vec<Repository>>,
}

/// GitHub's `hook-delivery-item`, a row of `GET /app/hook/deliveries`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HookDeliveryItem {
    pub id: i64,
    pub guid: String,
    pub delivered_at: m::Timestamp,
    pub redelivery: bool,
    pub duration: f64,
    pub status: String,
    pub status_code: i64,
    pub event: String,
    pub action: Option<String>,
    pub installation_id: Option<i64>,
    pub repository_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub throttled_at: Option<m::Timestamp>,
}

impl HookDeliveryItem {
    #[must_use]
    pub fn new(d: &m::Delivery) -> HookDeliveryItem {
        HookDeliveryItem {
            id: d.id,
            guid: d.guid.clone(),
            delivered_at: d.delivered_at,
            redelivery: d.redelivery,
            duration: d.duration,
            status: d.status.clone(),
            status_code: d.status_code,
            event: d.event.clone(),
            action: d.action.clone(),
            installation_id: d.installation_id,
            repository_id: d.repo_id,
            throttled_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HookDeliveryRequest {
    pub headers: BTreeMap<String, String>,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HookDeliveryResponse {
    pub headers: BTreeMap<String, String>,
    pub payload: Option<String>,
}

/// GitHub's `hook-delivery`, the answer of `GET /app/hook/deliveries/{id}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HookDelivery {
    #[serde(flatten)]
    pub item: HookDeliveryItem,
    pub url: String,
    pub request: HookDeliveryRequest,
    pub response: HookDeliveryResponse,
}

impl HookDelivery {
    #[must_use]
    pub fn new(d: &m::Delivery) -> HookDelivery {
        HookDelivery {
            item: HookDeliveryItem::new(d),
            url: d.url.clone(),
            request: HookDeliveryRequest {
                headers: d.request.headers.clone(),
                payload: serde_json::from_slice(&d.request.payload)
                    .unwrap_or(serde_json::Value::Null),
            },
            response: HookDeliveryResponse {
                headers: d.response.headers.clone(),
                payload: d.response.payload.clone(),
            },
        }
    }
}
