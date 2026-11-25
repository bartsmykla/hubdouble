use hubdouble_model::{AccountKind, ApiError, Issue, Timestamp};
use hubdouble_store::{Accounts, Teams, Tx, TxMut};

use crate::timeline::{self, StateEvents, StateKind};

const MAX_LOGIN: usize = 39;

/// Records the `mentioned` and `subscribed` issue events of the `@` mentions that `after`
/// adds to `before`, in the order they appear, as live GitHub records them: a user gets
/// `mentioned` and then `subscribed`, and each member of a mentioned team only `subscribed`.
/// A user gets each event once per issue. A mention in a code span or a fenced code block
/// does not count.
///
/// # Errors
///
/// A store error.
pub fn record_mentions(
    tx: &mut TxMut<'_>,
    issue: &Issue,
    (before, after): (Option<&str>, Option<&str>),
    at: Timestamp,
) -> Result<(), ApiError> {
    let Some(after) = after else {
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
        match name.split_once('/') {
            None => {
                let Some(user) = account(tx, name, AccountKind::User) else {
                    continue;
                };
                once(tx, issue, user, StateKind::Mentioned, at)?;
                once(tx, issue, user, StateKind::Subscribed, at)?;
            }
            Some((org, slug)) => {
                let Some(org) = account(tx, org, AccountKind::Organization) else {
                    continue;
                };
                let members: Vec<i64> = tx
                    .table::<Teams>()
                    .by_slug(org, slug)
                    .map_or_default(|team| team.members.iter().map(|m| m.user_id).collect());
                for member in members {
                    once(tx, issue, member, StateKind::Subscribed, at)?;
                }
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

fn once(
    tx: &mut TxMut<'_>,
    issue: &Issue,
    user: i64,
    kind: StateKind,
    at: Timestamp,
) -> Result<(), ApiError> {
    let recorded = tx
        .table::<StateEvents>()
        .children(issue.id)
        .any(|e| e.kind == kind && e.actor_id == user);
    if recorded {
        return Ok(());
    }
    timeline::record_at(tx, issue, (user, None), kind, at)
}

fn mentions(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let bytes = line.as_bytes();
        let mut code = false;
        for (i, byte) in bytes.iter().enumerate() {
            match byte {
                b'`' => code = !code,
                b'@' if !code && (i == 0 || !word(bytes[i - 1])) => {
                    if let Some(name) = line.get(i + 1..).and_then(mention) {
                        found.push(name);
                    }
                }
                _ => {}
            }
        }
    }
    found
}

fn word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'`'
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
}
