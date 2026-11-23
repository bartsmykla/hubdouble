//! The fixture seed every track starts from: three users with personal tokens, an org with an
//! admin and a member, an org that enforces SAML SSO, a public and a private repository, and an
//! App installed on the org for one repository with narrowed permissions.

use serde_json::{Value, json};

/// The variable the App's webhook URL is read from; [`crate::Builder::start`] sets it to the
/// receiver's URL.
pub const WEBHOOK_URL_ENV: &str = "HUBDOUBLE_TEST_WEBHOOK_URL";
pub const WEBHOOK_SECRET: &str = "fixture-webhook-secret";

pub const ALICE: &str = "alice";
pub const BOB: &str = "bob";
pub const CAROL: &str = "carol";
pub const ORG: &str = "acme";
pub const SSO_ORG: &str = "acme-sso";
pub const PUBLIC_REPO: &str = "public";
pub const PRIVATE_REPO: &str = "private";
pub const APP_SLUG: &str = "fixture-app";

pub const ALICE_TOKEN: &str = "ghp_a11ce0000000000000000000000000000000";
pub const BOB_TOKEN: &str = "ghp_b0b000000000000000000000000000000000";
pub const CAROL_TOKEN: &str = "ghp_ca4010000000000000000000000000000000";

pub const ALICE_ID: i64 = 1001;
pub const BOB_ID: i64 = 1002;
pub const CAROL_ID: i64 = 1003;
pub const ORG_ID: i64 = 2001;
pub const SSO_ORG_ID: i64 = 2002;
pub const PUBLIC_REPO_ID: i64 = 3001;
pub const PRIVATE_REPO_ID: i64 = 3002;
pub const APP_ID: i64 = 4001;
pub const INSTALLATION_ID: i64 = 5001;

/// The App's RSA key pair, made for tests only.
pub const APP_PRIVATE_KEY: &str = include_str!("../fixture/app-private-key.pem");
pub const APP_PUBLIC_KEY: &str = include_str!("../fixture/app-public-key.pem");

/// The fixture entries. `acme/public` has `main` with `README.md`, a `develop` branch at
/// `main`, an annotated tag `v1.0.0` on `main`, and a `feature` branch one commit ahead.
#[must_use]
pub fn seed() -> Vec<Value> {
    vec![
        json!({"kind": "user", "id": ALICE_ID, "login": ALICE, "name": "Alice",
               "email": "alice@example.com", "tokens": [{"token": ALICE_TOKEN}]}),
        json!({"kind": "user", "id": BOB_ID, "login": BOB, "name": "Bob",
               "tokens": [{"token": BOB_TOKEN}]}),
        json!({"kind": "user", "id": CAROL_ID, "login": CAROL,
               "tokens": [{"token": CAROL_TOKEN}]}),
        json!({"kind": "org", "id": ORG_ID, "login": ORG, "name": "Acme",
               "members": [{"login": ALICE, "role": "admin"}, {"login": BOB}]}),
        json!({"kind": "org", "id": SSO_ORG_ID, "login": SSO_ORG, "sso_enforced": true,
               "members": [{"login": ALICE, "role": "admin"}]}),
        json!({"kind": "repository", "id": PUBLIC_REPO_ID, "owner": ORG, "name": PUBLIC_REPO,
               "visibility": "public", "has_issues": true,
               "files": {"README.md": "# public\n"},
               "branches": ["develop"],
               "tags": [{"name": "v1.0.0", "message": "first release"}],
               "commits": [{"branch": "feature", "message": "add the feature",
                            "files": {"src/feature.txt": "feature\n"}}]}),
        json!({"kind": "repository", "id": PRIVATE_REPO_ID, "owner": ORG, "name": PRIVATE_REPO,
               "visibility": "private", "has_issues": true,
               "files": {"README.md": "# private\n"},
               "collaborators": [{"login": CAROL, "permission": "read"}]}),
        json!({"kind": "app", "id": APP_ID, "slug": APP_SLUG, "name": "Fixture App", "owner": ORG,
               "public_key": APP_PUBLIC_KEY, "private_key": APP_PRIVATE_KEY,
               "webhook": {"url": format!("{{{{env \"{WEBHOOK_URL_ENV}\"}}}}"),
                           "secret": WEBHOOK_SECRET},
               "permissions": {"contents": "write", "issues": "write", "pull_requests": "write",
                               "checks": "write", "metadata": "read"},
               "events": ["push", "create", "issues", "issue_comment", "pull_request", "check_run"]}),
        json!({"kind": "installation", "id": INSTALLATION_ID, "app": APP_SLUG, "account": ORG,
               "repositories": [PUBLIC_REPO],
               "permissions": {"contents": "read", "issues": "write", "metadata": "read",
                               "pull_requests": "read"}}),
    ]
}
