use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use hubdouble_model::Config;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("{name}={value:?} is not valid: {reason}")]
    Invalid {
        name: &'static str,
        value: String,
        reason: &'static str,
    },
}

fn invalid(name: &'static str, value: &str, reason: &'static str) -> ConfigError {
    ConfigError::Invalid {
        name,
        value: value.to_owned(),
        reason,
    }
}

/// Parses the `HUBDOUBLE_*` variables through `lookup`; an empty value counts as unset.
///
/// # Errors
///
/// [`ConfigError`] naming the first variable that does not parse.
pub fn config_from_env(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
    let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
    let defaults = Config::default();
    let addr = match get("HUBDOUBLE_ADDR") {
        Some(v) => parse_addr(&v)
            .ok_or_else(|| invalid("HUBDOUBLE_ADDR", &v, "expected :port or ip:port"))?,
        None => defaults.addr,
    };
    let origin = |name: &'static str| -> Result<Option<String>, ConfigError> {
        get(name)
            .map(|v| {
                let trimmed = v.trim().trim_end_matches('/');
                if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                    Ok(trimmed.to_owned())
                } else {
                    Err(invalid(name, &v, "expected an http:// or https:// URL"))
                }
            })
            .transpose()
    };
    let base_url = origin("HUBDOUBLE_BASE_URL")?.unwrap_or(defaults.base_url);
    let web_url = origin("HUBDOUBLE_WEB_URL")?.unwrap_or_else(|| base_url.clone());
    let oidc_issuer = match origin("HUBDOUBLE_OIDC_ISSUER")? {
        Some(issuer) if issuer.ends_with("/_actions") => issuer,
        Some(issuer) => {
            return Err(invalid(
                "HUBDOUBLE_OIDC_ISSUER",
                &issuer,
                "expected a URL whose path ends in /_actions, where hubdouble serves the issuer",
            ));
        }
        None => format!("{base_url}/_actions"),
    };
    let workers = get("HUBDOUBLE_WORKERS")
        .map(|v| {
            v.trim()
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| invalid("HUBDOUBLE_WORKERS", &v, "expected a positive integer"))
        })
        .transpose()?;
    Ok(Config {
        addr,
        base_url,
        web_url,
        oidc_issuer,
        admin_token: get("HUBDOUBLE_ADMIN_TOKEN").unwrap_or(defaults.admin_token),
        seed: get("HUBDOUBLE_SEED").map(PathBuf::from),
        workers,
        ..defaults
    })
}

fn parse_addr(v: &str) -> Option<SocketAddr> {
    let v = v.trim();
    match v.strip_prefix(':') {
        Some(port) => Some(SocketAddr::from((
            Ipv4Addr::UNSPECIFIED,
            port.parse::<u16>().ok()?,
        ))),
        None => v.parse().ok(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn parse(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let env: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        config_from_env(&|name| env.get(name).cloned())
    }

    #[test]
    fn defaults_follow_the_plan() {
        let c = parse(&[]).unwrap();
        assert_eq!(c.addr, "0.0.0.0:8000".parse().unwrap());
        assert_eq!(c.base_url, "http://localhost:8000");
        assert_eq!(c.web_url, c.base_url);
        assert_eq!(c.oidc_issuer, "http://localhost:8000/_actions");
        assert_eq!(c.admin_token, "hubdouble-admin");
        assert_eq!(c.seed, None);
        assert_eq!(c.workers, None);
    }

    #[test]
    fn an_issuer_override_keeps_the_actions_path() {
        let moved = parse(&[("HUBDOUBLE_OIDC_ISSUER", "http://issuer.test/_actions")]).unwrap();
        assert_eq!(moved.oidc_issuer, "http://issuer.test/_actions");
        assert!(matches!(
            parse(&[("HUBDOUBLE_OIDC_ISSUER", "http://issuer.test/oidc")]),
            Err(ConfigError::Invalid {
                name: "HUBDOUBLE_OIDC_ISSUER",
                ..
            })
        ));
    }

    #[test]
    fn the_local_cluster_environment_parses() {
        let c = parse(&[
            ("HUBDOUBLE_ADDR", ":8000"),
            ("HUBDOUBLE_BASE_URL", "http://hubdouble:8000/"),
            ("HUBDOUBLE_WEB_URL", "https://github.com"),
            ("HUBDOUBLE_SEED", "/seed/client-v5.yaml"),
            ("HUBDOUBLE_ADMIN_TOKEN", "s3cret"),
            ("HUBDOUBLE_WORKERS", "2"),
            ("HUBDOUBLE_OIDC_ISSUER", ""),
        ])
        .unwrap();
        assert_eq!(c.base_url, "http://hubdouble:8000");
        assert_eq!(c.web_url, "https://github.com");
        assert_eq!(c.oidc_issuer, "http://hubdouble:8000/_actions");
        assert_eq!(c.seed, Some(PathBuf::from("/seed/client-v5.yaml")));
        assert_eq!(c.admin_token, "s3cret");
        assert_eq!(c.workers, Some(2));
    }

    #[test]
    fn bad_values_name_their_variable() {
        for (name, value) in [
            ("HUBDOUBLE_ADDR", "8000"),
            ("HUBDOUBLE_BASE_URL", "hubdouble:8000"),
            ("HUBDOUBLE_WORKERS", "0"),
        ] {
            let ConfigError::Invalid { name: got, .. } = parse(&[(name, value)]).unwrap_err();
            assert_eq!(got, name);
        }
    }
}
