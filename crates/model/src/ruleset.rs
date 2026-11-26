use std::collections::BTreeMap;

use serde_json::Value;

use crate::glob;
use crate::{Enforcement, Repo, Ruleset, RulesetSource, RulesetTarget};

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

pub type RepositoryPropertyValues = BTreeMap<String, Vec<String>>;

#[derive(Clone, Copy)]
pub struct RulesetRepository<'a> {
    pub repo: &'a Repo,
    pub custom_properties: Option<&'a RepositoryPropertyValues>,
}

fn repo_matches(conditions: Option<&Value>, target: RulesetRepository<'_>) -> bool {
    let Some(c) = conditions else {
        return false;
    };
    if let Some(names) = c.get("repository_name") {
        return admits(names, &target.repo.name, |p| (p == "~ALL").then_some(true));
    }
    if let Some(properties) = c.get("repository_property") {
        return properties_match(properties, target);
    }
    c.pointer("/repository_id/repository_ids")
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|id| id.as_i64() == Some(target.repo.id)))
}

fn properties_match(condition: &Value, target: RulesetRepository<'_>) -> bool {
    let entries = |key| {
        condition
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    };
    let matches = |property: &Value| {
        let Some(name) = property.get("name").and_then(Value::as_str) else {
            return false;
        };
        let Some(values) = property.get("property_values").and_then(Value::as_array) else {
            return false;
        };
        match property
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("custom")
        {
            "custom" => target
                .custom_properties
                .and_then(|properties| properties.get(name))
                .is_some_and(|actual| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|value| actual.iter().any(|a| a == value))
                }),
            "system" => {
                let actual = match name {
                    "visibility" => Some(target.repo.visibility.as_str()),
                    "language" => target.repo.language.as_deref(),
                    _ => None,
                };
                actual.is_some_and(|actual| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|v| v.eq_ignore_ascii_case(actual))
                })
            }
            _ => false,
        }
    };
    entries("include").all(matches) && !entries("exclude").any(matches)
}

impl Ruleset {
    /// Whether the ruleset governs the repository: its own ruleset, or one of its owner's whose
    /// repository condition admits it.
    #[must_use]
    pub fn governs(&self, target: RulesetRepository<'_>) -> bool {
        match self.source_type {
            RulesetSource::Repository => self.repo_id == Some(target.repo.id),
            RulesetSource::Organization => {
                self.source_id == target.repo.owner_id
                    && repo_matches(self.conditions.as_ref(), target)
            }
        }
    }

    /// Whether the ruleset is an active branch ruleset that applies to `branch` of the repository.
    #[must_use]
    pub fn applies_to_branch(&self, target: RulesetRepository<'_>, branch: &str) -> bool {
        self.enforcement == Enforcement::Active
            && self.target == RulesetTarget::Branch
            && self.governs(target)
            && ref_matches(
                self.conditions.as_ref(),
                branch,
                &target.repo.default_branch,
            )
    }

    /// Whether the ruleset is an active tag ruleset that applies to `tag` of the repository.
    #[must_use]
    pub fn applies_to_tag(&self, target: RulesetRepository<'_>, tag: &str) -> bool {
        self.enforcement == Enforcement::Active
            && self.target == RulesetTarget::Tag
            && self.governs(target)
            && tag_matches(self.conditions.as_ref(), tag)
    }

    /// Whether the ruleset is an active repository ruleset of the repository's organization
    /// that governs it. GitHub lists its rules among the rules of every branch.
    #[must_use]
    pub fn applies_to_repository(&self, target: RulesetRepository<'_>) -> bool {
        self.enforcement == Enforcement::Active
            && self.target == RulesetTarget::Repository
            && self.governs(target)
    }

    /// Whether the ruleset is an active push ruleset of the repository or of `root`, the
    /// repository at the root of its fork network: GitHub applies a push ruleset to the whole
    /// network.
    #[must_use]
    pub fn applies_to_push(
        &self,
        repo: RulesetRepository<'_>,
        root: RulesetRepository<'_>,
    ) -> bool {
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
            ("refs/heads/**", "refs/heads/a/b/c", false),
            ("refs/heads/**", "refs/heads/a", true),
            ("refs/heads/v?", "refs/heads/v1", true),
            ("refs/heads/v?", "refs/heads/v10", false),
            ("refs/heads/*-dev", "refs/heads/feature-dev", true),
            ("refs/heads/**/*", "refs/heads/a", true),
            ("refs/heads/**/x", "refs/heads/x", true),
            ("refs/**/heads/x", "refs/heads/x", true),
            ("refs/**/x", "refs/a/b/x", true),
            ("refs/**/x", "refs/ax", false),
            ("refs/**x", "refs/a/bx", false),
            ("refs/heads/qa**/**/*", "refs/heads/qa", false),
            ("refs/heads/qa**/**/*", "refs/heads/qafoo/nested", true),
            ("refs/heads/foo**/bar", "refs/heads/foox/dir/bar", false),
            ("refs/heads/***/*", "refs/heads/a/b/c", false),
            ("*", "a/b", false),
            ("a*", "a", true),
            ("?", "/", false),
            ("", "", true),
            ("", "a", false),
            ("a/**/", "a/", true),
            ("a/**/", "a/x/", true),
            ("a/**/", "a/x", false),
            ("*.md?", "a.mdx", true),
            ("*.md?", "a/b.mdx", false),
            ("*b?c", "aXbYc", true),
            ("*b?c", "aXb/c", false),
            ("*?", "", false),
        ] {
            assert_eq!(glob(pattern, text), want, "{pattern} against {text}");
        }
        let long_pattern = format!("{}*", "a?".repeat(20));
        let long_text = format!("{}tail", "ab".repeat(80));
        assert!(glob(&long_pattern, &long_text));
        assert!(!glob(&long_pattern, &format!("a/{long_text}")));
    }

    #[test]
    fn a_pattern_that_backtracks_on_every_star_still_gets_its_answer() {
        let stars = format!("{}b", "a*".repeat(15));
        let deep = format!("{}b", "**/".repeat(7));
        let stars_inside = format!("{}c*b", "a*".repeat(15));
        let deep_inside = format!("{}c/*b", "**/".repeat(7));
        let a = "a".repeat(100);
        for (pattern, text, want) in [
            (stars.clone(), "a".repeat(200), false),
            (stars, format!("{}b", "a".repeat(200)), true),
            (deep.clone(), "a/".repeat(100), false),
            (deep, format!("{}b", "a/".repeat(100)), true),
            (stars_inside.clone(), format!("{a}{a}b"), false),
            (stars_inside, format!("{a}c{a}b"), true),
            (
                deep_inside.clone(),
                format!("{}ab", "a/".repeat(100)),
                false,
            ),
            (deep_inside, format!("{}c/ab", "a/".repeat(100)), true),
        ] {
            // Given a pattern whose stars a backtracking match would retry at each position
            // When it meets a long text
            // Then it answers as the glob rules say
            assert_eq!(glob(&pattern, &text), want, "{}", &pattern[..4]);
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
