use hubdouble_model::Timestamp;
use hubdouble_render::Urls;
use hubdouble_store::{Accounts, Entity, Repos, Rows, Tx};
use serde_json::{Value, json};

/// The repository or organization a webhook belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookOwner {
    Repo(i64),
    Org(i64),
}

impl HookOwner {
    #[must_use]
    pub fn id(self) -> i64 {
        match self {
            HookOwner::Repo(id) | HookOwner::Org(id) => id,
        }
    }

    /// The `X-GitHub-Hook-Installation-Target-Type` of the deliveries.
    #[must_use]
    pub fn target_type(self) -> &'static str {
        match self {
            HookOwner::Repo(_) => "repository",
            HookOwner::Org(_) => "organization",
        }
    }
}

/// How a webhook's body is sent: `application/json`, or `application/x-www-form-urlencoded`
/// with the JSON in a `payload` parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    Json,
    Form,
}

impl ContentType {
    #[must_use]
    pub fn parse(s: &str) -> Option<ContentType> {
        match s {
            "json" => Some(ContentType::Json),
            "form" => Some(ContentType::Form),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ContentType::Json => "json",
            ContentType::Form => "form",
        }
    }
}

/// A repository or organization webhook. Its ID comes from the global ID sequence, so it never
/// equals an App's ID, and its deliveries are logged under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    pub id: i64,
    pub owner: HookOwner,
    pub url: String,
    pub content_type: ContentType,
    pub secret: Option<String>,
    pub insecure_ssl: bool,
    /// The subscribed events; `*` subscribes to every event.
    pub events: Vec<String>,
    pub active: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Hook {
    /// Whether the hook gets `event`.
    #[must_use]
    pub fn subscribes(&self, event: &str) -> bool {
        self.events.iter().any(|e| e == "*" || e == event)
    }

    /// GitHub's `webhook-config`: the secret shows as `********` when it is set.
    #[must_use]
    pub fn config(&self) -> Value {
        let mut config = json!({
            "content_type": self.content_type.as_str(),
            "insecure_ssl": if self.insecure_ssl { "1" } else { "0" },
            "url": self.url,
        });
        if self.secret.as_deref().is_some_and(|s| !s.is_empty()) {
            config["secret"] = json!("********");
        }
        config
    }

    /// The API path of the hook, `repos/{owner}/{repo}/hooks/{id}` or `orgs/{org}/hooks/{id}`;
    /// `None` when its repository or organization is gone.
    #[must_use]
    pub fn path(&self, tx: &Tx) -> Option<String> {
        let accounts = tx.table::<Accounts>();
        match self.owner {
            HookOwner::Repo(id) => {
                let repo = tx.table::<Repos>().get(id)?;
                let owner = accounts.get(repo.owner_id)?;
                Some(format!(
                    "repos/{}/{}/hooks/{}",
                    owner.login, repo.name, self.id
                ))
            }
            HookOwner::Org(id) => Some(format!(
                "orgs/{}/hooks/{}",
                accounts.get(id)?.login,
                self.id
            )),
        }
    }

    /// GitHub's `hook` for a repository webhook and `org-hook` for an organization webhook.
    #[must_use]
    pub fn render(&self, tx: &Tx) -> Option<Value> {
        let url = format!("{}/{}", Urls::new(tx.config(), "").api, self.path(tx)?);
        let mut hook = json!({
            "id": self.id,
            "name": "web",
            "active": self.active,
            "events": self.events,
            "config": self.config(),
            "updated_at": self.updated_at,
            "created_at": self.created_at,
            "url": url,
            "ping_url": format!("{url}/pings"),
            "deliveries_url": format!("{url}/deliveries"),
        });
        match self.owner {
            HookOwner::Repo(_) => {
                hook["type"] = json!("Repository");
                hook["test_url"] = json!(format!("{url}/test"));
                hook["last_response"] = json!({"code": null, "status": "unused", "message": null});
            }
            HookOwner::Org(_) => hook["type"] = json!("Organization"),
        }
        Some(hook)
    }
}

/// The repository and organization webhooks; a hook's parent is its repository or organization.
pub type Hooks = Rows<Hook>;

impl Entity for Hook {
    fn id(&self) -> i64 {
        self.id
    }
    fn repo_id(&self) -> Option<i64> {
        match self.owner {
            HookOwner::Repo(id) => Some(id),
            HookOwner::Org(_) => None,
        }
    }
    fn parent_id(&self) -> Option<i64> {
        Some(self.owner.id())
    }
}
