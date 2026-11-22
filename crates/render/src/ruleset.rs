use hubdouble_model as m;
use serde::Serialize;

use crate::{Href, Urls};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RulesetLinks {
    #[serde(rename = "self")]
    pub self_link: Href,
    pub html: Href,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RulesetBypassActor {
    pub actor_id: Option<i64>,
    pub actor_type: String,
    pub bypass_mode: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RulesetRule {
    #[serde(rename = "type")]
    pub rule_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

/// GitHub's `repository-ruleset`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Ruleset {
    pub id: i64,
    pub name: String,
    pub target: &'static str,
    pub source_type: &'static str,
    /// The `owner/name` of a repository ruleset, the login of an organization ruleset.
    pub source: String,
    pub enforcement: &'static str,
    pub bypass_actors: Vec<RulesetBypassActor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_user_can_bypass: Option<&'static str>,
    pub node_id: String,
    #[serde(rename = "_links")]
    pub links: RulesetLinks,
    pub conditions: Option<serde_json::Value>,
    pub rules: Vec<RulesetRule>,
    pub created_at: m::Timestamp,
    pub updated_at: m::Timestamp,
}

impl Ruleset {
    #[must_use]
    pub fn new(urls: &Urls, r: &m::Ruleset, source: &str) -> Ruleset {
        let (api, html) = match r.source_type {
            m::RulesetSource::Repository => (
                format!("{}/repos/{source}/rulesets/{}", urls.api, r.id),
                format!("{}/{source}/rules/{}", urls.web, r.id),
            ),
            m::RulesetSource::Organization => (
                format!("{}/orgs/{source}/rulesets/{}", urls.api, r.id),
                format!(
                    "{}/organizations/{source}/settings/rules/{}",
                    urls.web, r.id
                ),
            ),
        };
        Ruleset {
            id: r.id,
            name: r.name.clone(),
            target: r.target.as_str(),
            source_type: r.source_type.as_str(),
            source: source.to_owned(),
            enforcement: r.enforcement.as_str(),
            bypass_actors: r
                .bypass_actors
                .iter()
                .map(|b| RulesetBypassActor {
                    actor_id: b.actor_id,
                    actor_type: b.actor_type.clone(),
                    bypass_mode: b.bypass_mode.clone(),
                })
                .collect(),
            current_user_can_bypass: None,
            node_id: r.node_id.clone(),
            links: RulesetLinks {
                self_link: Href { href: api },
                html: Href { href: html },
            },
            conditions: r.conditions.clone(),
            rules: r
                .rules
                .iter()
                .map(|rule| RulesetRule {
                    rule_type: rule.rule_type.clone(),
                    parameters: rule.parameters.clone(),
                })
                .collect(),
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}
