use std::fmt::Display;

use hubdouble_auth::repo_role;
use hubdouble_git::{Git, GitError, ObjectData, ObjectId};
use hubdouble_model::{AccountKind, ApiError, Repo, RepoRole, Team, TeamPrivacy, glob};
use hubdouble_store::{Accounts, Teams, Tx};
use serde::Serialize;

/// Where GitHub looks for a CODEOWNERS file, in order; the first one found is used.
pub const LOCATIONS: [&str; 3] = [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"];

/// GitHub loads a CODEOWNERS file only when it is under 3 MB, taken as 3 MiB.
pub const MAX_FILE_BYTES: usize = 3 * 1024 * 1024;

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
        let mut tokens = tokens(rule_of(raw)?);
        let pattern = tokens.next()?;
        Some(Line {
            number: i + 1,
            raw,
            pattern,
            owners: tokens.collect(),
        })
    })
}

/// The rules of [`lines`] as byte ranges into `text`: the line number, the line as written and
/// the pattern, without reading the owners.
pub fn spans(
    text: &str,
) -> impl Iterator<Item = (usize, std::ops::Range<usize>, std::ops::Range<usize>)> {
    let mut start = 0;
    text.split_inclusive('\n')
        .enumerate()
        .filter_map(move |(i, chunk)| {
            let at = start;
            start += chunk.len();
            let raw = chunk
                .strip_suffix('\n')
                .map_or(chunk, |r| r.strip_suffix('\r').unwrap_or(r));
            let (column, pattern) = tokens(rule_of(raw)?).next()?;
            let from = at + column - 1;
            Some((i + 1, at..at + raw.len(), from..from + pattern.len()))
        })
}

fn rule_of(raw: &str) -> Option<&str> {
    let line = raw
        .as_bytes()
        .windows(2)
        .position(|pair| pair == b" #")
        .and_then(|end| raw.get(..end))
        .unwrap_or(raw);
    (!line.trim_start().starts_with('#')).then_some(line)
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

/// The paths that a pattern can cover, for an index of rules: [`covers`] is true only when
/// the path starts with the segments of [`Reach::Root`], the literal segments that lead the
/// pattern, or for [`Reach::Name`] when one of its segments is the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach<'p> {
    Root(&'p str),
    Name(&'p str),
    Any,
}

/// The [`Reach`] of a pattern.
#[must_use]
pub fn reach(pattern: &str) -> Reach<'_> {
    let trimmed = pattern.trim_end_matches('/');
    let anchored = trimmed.starts_with('/') || trimmed.contains('/');
    let p = trimmed.trim_start_matches('/');
    let literal = |s: &str| !s.is_empty() && !s.contains(['*', '?']);
    if !anchored {
        return if literal(p) {
            Reach::Name(p)
        } else {
            Reach::Any
        };
    }
    let end = p
        .split('/')
        .take_while(|s| literal(s))
        .map(|s| s.len() + 1)
        .sum::<usize>();
    match p.get(..end.saturating_sub(1)) {
        Some(prefix) if !prefix.is_empty() => Reach::Root(prefix),
        _ => Reach::Any,
    }
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

/// The CODEOWNERS file at a commit or ref: its location and text. `None` when there is none,
/// or when the first one found is not under [`MAX_FILE_BYTES`], which GitHub does not load.
///
/// # Errors
///
/// A git error other than a missing file.
pub fn file_at(
    tx: &Tx,
    git: &Git,
    repo: i64,
    at: impl Display,
) -> Result<Option<(&'static str, String)>, ApiError> {
    blob_at(tx, git, repo, at)?
        .map(|(path, blob)| Ok((path, text_of(tx, git, repo, blob)?)))
        .transpose()
}

/// The location and blob of the CODEOWNERS file that [`file_at`] reads.
///
/// # Errors
///
/// A git error other than a missing file.
pub fn blob_at(
    tx: &Tx,
    git: &Git,
    repo: i64,
    at: impl Display,
) -> Result<Option<(&'static str, ObjectId)>, ApiError> {
    for path in LOCATIONS {
        match git.expression(tx, repo, &format!("{at}:{path}")) {
            Ok(o) => {
                if let ObjectData::Blob(data) = &o.data {
                    return Ok((data.len() < MAX_FILE_BYTES).then_some((path, o.id)));
                }
            }
            Err(GitError::NotFound | GitError::WrongKind) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}

/// The text of a CODEOWNERS blob that [`blob_at`] found.
///
/// # Errors
///
/// A git error, also when `blob` is not a blob.
pub fn text_of(tx: &Tx, git: &Git, repo: i64, blob: ObjectId) -> Result<String, ApiError> {
    match git.object(tx, repo, blob)?.data {
        ObjectData::Blob(data) => Ok(String::from_utf8_lossy(&data).into_owned()),
        _ => Err(GitError::WrongKind.into()),
    }
}

/// A team that may own files of `repo`, as GitHub's docs ask: a visible team of `org` with
/// write access to the repository.
#[must_use]
pub fn owning_team<'t>(tx: &'t Tx, repo: &Repo, org: &str, slug: &str) -> Option<&'t Team> {
    let org = tx.table::<Accounts>().by_login(org)?;
    tx.table::<Teams>()
        .by_slug(org.id, slug)
        .map(AsRef::as_ref)
        .filter(|t| {
            t.privacy != TeamPrivacy::Secret
                && t.repos
                    .iter()
                    .any(|r| r.repo_id == repo.id && r.role >= RepoRole::Write)
        })
}

/// One entry of `repos/codeowners-errors`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnersError {
    pub line: usize,
    pub column: usize,
    pub source: String,
    pub kind: &'static str,
    pub suggestion: Option<String>,
    pub message: String,
    pub path: String,
}

/// Who may own files, for [`check`] and [`valid_lines`].
pub trait Owners {
    fn user_can_write(&self, login: &str) -> bool;
    fn team_can_write(&self, org: &str, slug: &str) -> bool;
    fn email_can_write(&self, email: &str) -> bool;
}

/// The owners of a repository as the store holds them.
pub struct RepoOwners<'a> {
    pub tx: &'a Tx,
    pub repo: &'a Repo,
}

impl RepoOwners<'_> {
    fn writes(&self, user_id: i64) -> bool {
        repo_role(self.tx, user_id, self.repo) >= Some(RepoRole::Write)
    }
}

impl Owners for RepoOwners<'_> {
    fn user_can_write(&self, login: &str) -> bool {
        self.tx
            .table::<Accounts>()
            .by_login(login)
            .is_some_and(|a| a.kind != AccountKind::Organization && self.writes(a.id))
    }

    fn team_can_write(&self, org: &str, slug: &str) -> bool {
        owning_team(self.tx, self.repo, org, slug).is_some()
    }

    fn email_can_write(&self, email: &str) -> bool {
        self.tx.table::<Accounts>().iter().any(|a| {
            a.email
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case(email))
                && self.writes(a.id)
        })
    }
}

fn error(path: &str, line: usize, column: usize, source: &str, kind: &'static str) -> OwnersError {
    let message = format!(
        "{kind} on line {line}:\n\n  {source}\n  {}^",
        " ".repeat(column - 1)
    );
    OwnersError {
        line,
        column,
        source: source.to_owned(),
        kind,
        suggestion: None,
        message,
        path: path.to_owned(),
    }
}

fn owner_kind(owner: &str, owners: &impl Owners) -> Option<&'static str> {
    if let Some(name) = owner.strip_prefix('@') {
        let known = match name.split_once('/') {
            Some((org, slug)) => owners.team_can_write(org, slug),
            None => owners.user_can_write(name),
        };
        return (!known).then_some("Unknown owner");
    }
    if owner.contains('@') {
        return (!owners.email_can_write(owner)).then_some("Unknown owner");
    }
    Some("Invalid owner")
}

fn problems(line: &Line<'_>, owners: &impl Owners) -> Vec<(usize, &'static str)> {
    let (column, pattern) = line.pattern;
    if pattern.starts_with('!') || pattern.contains('[') || pattern.contains(']') {
        return vec![(column, "Invalid pattern")];
    }
    line.owners
        .iter()
        .filter_map(|&(column, owner)| owner_kind(owner, owners).map(|kind| (column, kind)))
        .collect()
}

/// The errors of the CODEOWNERS file at `path`, as `repos/codeowners-errors` lists them.
#[must_use]
pub fn check(path: &str, text: &str, owners: &impl Owners) -> Vec<OwnersError> {
    let mut out = Vec::new();
    for line in lines(text) {
        for (column, kind) in problems(&line, owners) {
            out.push(error(path, line.number, column, line.raw, kind));
        }
    }
    out
}

/// The rules that GitHub applies: every rule except the lines that [`check`] reports, as
/// GitHub skips a line with an error.
#[must_use]
pub fn valid_lines<'t>(text: &'t str, owners: &impl Owners) -> Vec<Line<'t>> {
    lines(text).filter(|line| line.is_valid(owners)).collect()
}

impl Line<'_> {
    /// Whether GitHub applies this rule: [`check`] reports nothing on its line.
    #[must_use]
    pub fn is_valid(&self, owners: &impl Owners) -> bool {
        problems(self, owners).is_empty()
    }
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
    fn spans_name_the_lines_and_patterns_of_the_rules() {
        // Given rules with comments, a blank line, CRLF ends and no final newline
        let text = "# owners\r\n*.js @a # web\r\n\r\n  /docs/  @b\n#x\nlogs @c";
        // When its spans are read
        let read: Vec<(usize, &str, &str)> = spans(text)
            .map(|(n, raw, pattern)| (n, &text[raw], &text[pattern]))
            .collect();
        // Then they are the rules of lines, by number, line and pattern
        let expected: Vec<(usize, &str, &str)> = lines(text)
            .map(|l| (l.number, l.raw, l.pattern.1))
            .collect();
        assert_eq!(read, expected);
        assert_eq!(
            read,
            [
                (2, "*.js @a # web", "*.js"),
                (4, "  /docs/  @b", "/docs/"),
                (6, "logs @c", "logs")
            ]
        );
    }

    #[test]
    fn a_pattern_covers_only_paths_in_its_reach() {
        // Given patterns of each reach and paths at the root, deeper and with the name inside
        let patterns = [
            "*",
            "*.js",
            "/build/logs/",
            "docs/*",
            "apps/",
            "apps",
            "/docs/",
            "**/logs",
            "/a?c/",
            "/scripts/ci.sh",
            "src/apps/main.go",
            "/",
            "logs",
        ];
        let paths = [
            "apps",
            "apps/x",
            "src/apps/main.go",
            "docs/a.md",
            "docs",
            "build/logs/y.log",
            "deep/logs/x.log",
            "logs",
            "abc/d",
            "scripts/ci.sh",
            "other/scripts/ci.sh",
            "x.js",
            "apps.js",
            "src/apps",
        ];
        // When each pattern meets each path
        for pattern in patterns {
            let reach = reach(pattern);
            for path in paths {
                let segments: Vec<&str> = path.split('/').collect();
                let admitted = match reach {
                    Reach::Root(prefix) => {
                        let literal: Vec<&str> = prefix.split('/').collect();
                        segments.starts_with(&literal)
                    }
                    Reach::Name(name) => segments.contains(&name),
                    Reach::Any => true,
                };
                // Then a covered path is in the reach of the pattern
                assert!(
                    !covers(pattern, path) || admitted,
                    "{pattern} against {path}"
                );
            }
        }
        assert_eq!(reach("/build/logs/"), Reach::Root("build/logs"));
        assert_eq!(reach("docs/*"), Reach::Root("docs"));
        assert_eq!(reach("/src/a*/b"), Reach::Root("src"));
        assert_eq!(reach("apps/"), Reach::Name("apps"));
        assert_eq!(reach("*.js"), Reach::Any);
        assert_eq!(reach("/a?c/"), Reach::Any);
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

    struct Known;

    impl Owners for Known {
        fn user_can_write(&self, login: &str) -> bool {
            login == "alice"
        }
        fn team_can_write(&self, org: &str, slug: &str) -> bool {
            (org, slug) == ("acme", "core")
        }
        fn email_can_write(&self, email: &str) -> bool {
            email == "alice@example.com"
        }
    }

    #[test]
    fn each_bad_owner_and_pattern_is_reported_with_its_line_and_column() {
        // Given
        let text = "# owners\n*.md @alice @nobody\n\n/docs/ @acme/core @acme/ghosts\n!x.txt @alice\nsrc/ alice alice@example.com bob@example.com # trailing\n* @alice\n";
        // When
        let errors = check(".github/CODEOWNERS", text, &Known);
        let applied: Vec<usize> = valid_lines(text, &Known).iter().map(|l| l.number).collect();
        // Then
        let got: Vec<(usize, usize, &str)> =
            errors.iter().map(|e| (e.line, e.column, e.kind)).collect();
        assert_eq!(
            got,
            [
                (2, 13, "Unknown owner"),
                (4, 19, "Unknown owner"),
                (5, 1, "Invalid pattern"),
                (6, 6, "Invalid owner"),
                (6, 30, "Unknown owner"),
            ]
        );
        assert!(errors.iter().all(|e| e.path == ".github/CODEOWNERS"));
        assert_eq!(errors[0].source, "*.md @alice @nobody");
        assert_eq!(applied, [7]);
    }

    #[test]
    fn a_clean_file_has_no_errors() {
        assert_eq!(check("CODEOWNERS", "* @alice\n", &Known), Vec::new());
    }
}
