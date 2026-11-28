use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use rust_stemmers::{Algorithm, Stemmer};

use hubdouble_api_orgs::TeamViewer;
use hubdouble_auth::{Actor, Target, authorize};
use hubdouble_git::{Git, ObjectId};
use hubdouble_gql::{Args, Conn, Ctx, ErrorType, GqlError, Obj, Resolved, connection};
use hubdouble_model::{
    Account, Grant, Issue, IssueState, Pull, ReactionContent, ReactionSubjectKind, Repo,
    Requirement, ReviewState, Scope, StateReason, Visibility,
};
use hubdouble_store::{
    Accounts, IssueComments, Issues, Labels, Reactions, Repos, ReviewComments, Reviews, Teams, Tx,
};

use crate::query::{
    Clause, Count, Filter, Is, Missing, Person, Query, Review, SortKey, Status, When,
};

const fn read(grants: &'static [Grant]) -> Requirement {
    Requirement {
        any_of: grants,
        all_of: &[],
        scope: Scope::Repo,
        scopes: &[],
    }
}

const PULLS: Requirement = read(&[Grant::read("pull_requests")]);
const ISSUES: Requirement = read(&[Grant::read("issues")]);
const PULL_COMMITS: usize = 250;

pub(crate) fn search(cx: &Ctx<'_>, _: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let kind = args.get::<String>("type")?.unwrap_or_default();
    if !matches!(kind.as_str(), "ISSUE" | "ISSUE_ADVANCED" | "ISSUE_HYBRID") {
        return Err(GqlError::new(
            ErrorType::NotImplemented,
            format!("search(type: {kind}) is not implemented"),
        ));
    }
    let text = args.get::<String>("query")?.unwrap_or_default();
    let found = run(cx.tx, cx.git, cx.actor, &Query::parse(&text));
    connection(
        "SearchResultItemConnection",
        &found,
        args,
        |i| i.id.to_string(),
        |i| Resolved::Object {
            type_name: if i.pull.is_some() {
                "PullRequest"
            } else {
                "Issue"
            },
            value: Arc::<Issue>::clone(i),
        },
    )
}

pub(crate) fn nodes(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let conn = obj.get::<Conn>()?;
    Ok(Resolved::List(
        conn.edges.iter().map(|e| e.node.clone()).collect(),
    ))
}

pub(crate) fn issue_count(_: &Ctx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(obj.get::<Conn>()?.total_count.into()))
}

struct Env<'a> {
    tx: &'a Tx,
    git: &'a Git,
    actor: &'a Actor,
    linked: Option<Linked>,
}

#[derive(Default)]
struct Linked {
    issues: HashSet<i64>,
    pulls: HashSet<i64>,
}

fn uses_links(q: &Query<'_>) -> bool {
    q.clauses
        .iter()
        .flat_map(|c| &c.any)
        .any(|f| matches!(f, Filter::LinkedPull | Filter::LinkedIssue))
}

pub(crate) fn run<'t>(tx: &'t Tx, git: &Git, actor: &Actor, q: &Query<'_>) -> Vec<&'t Arc<Issue>> {
    let mut env = Env {
        tx,
        git,
        actor,
        linked: None,
    };
    let repos = repos(tx, q);
    if uses_links(q) {
        let mut linked = Linked::default();
        for (pull, issue) in hubdouble_api_issues::closing_links(tx) {
            linked.pulls.insert(pull);
            linked.issues.insert(issue);
        }
        env.linked = Some(linked);
    }
    let mut found = Vec::new();
    for repo in repos {
        let pulls = authorize(tx, actor, &PULLS, Target::Repo(repo)).is_ok();
        let issues = authorize(tx, actor, &ISSUES, Target::Repo(repo)).is_ok();
        if !(pulls || issues) {
            continue;
        }
        found.extend(tx.table::<Issues>().in_repo(repo.id).filter(|i| {
            (if i.pull.is_some() { pulls } else { issues }) && matches(&env, q, repo, i)
        }));
    }
    match q.sort {
        SortKey::Created => found.sort_unstable_by_key(|i| (i.created_at, i.id)),
        SortKey::Updated => found.sort_unstable_by_key(|i| (i.updated_at, i.id)),
        SortKey::Comments => found.sort_by_cached_key(|i| (comments(tx, i), i.id)),
        SortKey::Reactions(content) => {
            found.sort_by_cached_key(|i| (reactions(tx, i, content), i.id));
        }
        SortKey::Interactions => {
            found.sort_by_cached_key(|i| (comments(tx, i) + reactions(tx, i, None), i.id));
        }
    }
    if !q.ascending {
        found.reverse();
    }
    found
}

fn repos<'t>(tx: &'t Tx, q: &Query<'_>) -> Vec<&'t Arc<Repo>> {
    let accounts = tx.table::<Accounts>();
    let table = tx.table::<Repos>();
    let owners: Vec<i64> = q
        .owners
        .iter()
        .filter_map(|login| accounts.by_login(login).map(|a| a.id))
        .collect();
    let mut repos: Vec<&Arc<Repo>> = if q.repos.is_empty() && q.owners.is_empty() {
        table.iter().collect()
    } else {
        q.repos
            .iter()
            .filter_map(|full| named_repo(tx, full))
            .chain(owners.iter().flat_map(|id| table.children(*id)))
            .collect()
    };
    repos.sort_unstable_by_key(|r| r.id);
    repos.dedup_by_key(|r| r.id);
    repos
}

fn named_repo<'t>(tx: &'t Tx, full: &str) -> Option<&'t Arc<Repo>> {
    let (owner, name) = full.split_once('/')?;
    let owner = tx.table::<Accounts>().by_login(owner)?;
    tx.table::<Repos>().by_name(owner.id, name)
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn matches(env: &Env<'_>, q: &Query<'_>, repo: &Repo, i: &Issue) -> bool {
    q.clauses.iter().all(|c| holds(env, c, repo, i)) && text(env.tx, env.git, q, i)
}

fn holds(env: &Env<'_>, clause: &Clause<'_>, repo: &Repo, i: &Issue) -> bool {
    clause.any.iter().any(|f| filter(env, f, repo, i)) != clause.negated
}

fn filter(env: &Env<'_>, f: &Filter<'_>, repo: &Repo, i: &Issue) -> bool {
    let tx = env.tx;
    let pull = i.pull.as_ref();
    match f {
        Filter::Repo(full) => named_repo(tx, full).is_some_and(|r| r.id == repo.id),
        Filter::Owner(login) => account(env, login).is_some_and(|a| a.id == repo.owner_id),
        Filter::Is(is) => is_state(*is, repo, i),
        Filter::Head(h) => pull.is_some_and(|p| starts_with_ignore_case(&p.head.ref_name, h)),
        Filter::Base(b) => pull.is_some_and(|p| starts_with_ignore_case(&p.base.ref_name, b)),
        Filter::Person(person, login) => involves(env, *person, login, i),
        Filter::Team(name) => {
            team_id(env, name).is_some() && {
                let handle = format!("@{name}");
                texts(tx, i).any(|t| mentions(t, &handle))
            }
        }
        Filter::TeamReviewRequested(name) => pull
            .zip(team_id(env, name))
            .is_some_and(|(p, id)| p.requested_teams.contains(&id)),
        Filter::Label(group) => {
            let labels = tx.table::<Labels>();
            i.label_ids
                .iter()
                .filter_map(|id| labels.get(*id))
                .any(|label| {
                    group
                        .iter()
                        .any(|name| label.name.eq_ignore_ascii_case(name))
                })
        }
        Filter::Milestone | Filter::Project => false,
        Filter::Status(status) => pull.is_some_and(|p| {
            use hubdouble_api_checks::RollupState as R;
            let state = hubdouble_api_checks::rollup_state(tx, i.repo_id, &p.head.sha);
            matches!(
                (status, state.unwrap_or(R::Pending)),
                (Status::Success, R::Success)
                    | (Status::Pending, R::Pending)
                    | (Status::Failure, R::Failure | R::Error)
            )
        }),
        Filter::Language(name) => repo
            .language
            .as_deref()
            .is_some_and(|l| l.eq_ignore_ascii_case(name)),
        Filter::Count(kind, bounds) => bounds.admits(match kind {
            Count::Comments => comments(tx, i),
            Count::Reactions => reactions(tx, i, None),
            Count::Interactions => comments(tx, i) + reactions(tx, i, None),
        }),
        Filter::Date(when, bounds) => match when {
            When::Created => Some(i.created_at),
            When::Updated => Some(i.updated_at),
            When::Closed => i.closed_at,
            When::Merged => pull.and_then(|p| p.merged_at),
        }
        .is_some_and(|t| bounds.admits(t)),
        Filter::Draft(draft) => pull.is_some_and(|p| p.draft == *draft),
        Filter::Archived(archived) => repo.archived == *archived,
        Filter::Review(review) => pull.is_some_and(|p| reviewed(tx, *review, i, p)),
        Filter::Missing(missing) => match missing {
            Missing::Label => i.label_ids.is_empty(),
            Missing::Assignee => i.assignee_ids.is_empty(),
            Missing::Milestone | Missing::Project => true,
        },
        Filter::LinkedPull => {
            pull.is_none()
                && env
                    .linked
                    .as_ref()
                    .is_some_and(|l| l.issues.contains(&i.id))
        }
        Filter::LinkedIssue => {
            pull.is_some() && env.linked.as_ref().is_some_and(|l| l.pulls.contains(&i.id))
        }
        Filter::NotPlanned(not_planned) => {
            i.state == IssueState::Closed
                && matches!(
                    i.state_reason,
                    Some(StateReason::NotPlanned | StateReason::Duplicate)
                ) == *not_planned
        }
    }
}

fn is_state(is: Is, repo: &Repo, i: &Issue) -> bool {
    let pull = i.pull.as_ref();
    match is {
        Is::Pull => pull.is_some(),
        Is::Issue => pull.is_none(),
        Is::Open => i.state == IssueState::Open,
        Is::Closed => i.state == IssueState::Closed,
        Is::Merged => pull.is_some_and(|p| p.merged_at.is_some()),
        Is::Unmerged => pull.is_some_and(|p| p.merged_at.is_none()),
        Is::Draft => pull.is_some_and(|p| p.draft),
        Is::Locked => i.locked,
        Is::Unlocked => !i.locked,
        Is::Public => repo.visibility == Visibility::Public,
        Is::Private => repo.visibility != Visibility::Public,
        Is::Archived => repo.archived,
        Is::Queued => false,
    }
}

fn account<'t>(env: &Env<'t>, login: &str) -> Option<&'t Arc<Account>> {
    if login.eq_ignore_ascii_case("@me") {
        let me = env.actor.account()?;
        return env.tx.table::<Accounts>().get(me.id);
    }
    let accounts = env.tx.table::<Accounts>();
    match login.strip_prefix("app/") {
        Some(slug) => accounts.by_login(&format!("{slug}[bot]")),
        None => accounts.by_login(login),
    }
}

fn team_id(env: &Env<'_>, name: &str) -> Option<i64> {
    let (org, slug) = name.split_once('/')?;
    let org = env.tx.table::<Accounts>().by_login(org)?;
    env.tx
        .table::<Teams>()
        .by_slug(org.id, slug)
        .filter(|t| TeamViewer::authorized(env.tx, env.actor, org).can_see(t))
        .map(|t| t.id)
}

fn on_requested_team(tx: &Tx, p: &Pull, user: i64) -> bool {
    let teams = tx.table::<Teams>();
    p.requested_teams
        .iter()
        .filter_map(|id| teams.get(*id))
        .any(|t| t.members.iter().any(|m| m.user_id == user))
}

fn has_reviewed(tx: &Tx, i: &Issue, user: i64) -> bool {
    tx.table::<Reviews>()
        .children(i.id)
        .any(|r| r.user_id == user && r.state != ReviewState::Pending)
}

fn involves(env: &Env<'_>, person: Person, login: &str, i: &Issue) -> bool {
    let tx = env.tx;
    if person == Person::Assignee && login == "*" {
        return !i.assignee_ids.is_empty();
    }
    let Some(who) = account(env, login) else {
        return false;
    };
    let pull = i.pull.as_ref();
    let commented = || {
        tx.table::<IssueComments>()
            .children(i.id)
            .any(|c| c.user_id == who.id)
    };
    let mentioned = || {
        let handle = format!("@{}", who.login);
        texts(tx, i).any(|t| mentions(t, &handle))
    };
    let requested = |p: &Pull| p.requested_reviewers.contains(&who.id);
    match person {
        Person::Author => i.user_id == who.id,
        Person::Assignee => i.assignee_ids.contains(&who.id),
        Person::Mentions => mentioned(),
        Person::Commenter => commented(),
        Person::Involves => {
            i.user_id == who.id || i.assignee_ids.contains(&who.id) || commented() || mentioned()
        }
        Person::ReviewedBy => pull.is_some() && has_reviewed(tx, i, who.id),
        Person::ReviewRequested => {
            pull.is_some_and(|p| requested(p) || on_requested_team(tx, p, who.id))
        }
        Person::UserReviewRequested => pull.is_some_and(requested),
        Person::TeamReviewRequestedUser => pull.is_some_and(|p| on_requested_team(tx, p, who.id)),
        Person::ReviewInvolves => pull.is_some_and(|p| {
            requested(p) || on_requested_team(tx, p, who.id) || has_reviewed(tx, i, who.id)
        }),
    }
}

fn reviewed(tx: &Tx, review: Review, i: &Issue, p: &Pull) -> bool {
    let decision = hubdouble_api_pulls::review_decision(tx, i, p);
    match review {
        Review::None => decision.is_none(),
        Review::Required => decision == Some("REVIEW_REQUIRED"),
        Review::Approved => decision == Some("APPROVED"),
        Review::ChangesRequested => decision == Some("CHANGES_REQUESTED"),
    }
}

fn comments(tx: &Tx, i: &Issue) -> i64 {
    let n = tx.table::<IssueComments>().children(i.id).count()
        + tx.table::<ReviewComments>().children(i.id).count()
        + tx.table::<Reviews>()
            .children(i.id)
            .filter(|r| r.state != ReviewState::Pending && !r.body.is_empty())
            .count();
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn reactions(tx: &Tx, i: &Issue, content: Option<ReactionContent>) -> i64 {
    let n = tx
        .table::<Reactions>()
        .children(i.id)
        .filter(|r| {
            r.subject_kind == ReactionSubjectKind::Issue && content.is_none_or(|c| r.content == c)
        })
        .count();
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn texts<'t>(tx: &'t Tx, i: &'t Issue) -> impl Iterator<Item = &'t str> {
    std::iter::once(i.title.as_str())
        .chain(i.body.as_deref())
        .chain(
            tx.table::<IssueComments>()
                .children(i.id)
                .map(|c| c.body.as_str()),
        )
}

fn login_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

fn mentions(text: &str, handle: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let handle = handle.to_ascii_lowercase();
    lower.match_indices(&handle).any(|(at, _)| {
        let before = lower[..at].chars().next_back();
        let after = lower[at + handle.len()..].chars().next();
        !before.is_some_and(|c| login_char(c) || c == '/')
            && !after.is_some_and(|c| login_char(c) || c == '/')
    })
}

static ENGLISH: LazyLock<Stemmer> = LazyLock::new(|| Stemmer::create(Algorithm::English));

fn stem(word: &str) -> String {
    ENGLISH.stem(word).into_owned()
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn contains_phrase(haystack: &[(String, String)], phrase: &[String]) -> bool {
    !phrase.is_empty()
        && haystack.windows(phrase.len()).any(|window| {
            window
                .iter()
                .zip(phrase)
                .all(|((word, stemmed), want)| word == want || stemmed == want)
        })
}

fn is_sha(term: &str) -> bool {
    (7..=40).contains(&term.len()) && term.bytes().all(|b| b.is_ascii_hexdigit())
}

fn pull_commits(git: &Git, tx: &Tx, i: &Issue, p: &Pull) -> Vec<String> {
    let mut shas: Vec<String> = p
        .merge_commit_sha
        .iter()
        .chain([&p.head.sha])
        .cloned()
        .collect();
    let (Ok(head), Ok(base)) = (
        p.head.sha.parse::<ObjectId>(),
        p.base.sha.parse::<ObjectId>(),
    ) else {
        return shas;
    };
    if let Ok(log) = git.log(tx, i.repo_id, head, false, PULL_COMMITS) {
        shas.extend(
            log.into_iter()
                .filter(|(id, _)| !git.is_ancestor(tx, i.repo_id, *id, base).unwrap_or(false))
                .map(|(id, _)| id.to_string()),
        );
    }
    shas
}

fn text(tx: &Tx, git: &Git, q: &Query<'_>, i: &Issue) -> bool {
    if q.terms.is_empty() && q.excluded.is_empty() {
        return true;
    }
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut add = |text: &str| {
        fields.extend(words(text).into_iter().map(|w| {
            let stemmed = stem(&w);
            (w, stemmed)
        }));
    };
    if q.scope.title {
        add(&i.title);
    }
    if q.scope.body {
        add(i.body.as_deref().unwrap_or_default());
    }
    if q.scope.comments {
        for c in tx.table::<IssueComments>().children(i.id) {
            add(&c.body);
        }
    }
    let commits = match &i.pull {
        Some(p) if q.terms.iter().any(|t| is_sha(t)) => pull_commits(git, tx, i, p),
        _ => Vec::new(),
    };
    let found = |term: &String| {
        contains_phrase(&fields, &words(term))
            || (is_sha(term) && commits.iter().any(|sha| sha.starts_with(term.as_str())))
    };
    q.terms.iter().all(found) && !q.excluded.iter().any(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_word_finds_its_inflections_but_not_its_stem() {
        // Given the words of live atom/language-toml searches: `spec` found `specs` and not
        // `specific`, `specs` did not find `spec`, and `fix` found `fixed` and `fixes`
        let text = |t: &str| -> Vec<(String, String)> {
            words(t)
                .into_iter()
                .map(|w| {
                    let s = stem(&w);
                    (w, s)
                })
                .collect()
        };
        let found = |haystack: &str, query: &str| contains_phrase(&text(haystack), &words(query));

        // Then each finds what GitHub found
        assert!(found("add specs", "spec"));
        assert!(!found("a specific case", "spec"));
        assert!(!found("the spec file", "specs"));
        assert!(found("Fixed dates", "fix"));
        assert!(found("more fixes", "fix"));
        assert!(found("changed line endings by accident", "line endings"));
        assert!(!found("line and endings", "line endings"));
    }

    #[test]
    fn a_mention_is_a_whole_handle() {
        // Given texts that name a user with and without a word boundary
        assert!(mentions("thanks @Caleb531, merged", "@caleb531"));
        assert!(!mentions("thanks @caleb5310", "@caleb531"));
        assert!(!mentions("mail a@caleb531.dev", "@caleb531"));
        assert!(!mentions("see @acme/ops", "@acme"));
        assert!(mentions("ask @acme/ops", "@acme/ops"));
    }
}
