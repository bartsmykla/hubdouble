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
            ("refs/heads/**/*", "refs/heads/a", true),
            ("refs/heads/**/x", "refs/heads/x", true),
            ("refs/**/heads/x", "refs/heads/x", true),
            ("refs/**/x", "refs/a/b/x", true),
            ("refs/**/x", "refs/ax", false),
            ("refs/**x", "refs/a/bx", true),
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
