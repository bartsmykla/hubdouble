use hubdouble_model::ApiError;

/// Why a credential, a token request or a registered requirement was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("the JSON web token could not be decoded")]
    JwtMalformed,
    #[error("the JSON web token is not signed with RS256")]
    JwtAlgorithm,
    #[error("the JSON web token names no known App in iss")]
    JwtUnknownIssuer,
    #[error("the App's public key could not be read")]
    JwtKey,
    #[error("the JSON web token signature does not match the App's key")]
    JwtSignature,
    #[error("the JSON web token has no integer iat")]
    JwtMissingIat,
    #[error("the JSON web token has no integer exp")]
    JwtMissingExp,
    #[error("the JSON web token exp is past")]
    JwtExpired,
    #[error("the JSON web token exp is more than 10 minutes after now")]
    JwtExpTooFar,
    #[error("the JSON web token iat is more than 60 seconds after now")]
    JwtIatInFuture,
    #[error("a requested repository does not exist or is not in the installation")]
    RepositoryNotAccessible,
    #[error("more repositories requested than one token may hold")]
    TooManyRepositories,
    #[error("a requested permission is not granted to the installation")]
    PermissionNotGranted,
    #[error("a non-public requirement names no permission")]
    NoGrant,
    #[error("the operation's path has no parameter that names the requirement's target")]
    NoTarget,
    #[error("a public requirement names a permission it never checks")]
    PublicWithGrant,
    #[error("unknown GitHub App permission {0}")]
    UnknownPermission(&'static str),
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> ApiError {
        match e {
            AuthError::JwtMissingIat | AuthError::JwtIatInFuture => ApiError::new(
                401,
                "'Issued at' claim ('iat') must be an Integer representing the time that the assertion was issued",
            ),
            AuthError::JwtMissingExp | AuthError::JwtExpired => ApiError::new(
                401,
                "'Expiration time' claim ('exp') must be a numeric value representing the future time at which the assertion expires",
            ),
            AuthError::JwtExpTooFar => ApiError::new(
                401,
                "'Expiration time' claim ('exp') is too far in the future",
            ),
            AuthError::JwtMalformed
            | AuthError::JwtAlgorithm
            | AuthError::JwtUnknownIssuer
            | AuthError::JwtKey
            | AuthError::JwtSignature => {
                ApiError::new(401, "A JSON web token could not be decoded")
            }
            AuthError::RepositoryNotAccessible | AuthError::TooManyRepositories => {
                ApiError::unprocessable(
                    "There is at least one repository that does not exist or is not accessible to the parent installation.",
                )
            }
            AuthError::PermissionNotGranted => ApiError::unprocessable(
                "The permissions requested are not granted to this installation.",
            ),
            AuthError::NoGrant
            | AuthError::NoTarget
            | AuthError::PublicWithGrant
            | AuthError::UnknownPermission(_) => ApiError::internal(e.to_string()),
        }
    }
}
