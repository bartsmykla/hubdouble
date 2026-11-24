use serde_json::Value;

use crate::{Enforcement, Repo, Ruleset, RulesetSource, RulesetTarget};

/// GitHub's ruleset `fnmatch` patterns: `*` matches within one path segment, `?` one character
/// other than `/`, `**/` zero or more whole segments, and `**` anything.
#[must_use]
pub fn glob(pattern: &str, text: &str) -> bool {
    matches(pattern.as_bytes(), text.as_bytes())
}

fn matches(p: &[u8], t: &[u8]) -> bool {
    match p {
        [] => t.is_empty(),
        [b'*', b'*', b'/', rest @ ..] => {
            matches(rest, t)
                || t.iter()
                    .enumerate()
                    .any(|(i, &c)| c == b'/' && matches(rest, &t[i + 1..]))
        }
        [b'*', b'*', rest @ ..] => (0..=t.len()).any(|i| matches(rest, &t[i..])),
        [b'*', rest @ ..] => {
            let segment = t.iter().position(|&c| c == b'/').unwrap_or(t.len());
            (0..=segment).any(|i| matches(rest, &t[i..]))
        }
        [b'?', rest @ ..] => matches!(t, [c, ..] if *c != b'/') && matches(rest, &t[1..]),
        [c, rest @ ..] => t.first() == Some(c) && matches(rest, &t[1..]),
    }
}

fn admits(condition: &Value, name: &str, alias: impl Fn(&str) -> Option<bool>) -> bool {
    let list = |key: &str| {
        condition
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
    };
    let hit = |p: &&str| alias(p).unwrap_or_else(|| glob(p, name));
    list("include").iter().any(hit) && !list("exclude").iter().any(hit)
}

/// Whether a ruleset's `ref_name` condition admits `refs/heads/<branch>` on a repository whose
/// default branch is `default_branch`. A ruleset without the condition targets no branch.
#[must_use]
pub fn ref_matches(conditions: Option<&Value>, branch: &str, default_branch: &str) -> bool {
    let Some(ref_name) = conditions.and_then(|c| c.get("ref_name")) else {
        return false;
    };
    let full = format!("refs/heads/{branch}");
    admits(ref_name, &full, |p| match p {
        "~ALL" => Some(true),
        "~DEFAULT_BRANCH" => Some(branch == default_branch),
        _ => None,
    })
}

fn tag_matches(conditions: Option<&Value>, tag: &str) -> bool {
    let Some(ref_name) = conditions.and_then(|c| c.get("ref_name")) else {
        return false;
    };
    admits(ref_name, &format!("refs/tags/{tag}"), |p| {
        (p == "~ALL").then_some(true)
    })
}

fn repo_matches(conditions: Option<&Value>, repo: &Repo) -> bool {
    let Some(c) = conditions else {
        return false;
    };
    if let Some(names) = c.get("repository_name") {
        return admits(names, &repo.name, |p| (p == "~ALL").then_some(true));
    }
    c.pointer("/repository_id/repository_ids")
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|id| id.as_i64() == Some(repo.id)))
}

impl Ruleset {
    /// Whether the ruleset governs the repository: its own ruleset, or one of its owner's whose
    /// repository condition admits it.
    #[must_use]
    pub fn governs(&self, repo: &Repo) -> bool {
        match self.source_type {
            RulesetSource::Repository => self.repo_id == Some(repo.id),
            RulesetSource::Organization => {
                self.source_id == repo.owner_id && repo_matches(self.conditions.as_ref(), repo)
            }
        }
    }

    /// Whether the ruleset is an active branch ruleset that applies to `branch` of the repository.
    #[must_use]
    pub fn applies_to_branch(&self, repo: &Repo, branch: &str) -> bool {
        self.enforcement == Enforcement::Active
            && self.target == RulesetTarget::Branch
            && self.governs(repo)
            && ref_matches(self.conditions.as_ref(), branch, &repo.default_branch)
    }

    /// Whether the ruleset is an active tag ruleset that applies to `tag` of the repository.
    #[must_use]
    pub fn applies_to_tag(&self, repo: &Repo, tag: &str) -> bool {
        self.enforcement == Enforcement::Active
            && self.target == RulesetTarget::Tag
            && self.governs(repo)
            && tag_matches(self.conditions.as_ref(), tag)
    }

    /// Whether the ruleset is an active push ruleset of the repository or of `root`, the
    /// repository at the root of its fork network: GitHub applies a push ruleset to the whole
    /// network.
    #[must_use]
    pub fn applies_to_push(&self, repo: &Repo, root: &Repo) -> bool {
        self.enforcement == Enforcement::Active
            && self.target == RulesetTarget::Push
            && (self.governs(repo) || self.governs(root))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn patterns_follow_githubs_ruleset_examples() {
        for (pattern, text, want) in [
            ("refs/heads/main", "refs/heads/main", true),
            ("refs/heads/main", "refs/heads/mainline", false),
            ("refs/heads/release/*", "refs/heads/release/v1", true),
            (
                "refs/heads/release/*",
                "refs/heads/release/v1/hotfix",
                false,
            ),
            ("refs/heads/releases/**/*", "refs/heads/releases/v2", true),
            (
                "refs/heads/releases/**/*",
                "refs/heads/releases/v2/v2.1",
                true,
            ),
            ("refs/heads/**", "refs/heads/a/b/c", true),
            ("refs/heads/v?", "refs/heads/v1", true),
            ("refs/heads/v?", "refs/heads/v10", false),
            ("refs/heads/*-dev", "refs/heads/feature-dev", true),
        ] {
            assert_eq!(glob(pattern, text), want, "{pattern} against {text}");
        }
    }

    #[test]
    fn ref_name_conditions_follow_include_exclude_and_aliases() {
        let c = |v: Value| Some(json!({ "ref_name": v }));
        for (conditions, branch, want) in [
            (c(json!({"include": ["~DEFAULT_BRANCH"]})), "main", true),
            (c(json!({"include": ["~DEFAULT_BRANCH"]})), "dev", false),
            (
                c(json!({"include": ["~ALL"], "exclude": ["refs/heads/dev"]})),
                "dev",
                false,
            ),
            (
                c(json!({"include": ["~ALL"], "exclude": ["refs/heads/dev"]})),
                "x/y",
                true,
            ),
            (
                c(json!({"include": ["refs/heads/release/*"]})),
                "release/v1",
                true,
            ),
            (
                c(json!({"include": ["refs/heads/release/*"]})),
                "main",
                false,
            ),
            (c(json!({"include": []})), "main", false),
            (None, "main", false),
        ] {
            assert_eq!(
                ref_matches(conditions.as_ref(), branch, "main"),
                want,
                "{conditions:?} on {branch}"
            );
        }
    }
}
