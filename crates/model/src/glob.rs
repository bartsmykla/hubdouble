/// GitHub's ruleset patterns, Ruby's `File.fnmatch` with `FNM_PATHNAME` and without escapes:
/// `*` matches within one path segment, `?` one character other than `/`, `[...]` one
/// character other than `/` in the set (with `a-z` ranges, and `!` or `^` first to negate it),
/// `**/` at a segment boundary matches zero or more whole segments. A leading dot in a segment
/// must match a literal dot.
/// A `[` without its `]` matches nothing.
/// The cost is at most a small multiple of the pattern length times the text length, and the
/// stack depth is bounded whatever the pattern.
#[must_use]
pub fn glob(pattern: &str, text: &str) -> bool {
    matches(pattern, text, true)
}

/// The CODEOWNERS form of [`glob`], whose docs do not support `[ ]` ranges: a `[` is itself.
#[must_use]
pub fn codeowners_glob(pattern: &str, text: &str) -> bool {
    matches(pattern, text, false)
}

const MAX_DEPTH: usize = 64;

trait Unit: Copy + Ord + From<u8> {}

impl Unit for u8 {}

impl Unit for char {}

fn is<U: Unit>(unit: U, byte: u8) -> bool {
    unit == U::from(byte)
}

fn matches(pattern: &str, text: &str, sets: bool) -> bool {
    let literal = pattern
        .bytes()
        .take_while(|&b| !(b == b'*' || b == b'?' || (sets && b == b'[')))
        .count();
    let (head, pattern) = pattern.split_at(literal);
    let Some(text) = text.strip_prefix(head) else {
        return false;
    };
    let segment_start = head.is_empty() || head.ends_with('/');
    let leading_dot = sets && segment_start && text.starts_with('.');
    match pattern {
        "" => return text.is_empty(),
        "*" => return !leading_dot && !text.as_bytes().contains(&b'/'),
        "**" => return !sets || (!leading_dot && !text.as_bytes().contains(&b'/')),
        "**/*" if !sets => return true,
        _ => {}
    }
    let hidden = leading_dot || (sets && text.contains("/."));
    if segment_start && pattern == "**/*" {
        return !hidden;
    }
    if pattern.is_ascii() && text.is_ascii() {
        run(
            pattern.as_bytes(),
            text.as_bytes(),
            sets,
            segment_start,
            hidden,
        )
    } else {
        let pattern: Vec<char> = pattern.chars().collect();
        let text: Vec<char> = text.chars().collect();
        run(&pattern, &text, sets, segment_start, hidden)
    }
}

fn run<U: Unit>(pattern: &[U], text: &[U], sets: bool, segment_start: bool, hidden: bool) -> bool {
    if hidden {
        return tokens_match::<_, true>(pattern, text, sets, segment_start);
    }
    let mut budget = (pattern.len() + 1) * (text.len() + 1);
    backtrack(pattern, text, sets, &mut budget, 0, segment_start)
        .unwrap_or_else(|| tokens_match::<_, false>(pattern, text, sets, segment_start))
}

#[derive(Clone, Copy)]
enum Token<'p, U> {
    Unit(U),
    One,
    Set(&'p [U], bool),
    Never,
    InSegment,
    Anything,
    Segments,
}

fn next_token<U: Unit>(
    pattern: &[U],
    sets: bool,
    segment_start: bool,
) -> Option<(Token<'_, U>, usize)> {
    let &first = pattern.first()?;
    let at = |i: usize| pattern.get(i).copied();
    let star = |i: usize| at(i).is_some_and(|u| is(u, b'*'));
    Some(if is(first, b'*') {
        if !star(1) {
            (Token::InSegment, 1)
        } else if (!sets || segment_start) && at(2).is_some_and(|u| is(u, b'/')) {
            (Token::Segments, 3)
        } else if sets {
            let stars = pattern.iter().take_while(|&&u| is(u, b'*')).count();
            (Token::InSegment, stars)
        } else {
            (Token::Anything, 2)
        }
    } else if is(first, b'?') {
        (Token::One, 1)
    } else if sets && is(first, b'[') {
        set(pattern)
    } else {
        (Token::Unit(first), 1)
    })
}

fn set<U: Unit>(pattern: &[U]) -> (Token<'_, U>, usize) {
    let negated = pattern.get(1).is_some_and(|&u| is(u, b'!') || is(u, b'^'));
    let start = if negated { 2 } else { 1 };
    let mut i = start;
    loop {
        match pattern.get(i) {
            None => return (Token::Never, pattern.len()),
            Some(&u) if is(u, b']') => {
                let body = pattern.get(start..i).unwrap_or_default();
                return (Token::Set(body, negated), i + 1);
            }
            Some(_) => {
                let range = pattern.get(i + 1).is_some_and(|&u| is(u, b'-'))
                    && pattern.get(i + 2).is_some_and(|&u| !is(u, b']'));
                i += if range { 3 } else { 1 };
            }
        }
    }
}

fn in_set<U: Unit>(body: &[U], unit: U) -> bool {
    let mut i = 0;
    while let Some(&lo) = body.get(i) {
        match (body.get(i + 1), body.get(i + 2)) {
            (Some(&dash), Some(&hi)) if is(dash, b'-') => {
                if unit == lo || unit == hi || (lo <= unit && unit <= hi) {
                    return true;
                }
                i += 3;
            }
            _ => {
                if unit == lo {
                    return true;
                }
                i += 1;
            }
        }
    }
    false
}

fn fits<U: Unit>(token: Token<'_, U>, unit: U) -> bool {
    match token {
        Token::Unit(u) => unit == u,
        Token::One => !is(unit, b'/'),
        Token::Set(body, negated) => !is(unit, b'/') && in_set(body, unit) != negated,
        _ => false,
    }
}

fn backtrack<U: Unit>(
    mut pattern: &[U],
    mut text: &[U],
    sets: bool,
    budget: &mut usize,
    depth: usize,
    mut segment_start: bool,
) -> Option<bool> {
    if depth > MAX_DEPTH {
        return None;
    }
    loop {
        let Some((token, len)) = next_token(pattern, sets, segment_start) else {
            return Some(text.is_empty());
        };
        *budget = budget.checked_sub(len)?;
        let rest = pattern.get(len..).unwrap_or_default();
        let slash = |u: &U| is(*u, b'/');
        match token {
            Token::Anything if rest.is_empty() => return Some(true),
            Token::InSegment if rest.is_empty() => {
                *budget = budget.checked_sub(text.len())?;
                return Some(!text.iter().any(slash));
            }
            Token::Segments => {
                *budget = budget.checked_sub(text.len())?;
                if backtrack(rest, text, sets, budget, depth + 1, true)? {
                    return Some(true);
                }
                for (at, u) in text.iter().enumerate() {
                    let after = text.get(at + 1..).unwrap_or_default();
                    if slash(u) && backtrack(rest, after, sets, budget, depth + 1, true)? {
                        return Some(true);
                    }
                }
                return Some(false);
            }
            Token::Anything | Token::InSegment => {
                let end = match token {
                    Token::Anything => text.len(),
                    _ => text.iter().position(slash).unwrap_or(text.len()),
                };
                for at in 0..=end {
                    let after = text.get(at..).unwrap_or_default();
                    if backtrack(rest, after, sets, budget, depth + 1, false)? {
                        return Some(true);
                    }
                }
                return Some(false);
            }
            fixed => match text.split_first() {
                Some((&u, after)) if fits(fixed, u) => {
                    (pattern, text) = (rest, after);
                    segment_start = matches!(fixed, Token::Unit(v) if is(v, b'/'));
                }
                _ => return Some(false),
            },
        }
    }
}

fn leading_dot_at<U: Unit>(text: &[U], at: usize, segment_start: bool) -> bool {
    text.get(at).is_some_and(|&u| is(u, b'.'))
        && at
            .checked_sub(1)
            .map_or(segment_start, |i| is(text[i], b'/'))
}

fn tokens_match<U: Unit, const HIDDEN: bool>(
    pattern: &[U],
    text: &[U],
    sets: bool,
    segment_start: bool,
) -> bool {
    let mut tokens = Vec::new();
    let mut rest = pattern;
    let mut boundary = segment_start;
    while let Some((token, len)) = next_token(rest, sets, boundary) {
        boundary =
            matches!(token, Token::Segments) || matches!(token, Token::Unit(v) if is(v, b'/'));
        tokens.push(token);
        rest = rest.get(len..).unwrap_or_default();
    }
    let mut tokens = &tokens[..];
    let mut text = text;
    while let Some((&last, before)) = tokens.split_last() {
        if matches!(last, Token::InSegment | Token::Anything | Token::Segments) {
            return table::<_, HIDDEN>(tokens, text, segment_start);
        }
        let Some((&u, rest)) = text.split_last() else {
            return false;
        };
        if !fits(last, u)
            || (HIDDEN
                && !matches!(last, Token::Unit(_))
                && leading_dot_at(text, rest.len(), segment_start))
        {
            return false;
        }
        (tokens, text) = (before, rest);
    }
    text.is_empty()
}

fn table<U: Unit, const HIDDEN: bool>(
    tokens: &[Token<'_, U>],
    text: &[U],
    segment_start: bool,
) -> bool {
    let n = text.len();
    let mut later = vec![false; n + 1];
    let mut row = vec![false; n + 1];
    later[n] = true;
    for &token in tokens.iter().rev() {
        let mut slash_ahead = false;
        for j in (0..=n).rev() {
            let u = text.get(j).copied();
            let in_segment = u.is_some_and(|u| !is(u, b'/'));
            let next = |r: &[bool]| r.get(j + 1).copied().unwrap_or(false);
            let hidden = HIDDEN && leading_dot_at(text, j, segment_start);
            row[j] = match token {
                Token::InSegment => !hidden && (later[j] || (in_segment && next(&row))),
                Token::Anything => later[j] || (u.is_some() && next(&row)),
                Token::Segments => {
                    slash_ahead = !hidden
                        && (slash_ahead || (u.is_some_and(|u| is(u, b'/')) && next(&later)));
                    later[j] || slash_ahead
                }
                Token::One | Token::Set(_, _) if hidden => false,
                fixed => u.is_some_and(|u| fits(fixed, u)) && next(&later),
            };
        }
        std::mem::swap(&mut row, &mut later);
    }
    later[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_ranges_and_negation_follow_rubys_fnmatch() {
        for (pattern, text, want) in [
            (
                "refs/heads/v[2-9][13579].x-staging",
                "refs/heads/v25.x-staging",
                true,
            ),
            (
                "refs/heads/v[2-9][13579].x-staging",
                "refs/heads/v24.x-staging",
                false,
            ),
            (
                "refs/heads/v[2-9][13579].x-staging",
                "refs/heads/v15.x-staging",
                false,
            ),
            ("[!a]", "b", true),
            ("[!a]", "a", false),
            ("[^a]", "a", false),
            ("[a-]", "-", true),
            ("[z-a]", "z", true),
            ("[z-a]", "m", false),
            ("a[/]b", "a/b", false),
            ("v[1", "v[1", false),
            ("[]a]", "]a]", false),
            ("*[0-9]", "release-7", true),
        ] {
            assert_eq!(glob(pattern, text), want, "{pattern} against {text}");
        }
        assert!(codeowners_glob("docs/[1]", "docs/[1]"));
        assert!(!codeowners_glob("docs/[1]", "docs/1"));
    }

    #[test]
    fn only_a_literal_dot_matches_the_start_of_a_hidden_component() {
        for (pattern, text, expected) in [
            ("src/**", "src/.env", false),
            ("src/*", "src/.env", false),
            ("src/**/*", "src/pkg/.cache/data", false),
            ("src/**/.env", "src/pkg/.env", true),
            ("src/**/.env", "src/.cache/.env", false),
            ("**/.config/*", ".config/é", true),
            ("**/main", ".config/main", false),
            ("src/*.env", "src/.env", false),
            ("**/?env", "pkg/.env", false),
            ("**/[.]env", "pkg/.env", false),
            ("src*/**/?.txt", "src/.cache/a.txt", false),
            ("src/v*", "src/v.1", true),
        ] {
            assert_eq!(glob(pattern, text), expected, "{pattern} against {text}");
        }
        assert!(codeowners_glob("src/**/*", "src/.cache/data"));
    }

    #[test]
    fn ruleset_globs_match_unicode_text() {
        for (pattern, text, want) in [
            ("refs/heads/v?", "refs/heads/vé", true),
            ("refs/heads/v??", "refs/heads/vé", false),
            ("refs/heads/[éa]", "refs/heads/é", true),
            ("refs/heads/[a-z]", "refs/heads/é", false),
            ("*é", "café", true),
            ("refs/heads/*", "refs/heads/vé", true),
            ("refs/heads/*", "refs/heads/vé/hotfix", false),
            ("refs/heads/**/*", "refs/heads/vé/hotfix", true),
        ] {
            assert_eq!(glob(pattern, text), want, "{pattern} against {text}");
        }
    }

    #[test]
    fn a_pattern_of_a_million_segments_stays_on_a_small_stack() {
        // Given patterns of 100000 and 1000000 `**/` that a recursive match would nest into
        let small_stack = std::thread::Builder::new().stack_size(256 * 1024);
        // When they meet a short text on a thread with a 256 KiB stack
        let answers = small_stack
            .spawn(|| {
                (
                    glob(&format!("{}x", "**/".repeat(1_000_000)), "a/b"),
                    glob(&format!("{}main.rs", "**/".repeat(100_000)), "main.rs"),
                    codeowners_glob(&format!("{}x", "**/".repeat(1_000_000)), "a/x"),
                )
            })
            .unwrap()
            .join()
            .unwrap();
        // Then they answer without overflowing it
        assert_eq!(answers, (false, true, true));
    }
}
