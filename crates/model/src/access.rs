use serde::{Deserialize, Serialize};

/// A GitHub App permission level. Ordered: `Read < Write < Admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Read,
    Write,
    Admin,
}

impl Access {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write",
            Access::Admin => "admin",
        }
    }
}

/// One GitHub App permission at a level, such as `contents: write`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Grant {
    pub permission: &'static str,
    pub access: Access,
}

impl Grant {
    #[must_use]
    pub const fn read(permission: &'static str) -> Grant {
        Grant {
            permission,
            access: Access::Read,
        }
    }

    #[must_use]
    pub const fn write(permission: &'static str) -> Grant {
        Grant {
            permission,
            access: Access::Write,
        }
    }

    #[must_use]
    pub const fn admin(permission: &'static str) -> Grant {
        Grant {
            permission,
            access: Access::Admin,
        }
    }
}

/// What an operation acts on, which decides how the actor's grant is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The repository named by the `{owner}`/`{repo}` path parameters.
    Repo,
    /// The organization named by the `{org}` path parameter.
    Org,
    /// The App itself; only an App JWT passes.
    App,
    /// The authenticated user.
    User,
    /// The installation behind an installation token.
    Installation,
    /// Public data; any actor passes.
    Public,
}

/// The access an operation needs, checked by the authorization choke point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requirement {
    /// Any one grant suffices, as GitHub documents per operation; empty for public data.
    pub any_of: &'static [Grant],
    /// Grants needed together with one of `any_of`, for the permission sets that GitHub
    /// documents with more than one permission (Pages needs `pages` and `administration`).
    pub all_of: &'static [Grant],
    pub scope: Scope,
    /// Classic OAuth scopes, any one of which suffices for a classic token.
    pub scopes: &'static [&'static str],
}

impl Requirement {
    #[must_use]
    pub const fn public() -> Requirement {
        Requirement {
            any_of: &[],
            all_of: &[],
            scope: Scope::Public,
            scopes: &[],
        }
    }
}
