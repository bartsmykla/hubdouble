use hubdouble_model as m;
use serde::Serialize;

use crate::{Urls, url};

/// GitHub's `simple-user`, for users, bots and organizations as actors.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SimpleUser {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub login: String,
    pub id: i64,
    pub node_id: String,
    pub avatar_url: String,
    pub gravatar_id: String,
    pub url: String,
    pub html_url: String,
    pub followers_url: String,
    pub following_url: String,
    pub gists_url: String,
    pub starred_url: String,
    pub subscriptions_url: String,
    pub organizations_url: String,
    pub repos_url: String,
    pub events_url: String,
    pub received_events_url: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub site_admin: bool,
    pub user_view_type: &'static str,
}

impl SimpleUser {
    #[must_use]
    pub fn new(urls: &Urls, a: &m::Account) -> SimpleUser {
        let api = url!("{}/users/{}", urls.api, crate::escaped(&a.login));
        let html_url = match (a.kind, a.login.strip_suffix("[bot]")) {
            (m::AccountKind::Bot, Some(slug)) => url!("{}/apps/{slug}", urls.web),
            _ => url!("{}/{}", urls.web, a.login),
        };
        SimpleUser {
            name: None,
            email: None,
            login: a.login.clone(),
            id: a.id,
            node_id: a.node_id.clone(),
            avatar_url: url!("{}/avatars/u/{}?v=4", urls.base, a.id),
            gravatar_id: String::new(),
            html_url,
            followers_url: crate::joined(&api, "/followers"),
            following_url: crate::joined(&api, "/following{/other_user}"),
            gists_url: crate::joined(&api, "/gists{/gist_id}"),
            starred_url: crate::joined(&api, "/starred{/owner}{/repo}"),
            subscriptions_url: crate::joined(&api, "/subscriptions"),
            organizations_url: crate::joined(&api, "/orgs"),
            repos_url: crate::joined(&api, "/repos"),
            events_url: crate::joined(&api, "/events{/privacy}"),
            received_events_url: crate::joined(&api, "/received_events"),
            url: api,
            kind: a.kind.as_str(),
            site_admin: a.site_admin,
            user_view_type: "public",
        }
    }
}

/// GitHub's `organization-simple`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OrganizationSimple {
    pub login: String,
    pub id: i64,
    pub node_id: String,
    pub url: String,
    pub repos_url: String,
    pub events_url: String,
    pub hooks_url: String,
    pub issues_url: String,
    pub members_url: String,
    pub public_members_url: String,
    pub avatar_url: String,
    pub description: Option<String>,
}

impl OrganizationSimple {
    #[must_use]
    pub fn new(urls: &Urls, org: &m::Account) -> OrganizationSimple {
        let api = url!("{}/orgs/{}", urls.api, org.login);
        OrganizationSimple {
            login: org.login.clone(),
            id: org.id,
            node_id: org.node_id.clone(),
            repos_url: crate::joined(&api, "/repos"),
            events_url: crate::joined(&api, "/events"),
            hooks_url: crate::joined(&api, "/hooks"),
            issues_url: crate::joined(&api, "/issues"),
            members_url: crate::joined(&api, "/members{/member}"),
            public_members_url: crate::joined(&api, "/public_members{/member}"),
            avatar_url: url!("{}/avatars/u/{}?v=4", urls.base, org.id),
            description: org.description.clone(),
            url: api,
        }
    }
}

/// GitHub's `team-simple`, the parent of a [`Team`] and the requested teams of a pull request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TeamSimple {
    pub id: i64,
    pub node_id: String,
    pub url: String,
    pub members_url: String,
    pub name: String,
    pub description: Option<String>,
    pub permission: &'static str,
    pub privacy: &'static str,
    pub notification_setting: &'static str,
    pub html_url: String,
    pub repositories_url: String,
    pub slug: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub organization_id: i64,
}

impl TeamSimple {
    #[must_use]
    pub fn new(urls: &Urls, t: &m::Team, org: &m::Account) -> TeamSimple {
        let api = url!("{}/organizations/{}/team/{}", urls.api, t.org_id, t.id);
        TeamSimple {
            id: t.id,
            node_id: t.node_id.clone(),
            members_url: crate::joined(&api, "/members{/member}"),
            repositories_url: crate::joined(&api, "/repos"),
            url: api,
            name: t.name.clone(),
            description: t.description.clone(),
            permission: "pull",
            privacy: t.privacy.as_str(),
            notification_setting: if t.notifications_enabled {
                "notifications_enabled"
            } else {
                "notifications_disabled"
            },
            html_url: url!("{}/orgs/{}/teams/{}", urls.web, org.login, t.slug),
            slug: t.slug.clone(),
            kind: "organization",
            organization_id: t.org_id,
        }
    }
}

/// GitHub's `team`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Team {
    #[serde(flatten)]
    pub team: TeamSimple,
    pub parent: Option<TeamSimple>,
}

impl Team {
    #[must_use]
    pub fn new(urls: &Urls, t: &m::Team, org: &m::Account, parent: Option<&m::Team>) -> Team {
        Team {
            team: TeamSimple::new(urls, t, org),
            parent: parent.map(|p| TeamSimple::new(urls, p, org)),
        }
    }
}

/// GitHub's `org-membership`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OrgMembership {
    pub url: String,
    pub state: &'static str,
    pub role: &'static str,
    pub organization_url: String,
    pub organization: OrganizationSimple,
    pub user: Option<SimpleUser>,
}

impl OrgMembership {
    #[must_use]
    pub fn new(
        urls: &Urls,
        ms: &m::Membership,
        org: &m::Account,
        user: &m::Account,
    ) -> OrgMembership {
        OrgMembership {
            url: url!("{}/orgs/{}/memberships/{}", urls.api, org.login, user.login),
            state: ms.state.as_str(),
            role: ms.role.as_str(),
            organization_url: url!("{}/orgs/{}", urls.api, org.login),
            organization: OrganizationSimple::new(urls, org),
            user: Some(SimpleUser::new(urls, user)),
        }
    }
}

/// GitHub's `team-membership`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TeamMembership {
    pub url: String,
    pub role: &'static str,
    pub state: &'static str,
}

impl TeamMembership {
    #[must_use]
    pub fn new(
        urls: &Urls,
        t: &m::Team,
        member: &m::TeamMember,
        user: &m::Account,
    ) -> TeamMembership {
        TeamMembership {
            url: url!(
                "{}/organizations/{}/team/{}/memberships/{}",
                urls.api,
                t.org_id,
                t.id,
                user.login
            ),
            role: member.role.as_str(),
            state: "active",
        }
    }
}
