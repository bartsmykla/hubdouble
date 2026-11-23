use hubdouble_admin::Registrar;
use hubdouble_auth::repo_role;
use hubdouble_git::{GitError, ObjectData};
use hubdouble_model::{AccountKind, ApiError, Repo, RepoRole};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Accounts, Teams, Tx};
use serde::Serialize;

use crate::CONTENTS;

const LOCATIONS: [&str; 3] = [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"];

pub(crate) fn register(r: &mut Registrar<'_>) {
    r.rest.handle("repos/codeowners-errors", CONTENTS, errors);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OwnersError {
    pub(crate) line: usize,
    pub(crate) column: usize,
    pub(crate) source: String,
    pub(crate) kind: &'static str,
    pub(crate) suggestion: Option<String>,
    pub(crate) message: String,
    pub(crate) path: String,
}

#[derive(Serialize)]
struct Errors {
    errors: Vec<OwnersError>,
}

pub(crate) trait Owners {
    fn user_can_write(&self, login: &str) -> bool;
    fn team_can_write(&self, org: &str, slug: &str) -> bool;
    fn email_can_write(&self, email: &str) -> bool;
}

struct RepoOwners<'a> {
    tx: &'a Tx,
    repo: &'a Repo,
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
        let Some(org) = self.tx.table::<Accounts>().by_login(org) else {
            return false;
        };
        self.tx
            .table::<Teams>()
            .by_slug(org.id, slug)
            .is_some_and(|t| {
                t.repos
                    .iter()
                    .any(|r| r.repo_id == self.repo.id && r.role >= RepoRole::Write)
            })
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

pub(crate) fn check(path: &str, text: &str, owners: &impl Owners) -> Vec<OwnersError> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.split_once(" #").map_or(raw, |(l, _)| l);
        if line.trim_start().starts_with('#') {
            continue;
        }
        let mut tokens = tokens(line);
        let Some((column, pattern)) = tokens.next() else {
            continue;
        };
        if pattern.starts_with('!') || pattern.contains('[') || pattern.contains(']') {
            out.push(error(path, i + 1, column, raw, "Invalid pattern"));
            continue;
        }
        for (column, owner) in tokens {
            if let Some(kind) = owner_kind(owner, owners) {
                out.push(error(path, i + 1, column, raw, kind));
            }
        }
    }
    out
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

fn errors(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?;
    let tx = ctx.tx();
    let git = ctx.git();
    let reference = ctx.query().get("ref").unwrap_or(&repo.default_branch);
    for path in LOCATIONS {
        let text = match git.expression(tx, repo.id, &format!("{reference}:{path}")) {
            Ok(o) => match o.data {
                ObjectData::Blob(data) => String::from_utf8_lossy(&data).into_owned(),
                _ => continue,
            },
            Err(GitError::NotFound) => continue,
            Err(e) => return Err(e.into()),
        };
        let errors = check(path, &text, &RepoOwners { tx, repo });
        return Reply::json(&Errors { errors });
    }
    Err(ApiError::not_found())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let text = "# owners\n*.md @alice @nobody\n\n/docs/ @acme/core @acme/ghosts\n!x.txt @alice\nsrc/ alice alice@example.com bob@example.com # trailing\n";
        // When
        let errors = check(".github/CODEOWNERS", text, &Known);
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
    }

    #[test]
    fn a_clean_file_has_no_errors() {
        assert_eq!(check("CODEOWNERS", "* @alice\n", &Known), Vec::new());
    }
}
