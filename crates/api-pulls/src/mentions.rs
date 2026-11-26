use hubdouble_auth::repo_role;
use hubdouble_model::{
    AccountKind, ApiError, Issue, MembershipState, OrgRole, Repo, Team, TeamPrivacy, Timestamp,
    Visibility,
};
use hubdouble_store::{Accounts, Memberships, Repos, Teams, Tx, TxMut};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::timeline::{self, StateKind};

const HIDING_ELEMENTS: [&str; 5] = ["pre", "code", "a", "style", "script"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Named {
    User(i64),
    Team(i64),
}

/// Records the `mentioned` and `subscribed` issue events of the `@` mentions that `after`
/// adds to `before`, in the order they appear, as live GitHub records them: each text that
/// mentions a user gives a `mentioned` and then a `subscribed`, again for a user that an
/// earlier text mentioned, and each member of a mentioned team, its child teams included, only
/// a `subscribed`. One text subscribes a user once. A user who cannot read the repository gets
/// none. `author` mentions a team only as a member of its organization, a secret team only as
/// one of its members or an owner, and a team with notifications off reaches no one. Logins
/// and team slugs match without case, and a mention ends where the user and team patterns of
/// html-pipeline's mention filters end it. A mention in GitHub Flavored Markdown code, in a
/// link, in an HTML comment, or inside a `pre`, `code`, `a`, `style` or `script` element does
/// not count.
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
    let mut counted: Vec<Named> = before
        .map_or_default(mentions)
        .iter()
        .filter_map(|name| resolve(tx, name))
        .collect();
    let mut subscribed: Vec<i64> = Vec::new();
    for name in mentions(after) {
        let Some(named) = resolve(tx, &name) else {
            continue;
        };
        if counted.contains(&named) {
            continue;
        }
        counted.push(named);
        let (mentioned, users) = match named {
            Named::User(user) => (true, vec![user]),
            Named::Team(team) => (false, team_members(tx, author, team)),
        };
        for user in users {
            if !can_read(tx, user, &repo) {
                continue;
            }
            if mentioned {
                timeline::record_at(tx, issue, (user, None), StateKind::Mentioned, at)?;
            }
            if !subscribed.contains(&user) {
                subscribed.push(user);
                timeline::record_at(tx, issue, (user, None), StateKind::Subscribed, at)?;
            }
        }
    }
    Ok(())
}

fn resolve(tx: &Tx, name: &str) -> Option<Named> {
    match name.split_once('/') {
        None => account(tx, name, AccountKind::User).map(Named::User),
        Some((org, slug)) => {
            let org = account(tx, org, AccountKind::Organization)?;
            let team = tx
                .table::<Teams>()
                .by_slug(org, &slug.to_ascii_lowercase())?;
            Some(Named::Team(team.id))
        }
    }
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

fn team_members(tx: &Tx, author: i64, team: i64) -> Vec<i64> {
    let teams = tx.table::<Teams>();
    let Some(team) = teams.get(team).filter(|t| t.notifications_enabled) else {
        return Vec::new();
    };
    let Some(membership) = tx
        .table::<Memberships>()
        .of(team.org_id, author)
        .filter(|m| m.state == MembershipState::Active)
    else {
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

#[derive(Default)]
struct Scan {
    found: Vec<String>,
    prose: String,
    hidden: usize,
    elements: usize,
    comment: bool,
}

impl Scan {
    fn text(&mut self, text: &str) {
        if self.hidden == 0 && self.elements == 0 && !self.comment {
            self.prose.push_str(text);
        }
    }

    fn flush(&mut self) {
        mentions_in(&self.prose, &mut self.found);
        self.prose.clear();
    }

    fn html(&mut self, chunk: &str) {
        let mut rest = chunk;
        loop {
            if self.comment {
                let Some(end) = rest.find("-->") else {
                    return;
                };
                self.comment = false;
                rest = rest.get(end + 3..).unwrap_or_default();
            }
            let Some(open) = rest.find('<') else {
                break;
            };
            self.text(rest.get(..open).unwrap_or_default());
            let tag_and_rest = rest.get(open + 1..).unwrap_or_default();
            if let Some(comment) = tag_and_rest.strip_prefix("!--") {
                self.flush();
                if let Some(after) = comment
                    .strip_prefix('>')
                    .or_else(|| comment.strip_prefix("->"))
                {
                    rest = after;
                } else {
                    self.comment = true;
                    rest = comment;
                }
                continue;
            }
            let Some(close) = tag_and_rest.find('>') else {
                self.text(rest.get(open..).unwrap_or_default());
                return;
            };
            self.flush();
            let tag = tag_and_rest.get(..close).unwrap_or_default();
            let (closing, name) = tag.strip_prefix('/').map_or((false, tag), |n| (true, n));
            let name_len = name.bytes().take_while(u8::is_ascii_alphanumeric).count();
            let name = name.get(..name_len).unwrap_or_default();
            if HIDING_ELEMENTS
                .iter()
                .any(|hiding| hiding.eq_ignore_ascii_case(name))
            {
                if closing {
                    self.elements = self.elements.saturating_sub(1);
                } else {
                    self.elements += 1;
                }
            }
            rest = tag_and_rest.get(close + 1..).unwrap_or_default();
        }
        self.text(rest);
    }
}

fn mentions(text: &str) -> Vec<String> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_GFM;
    let mut scan = Scan::default();
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Text(text) => {
                scan.text(&text);
                continue;
            }
            Event::SoftBreak | Event::HardBreak => {
                scan.text("\n");
                continue;
            }
            _ => scan.flush(),
        }
        match event {
            Event::Start(Tag::Link { .. } | Tag::Image { .. } | Tag::CodeBlock(_)) => {
                scan.hidden += 1;
            }
            Event::End(TagEnd::Link | TagEnd::Image | TagEnd::CodeBlock) => {
                scan.hidden = scan.hidden.saturating_sub(1);
            }
            Event::Html(chunk) | Event::InlineHtml(chunk) => scan.html(&chunk),
            _ => {}
        }
    }
    scan.flush();
    scan.found
}

fn mentions_in(prose: &str, found: &mut Vec<String>) {
    let bytes = prose.as_bytes();
    let mut i = 0;
    while let Some(&byte) = bytes.get(i) {
        let free = i == 0 || bytes.get(i - 1).is_some_and(|b| !word(*b));
        let rest = prose.get(i..).unwrap_or_default();
        if free
            && ["http://", "https://", "www."]
                .iter()
                .any(|p| starts_with_ignoring_case(rest, p))
        {
            i += rest
                .bytes()
                .position(|b| b.is_ascii_whitespace())
                .unwrap_or(rest.len());
            continue;
        }
        if byte == b'@'
            && free
            && let Some(name) = prose.get(i + 1..).and_then(mention)
        {
            found.push(name.to_owned());
        }
        i += 1;
    }
}

fn starts_with_ignoring_case(text: &str, prefix: &str) -> bool {
    text.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn mention(rest: &str) -> Option<&str> {
    let bytes = rest.as_bytes();
    let run = |from: usize, extra: &[u8]| {
        let starts = bytes.get(from).is_some_and(u8::is_ascii_alphanumeric);
        let len = bytes
            .iter()
            .skip(from)
            .take_while(|b| b.is_ascii_alphanumeric() || extra.contains(b))
            .count();
        starts.then_some(len)
    };
    let login = run(0, b"-")?;
    match bytes.get(login) {
        Some(b'/') => {
            let slug = rest.get(login + 1..login + 1 + run(login + 1, b"-_")?)?;
            rest.get(..login + 1 + slug.trim_end_matches('-').len())
        }
        Some(b'.') if bytes.get(login + 1).is_some_and(|b| word(*b)) => None,
        Some(b) if word(*b) => None,
        _ => rest.get(..login),
    }
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
        assert_eq!(found, ["alice", "acme/core-team", "bob-", "erin"]);
    }

    #[test]
    fn a_mention_ends_where_githubs_mention_filters_end_it() {
        for (text, want) in [
            ("@alice.bob", &[][..]),
            ("@alice_bob", &[]),
            ("@alice_x", &[]),
            ("@frank/", &[]),
            ("@alice.", &["alice"]),
            ("@alice. next", &["alice"]),
            ("@alice..bob", &["alice"]),
            ("@alice-", &["alice-"]),
            ("@acme/a.b", &["acme/a"]),
            ("@acme/core-", &["acme/core"]),
            ("@acme/core_", &["acme/core_"]),
            ("@acme/-core", &[]),
            ("é@alice", &["alice"]),
        ] {
            // Given a mention whose name meets a dot, an underscore, a slash or a dash
            // When its mentions are read
            let found = mentions(text);
            // Then the name is what the user and team patterns of html-pipeline match
            assert_eq!(found, want, "{text:?}");
        }
    }

    #[test]
    fn deep_markdown_stays_on_a_small_stack() {
        // Given text that nests block quotes, brackets and emphasis 100000 levels deep
        let n = 100_000;
        let texts = [
            format!("{}x\n\n@b", "> ".repeat(n)),
            format!("{} @b", "[".repeat(n)),
            format!("{}x{} @b", "*".repeat(n), "*".repeat(n)),
        ];
        // When its mentions are read on a thread with a 256 KiB stack
        let found = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || texts.iter().map(|t| mentions(t)).collect::<Vec<_>>())
            .unwrap()
            .join()
            .unwrap();
        // Then each finds the mention after the nesting
        assert_eq!(found, [["b"], ["b"], ["b"]]);
    }

    #[test]
    fn no_form_of_gfm_code_or_link_holds_a_mention() {
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
            ("\\\\`@a` @b", &["b"]),
            ("> quoted @a", &["a"]),
            ("> ```\n> @a\n> ```\n@b", &["b"]),
            ("- item\n\n      @a\n\n  @b", &["b"]),
            ("- item\n\n    @a", &["a"]),
            ("[@a](https://example.com) @b", &["b"]),
            ("<https://example.com/@a> @b", &["b"]),
            ("see https://medium.com/@a and @b", &["b"]),
            ("<code>@a</code> @b", &["b"]),
            ("<pre>@a</pre>\n\n@b", &["b"]),
            ("<div>@a <script>@b</script></div>", &["a"]),
            ("<!-- note > @a -->\n\n@b", &["b"]),
            ("x <!-- <foo> @a --> @b", &["b"]),
            ("<!--\nnote\n> @a\n-->\n\n@b", &["b"]),
            ("x <!--> @a", &["a"]),
            ("<a href=https://example.com/>@a</a> @b", &["b"]),
            ("**x**@a", &["a"]),
        ] {
            // Given text with code or a link in a form that GitHub Flavored Markdown names, or
            // an element that GitHub's mention filter skips
            // When its mentions are read
            let found = mentions(text);
            // Then the mentions inside them are left out
            assert_eq!(found, want, "{text:?}");
        }
    }
}
