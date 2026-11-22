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

#[cfg(test)]
mod tests {
    use super::glob;

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
}
