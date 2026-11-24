use hubdouble_git::{Git, GitError, ObjectData, ObjectId};
use hubdouble_model::{ApiError, glob};
use hubdouble_store::Tx;

/// Where GitHub looks for a CODEOWNERS file, in order; the first one found is used.
pub const LOCATIONS: [&str; 3] = [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"];

/// One rule of a CODEOWNERS file: a pattern and its owners, each token with its 1-based column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line<'t> {
    /// The 1-based line number.
    pub number: usize,
    /// The line as written, comment included.
    pub raw: &'t str,
    pub pattern: (usize, &'t str),
    pub owners: Vec<(usize, &'t str)>,
}

/// The rules of a CODEOWNERS file, without comments and blank lines. A `#` at the start of a
/// line or after a space starts a comment.
pub fn lines(text: &str) -> impl Iterator<Item = Line<'_>> {
    text.lines().enumerate().filter_map(|(i, raw)| {
        let line = raw.split_once(" #").map_or(raw, |(l, _)| l);
        if line.trim_start().starts_with('#') {
            return None;
        }
        let mut tokens = tokens(line);
        let pattern = tokens.next()?;
        Some(Line {
            number: i + 1,
            raw,
            pattern,
            owners: tokens.collect(),
        })
    })
}

fn tokens(line: &str) -> impl Iterator<Item = (usize, &str)> {
    line.split(' ')
        .scan(1, |column, token| {
            let at = *column;
            *column += token.len() + 1;
            Some((at, token))
        })
        .filter(|(_, t)| !t.is_empty())
}

/// Whether a CODEOWNERS pattern covers `path`, as GitHub documents the gitignore rules it
/// follows: a pattern without a slash matches a file or directory name at any depth, a pattern
/// with a leading or inner slash matches from the root, a trailing slash matches only a
/// directory, and a pattern that names a directory covers every file below it unless its last
/// part holds a single `*` (`docs/*` owns `docs/a.md` but not `docs/b/c.md`).
#[must_use]
pub fn covers(pattern: &str, path: &str) -> bool {
    let dir_only = pattern.ends_with('/');
    let trimmed = pattern.trim_end_matches('/');
    let anchored = trimmed.starts_with('/') || trimmed.contains('/');
    let p = trimmed.trim_start_matches('/');
    if p.is_empty() {
        return false;
    }
    let last = p.rsplit('/').next().unwrap_or(p);
    let into_dirs = last == "**" || !last.contains('*');
    let segments: Vec<&str> = path.split('/').collect();
    let mut prefix = String::with_capacity(path.len());
    for (i, segment) in segments.iter().enumerate() {
        if i > 0 {
            prefix.push('/');
        }
        prefix.push_str(segment);
        let is_file = i + 1 == segments.len();
        if (is_file && dir_only) || (!is_file && !into_dirs) {
            continue;
        }
        let hit = if anchored {
            glob(p, &prefix)
        } else {
            glob(p, segment)
        };
        if hit {
            return true;
        }
    }
    false
}

/// The owners of `path`: those of the last rule whose pattern covers it. `None` when no rule
/// covers it; an empty list when the last such rule names no owner.
#[must_use]
pub fn owners_of<'l, 't>(rules: &'l [Line<'t>], path: &str) -> Option<&'l [(usize, &'t str)]> {
    rules
        .iter()
        .rev()
        .find(|l| covers(l.pattern.1, path))
        .map(|l| l.owners.as_slice())
}

/// The CODEOWNERS file at a commit: its location and text.
///
/// # Errors
///
/// A git error other than a missing file.
pub fn file_at(
    tx: &Tx,
    git: &Git,
    repo: i64,
    at: ObjectId,
) -> Result<Option<(&'static str, String)>, ApiError> {
    for path in LOCATIONS {
        match git.expression(tx, repo, &format!("{at}:{path}")) {
            Ok(o) => {
                if let ObjectData::Blob(data) = o.data {
                    return Ok(Some((path, String::from_utf8_lossy(&data).into_owned())));
                }
            }
            Err(GitError::NotFound | GitError::WrongKind) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_follow_githubs_codeowners_examples() {
        for (pattern, path, want) in [
            ("*", "a/b/c.txt", true),
            ("*.js", "src/app/index.js", true),
            ("*.js", "index.ts", false),
            ("/build/logs/", "build/logs/x/y.log", true),
            ("/build/logs/", "src/build/logs/y.log", false),
            ("docs/*", "docs/getting-started.md", true),
            ("docs/*", "docs/build-app/troubleshooting.md", false),
            ("apps/", "src/apps/main.go", true),
            ("apps/", "apps", false),
            ("/docs/", "docs/a/b.md", true),
            ("**/logs", "deep/logs/x.log", true),
            ("/scripts/ci.sh", "scripts/ci.sh", true),
            ("/scripts/ci.sh", "other/scripts/ci.sh", false),
        ] {
            assert_eq!(covers(pattern, path), want, "{pattern} against {path}");
        }
    }

    #[test]
    fn the_last_matching_rule_wins_and_may_drop_the_owners() {
        // Given
        let text = "# owners\n* @acme/core\n*.md @alice @bob # docs\n/docs/private.md\n";
        let rules: Vec<_> = lines(text).collect();
        // When
        let any = owners_of(&rules, "src/main.rs");
        let docs = owners_of(&rules, "README.md");
        let private = owners_of(&rules, "docs/private.md");
        // Then
        assert_eq!(any.map(<[_]>::len), Some(1));
        assert_eq!(
            docs.map(|o| o.iter().map(|(_, t)| *t).collect::<Vec<_>>()),
            Some(vec!["@alice", "@bob"])
        );
        assert_eq!(private, Some(&[][..]));
    }
}
