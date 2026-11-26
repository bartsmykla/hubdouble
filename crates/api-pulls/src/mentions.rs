use hubdouble_auth::repo_role;
use hubdouble_model::{
    AccountKind, ApiError, Issue, MembershipState, OrgRole, Repo, Team, TeamPrivacy, Timestamp,
    Visibility,
};
use hubdouble_store::{Accounts, Memberships, Repos, Teams, Tx, TxMut};

use crate::timeline::{self, StateKind};

const MAX_LOGIN: usize = 39;

/// Records the `mentioned` and `subscribed` issue events of the `@` mentions that `after`
/// adds to `before`, in the order they appear, as live GitHub records them: each text that
/// mentions a user gives a `mentioned` and then a `subscribed`, again for a user that an
/// earlier text mentioned, and each member of a mentioned team, its child teams included, only
/// a `subscribed`. A user who cannot read the repository gets none. `author` mentions a team
/// only as a member of its organization, and a secret team only as one of its members or an
/// owner. A mention in GitHub Flavored Markdown code does not count.
///
/// # Errors
///
/// A store error.
pub fn record_mentions(
    tx: &mut TxMut<'_>,
    issue: &Issue,
    author: i64,
    (before, after): (Option<&str>, Option<&str>),
    at: Timestamp,
) -> Result<(), ApiError> {
    let Some(after) = after else {
        return Ok(());
    };
    let Some(repo) = tx.table::<Repos>().get(issue.repo_id).cloned() else {
        return Ok(());
    };
    let old = before.map_or_default(mentions);
    let mut added: Vec<&str> = Vec::new();
    for name in mentions(after) {
        if !old.contains(&name) && !added.contains(&name) {
            added.push(name);
        }
    }
    for name in added {
        let (kinds, users): (&[StateKind], Vec<i64>) = match name.split_once('/') {
            None => (
                &[StateKind::Mentioned, StateKind::Subscribed],
                account(tx, name, AccountKind::User).into_iter().collect(),
            ),
            Some((org, slug)) => (
                &[StateKind::Subscribed],
                team_members(tx, author, org, slug),
            ),
        };
        for user in users {
            if !can_read(tx, user, &repo) {
                continue;
            }
            for kind in kinds {
                timeline::record_at(tx, issue, (user, None), kind.clone(), at)?;
            }
        }
    }
    Ok(())
}

fn account(tx: &Tx, login: &str, kind: AccountKind) -> Option<i64> {
    tx.table::<Accounts>()
        .by_login(login)
        .filter(|a| a.kind == kind)
        .map(|a| a.id)
}

fn can_read(tx: &Tx, user: i64, repo: &Repo) -> bool {
    repo.visibility == Visibility::Public || repo_role(tx, user, repo).is_some()
}

fn team_members(tx: &Tx, author: i64, org: &str, slug: &str) -> Vec<i64> {
    let Some(org) = account(tx, org, AccountKind::Organization) else {
        return Vec::new();
    };
    let Some(membership) = tx
        .table::<Memberships>()
        .of(org, author)
        .filter(|m| m.state == MembershipState::Active)
    else {
        return Vec::new();
    };
    let teams = tx.table::<Teams>();
    let Some(team) = teams.by_slug(org, slug) else {
        return Vec::new();
    };
    let on_team = |team: &Team| team.members.iter().any(|m| m.user_id == author);
    if team.privacy == TeamPrivacy::Secret && membership.role != OrgRole::Admin && !on_team(team) {
        return Vec::new();
    }
    let mut reached = vec![team.id];
    let mut members = Vec::new();
    while let Some(id) = reached.pop() {
        for member in teams.get(id).into_iter().flat_map(|t| &t.members) {
            if !members.contains(&member.user_id) {
                members.push(member.user_id);
            }
        }
        reached.extend(
            teams
                .iter()
                .filter(|t| t.parent_id == Some(id))
                .map(|t| t.id),
        );
    }
    members
}

fn mentions(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    for prose in prose(text) {
        mentions_in(prose, &mut found);
    }
    found
}

fn prose(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    let mut paragraph: Option<usize> = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let body = line.trim_end_matches(['\n', '\r']);
        if let Some((mark, len)) = fence {
            if closes_fence(body, mark, len) {
                fence = None;
            }
            continue;
        }
        let blank = body.trim_matches([' ', '\t']).is_empty();
        let opened = opens_fence(body);
        let indented = !blank && paragraph.is_none() && indent(body) >= 4;
        if blank || opened.is_some() || indented {
            if let Some(from) = paragraph.take() {
                blocks.extend(text.get(from..start));
            }
            fence = opened;
        } else if paragraph.is_none() {
            paragraph = Some(start);
        }
    }
    if let Some(from) = paragraph {
        blocks.extend(text.get(from..));
    }
    blocks
}

fn indent(line: &str) -> usize {
    let mut width = 0;
    for byte in line.bytes() {
        match byte {
            b' ' => width += 1,
            b'\t' => width += 4 - width % 4,
            _ => break,
        }
    }
    width
}

fn fence_run(line: &str) -> Option<(u8, usize, &str)> {
    let spaces = line.bytes().take_while(|b| *b == b' ').count();
    if spaces > 3 {
        return None;
    }
    let rest = line.get(spaces..)?;
    let mark = *rest
        .as_bytes()
        .first()
        .filter(|b| matches!(b, b'`' | b'~'))?;
    let len = rest.bytes().take_while(|b| *b == mark).count();
    (len >= 3).then(|| (mark, len, rest.get(len..).unwrap_or_default()))
}

fn opens_fence(line: &str) -> Option<(u8, usize)> {
    let (mark, len, info) = fence_run(line)?;
    (mark == b'~' || !info.contains('`')).then_some((mark, len))
}

fn closes_fence(line: &str, mark: u8, len: usize) -> bool {
    fence_run(line).is_some_and(|(m, l, rest)| {
        m == mark && l >= len && rest.trim_matches([' ', '\t']).is_empty()
    })
}

fn mentions_in<'t>(prose: &'t str, found: &mut Vec<&'t str>) {
    let bytes = prose.as_bytes();
    let mut i = 0;
    while let Some(&byte) = bytes.get(i) {
        match byte {
            b'\\' if bytes.get(i + 1) == Some(&b'`') => i += 2,
            b'`' => {
                let run = ticks(bytes, i);
                i = span_end(bytes, i + run, run).unwrap_or(i + run);
            }
            b'@' if i == 0 || !word(bytes[i - 1]) => {
                if let Some(name) = prose.get(i + 1..).and_then(mention) {
                    found.push(name);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
}

fn ticks(bytes: &[u8], from: usize) -> usize {
    bytes
        .get(from..)
        .map_or(0, |rest| rest.iter().take_while(|b| **b == b'`').count())
}

fn span_end(bytes: &[u8], from: usize, run: usize) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes.get(i) == Some(&b'`') {
            let closing = ticks(bytes, i);
            if closing == run {
                return Some(i + closing);
            }
            i += closing;
        } else {
            i += 1;
        }
    }
    None
}

fn word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn mention(rest: &str) -> Option<&str> {
    let login_len = rest
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'-')
        .count();
    let login = rest.get(..login_len)?.trim_end_matches('-');
    if login.is_empty() || login.len() > MAX_LOGIN || login.starts_with('-') {
        return None;
    }
    let after = rest.get(login_len..).unwrap_or_default();
    let Some(team) = after.strip_prefix('/') else {
        return Some(login);
    };
    if login.len() != login_len {
        return Some(login);
    }
    let slug_len = team
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        .count();
    let slug = team.get(..slug_len)?.trim_end_matches('.');
    if slug.is_empty() {
        return Some(login);
    }
    rest.get(..login_len + 1 + slug.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_are_the_users_and_teams_outside_code_and_addresses() {
        // Given text with users, a team, an address, code spans and a fenced block
        let text = "cc @alice, @acme/core-team. and @bob-\nmail me at x@example.com `@carol`\n\
                    ```\n@dave\n```\n(@erin) @-nope @acme/ @frank/";
        // When its mentions are read
        let found = mentions(text);
        // Then only the users and the team outside code count
        assert_eq!(
            found,
            ["alice", "acme/core-team", "bob", "erin", "acme", "frank"]
        );
    }

    #[test]
    fn no_form_of_gfm_code_holds_a_mention() {
        for (text, want) in [
            ("~~~\n@a\n~~~\n@b", &["b"][..]),
            ("````\n```\n@a\n````\n@b", &["b"]),
            ("~~~\n@a\n```\n@b", &[]),
            ("  ```rust\n@a\n  ```\n@b", &["b"]),
            ("    ```\n@a", &["a"]),
            ("text\n\n    @a code\n\n@b", &["b"]),
            ("\t@a code\n@b", &["b"]),
            ("para\n    @a continues the paragraph", &["a"]),
            ("``@a ` still code`` @b", &["b"]),
            ("`@a``@b` @c", &["c"]),
            ("`` @a\nspans a line `` @b", &["b"]),
            ("unclosed `@a", &["a"]),
            ("\\`@a` @b", &["a", "b"]),
            ("> quoted @a", &["a"]),
        ] {
            // Given text with code in one of the forms that GitHub Flavored Markdown names
            // When its mentions are read
            let found = mentions(text);
            // Then the mentions inside code are left out
            assert_eq!(found, want, "{text:?}");
        }
    }
}
