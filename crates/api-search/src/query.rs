use hubdouble_model::{ReactionContent, Timestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Is {
    Pull,
    Issue,
    Open,
    Closed,
    Merged,
    Unmerged,
    Draft,
    Locked,
    Unlocked,
    Public,
    Private,
    Queued,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortKey {
    Created,
    Updated,
    Comments,
    Reactions(Option<ReactionContent>),
    Interactions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Person {
    Author,
    Assignee,
    Mentions,
    Commenter,
    Involves,
    ReviewedBy,
    ReviewRequested,
    UserReviewRequested,
    TeamReviewRequestedUser,
    ReviewInvolves,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Count {
    Comments,
    Reactions,
    Interactions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum When {
    Created,
    Updated,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Review {
    None,
    Required,
    Approved,
    ChangesRequested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Missing {
    Label,
    Assignee,
    Milestone,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Pending,
    Success,
    Failure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bounds<T> {
    pub(crate) low: Option<T>,
    pub(crate) high: Option<T>,
}

impl<T: PartialOrd + Copy> Bounds<T> {
    pub(crate) fn admits(&self, value: T) -> bool {
        self.low.is_none_or(|low| value >= low) && self.high.is_none_or(|high| value <= high)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Filter<'q> {
    Repo(&'q str),
    Owner(&'q str),
    Is(Is),
    Head(&'q str),
    Base(&'q str),
    Person(Person, &'q str),
    Team(&'q str),
    TeamReviewRequested(&'q str),
    Label(Vec<&'q str>),
    Milestone,
    Project,
    Status(Status),
    Language(&'q str),
    Count(Count, Bounds<i64>),
    Date(When, Bounds<Timestamp>),
    Draft(bool),
    Archived(bool),
    Review(Review),
    Missing(Missing),
    LinkedPull,
    LinkedIssue,
    NotPlanned(bool),
}

impl Filter<'_> {
    fn joins(&self, other: &Filter<'_>) -> bool {
        match (self, other) {
            (Filter::Head(_), Filter::Head(_)) | (Filter::Base(_), Filter::Base(_)) => true,
            (Filter::Person(a, _), Filter::Person(b, _)) => {
                a == b && matches!(a, Person::Author | Person::Involves)
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Clause<'q> {
    pub(crate) negated: bool,
    pub(crate) any: Vec<Filter<'q>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Scope {
    pub(crate) title: bool,
    pub(crate) body: bool,
    pub(crate) comments: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Query<'q> {
    pub(crate) repos: Vec<&'q str>,
    pub(crate) owners: Vec<&'q str>,
    pub(crate) clauses: Vec<Clause<'q>>,
    pub(crate) scope: Scope,
    pub(crate) terms: Vec<String>,
    pub(crate) excluded: Vec<String>,
    pub(crate) sort: SortKey,
    pub(crate) ascending: bool,
}

fn tokens(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        rest = rest.trim_start();
        if rest.is_empty() {
            return None;
        }
        let mut quoted = false;
        let end = rest
            .char_indices()
            .find(|&(_, c)| {
                if c == '"' {
                    quoted = !quoted;
                }
                c.is_whitespace() && !quoted
            })
            .map_or(rest.len(), |(i, _)| i);
        let (token, tail) = rest.split_at(end);
        rest = tail;
        Some(token)
    })
}

fn unquote(value: &str) -> &str {
    value.trim_matches('"')
}

fn list(value: &str) -> impl Iterator<Item = &str> {
    value.split(',').map(unquote).filter(|v| !v.is_empty())
}

fn bounds<T: Copy>(
    value: &str,
    parse: impl Fn(&str) -> Option<(T, T)>,
    after: impl Fn(T) -> T,
    before: impl Fn(T) -> T,
) -> Option<Bounds<T>> {
    let side = |text: &str| -> Option<Option<(T, T)>> {
        if text == "*" {
            Some(None)
        } else {
            parse(text).map(Some)
        }
    };
    let (low, high) = if let Some(v) = value.strip_prefix(">=") {
        (parse(v)?.0.into(), None)
    } else if let Some(v) = value.strip_prefix("<=") {
        (None, parse(v)?.1.into())
    } else if let Some(v) = value.strip_prefix('>') {
        (Some(after(parse(v)?.1)), None)
    } else if let Some(v) = value.strip_prefix('<') {
        (None, Some(before(parse(v)?.0)))
    } else if let Some((a, b)) = value.split_once("..") {
        (side(a)?.map(|r| r.0), side(b)?.map(|r| r.1))
    } else {
        let (low, high) = parse(value)?;
        (Some(low), Some(high))
    };
    Some(Bounds { low, high })
}

fn count(value: &str) -> Option<Bounds<i64>> {
    bounds(
        value,
        |v| v.parse::<i64>().ok().map(|n| (n, n)),
        |n: i64| n.saturating_add(1),
        |n: i64| n.saturating_sub(1),
    )
}

fn moment(text: &str) -> Option<(Timestamp, Timestamp)> {
    if text.len() == 10 {
        let start = Timestamp::parse(&format!("{text}T00:00:00Z")).ok()?;
        return Some((start, start.plus_seconds(86_399)));
    }
    let at = Timestamp::parse(text)
        .or_else(|_| Timestamp::parse(&format!("{text}Z")))
        .ok()?;
    Some((at, at))
}

fn date(value: &str) -> Option<Bounds<Timestamp>> {
    bounds(value, moment, |t| t.plus_seconds(1), |t| t.plus_seconds(-1))
}

fn sort_key(value: &str) -> Option<(SortKey, bool)> {
    let lower = value.to_ascii_lowercase();
    let (field, ascending) = if let Some(f) = lower.strip_suffix("-asc") {
        (f, true)
    } else {
        (lower.strip_suffix("-desc").unwrap_or(&lower), false)
    };
    let key = match field {
        "created" => SortKey::Created,
        "updated" => SortKey::Updated,
        "comments" => SortKey::Comments,
        "interactions" => SortKey::Interactions,
        "reactions" => SortKey::Reactions(None),
        "reactions-+1" => SortKey::Reactions(Some(ReactionContent::ThumbsUp)),
        "reactions--1" => SortKey::Reactions(Some(ReactionContent::ThumbsDown)),
        "reactions-smile" => SortKey::Reactions(Some(ReactionContent::Laugh)),
        "reactions-thinking_face" => SortKey::Reactions(Some(ReactionContent::Confused)),
        "reactions-heart" => SortKey::Reactions(Some(ReactionContent::Heart)),
        "reactions-tada" => SortKey::Reactions(Some(ReactionContent::Hooray)),
        _ => return None,
    };
    Some((
        key,
        ascending && !matches!(key, SortKey::Reactions(Some(_))),
    ))
}

fn is(value: &str) -> Option<Is> {
    Some(match value.to_ascii_lowercase().as_str() {
        "pr" => Is::Pull,
        "issue" => Is::Issue,
        "open" => Is::Open,
        "closed" => Is::Closed,
        "merged" => Is::Merged,
        "unmerged" => Is::Unmerged,
        "draft" => Is::Draft,
        "locked" => Is::Locked,
        "unlocked" => Is::Unlocked,
        "public" => Is::Public,
        "private" => Is::Private,
        "queued" => Is::Queued,
        "archived" => Is::Archived,
        _ => return None,
    })
}

fn boolean(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn person(key: &str) -> Option<Person> {
    Some(match key {
        "author" => Person::Author,
        "assignee" => Person::Assignee,
        "mentions" => Person::Mentions,
        "commenter" => Person::Commenter,
        "involves" => Person::Involves,
        "reviewed-by" => Person::ReviewedBy,
        "review-requested" => Person::ReviewRequested,
        "user-review-requested" => Person::UserReviewRequested,
        "team-review-requested-user" => Person::TeamReviewRequestedUser,
        "review-involves" => Person::ReviewInvolves,
        _ => return None,
    })
}

enum Parsed<'q> {
    Filter(Filter<'q>),
    Positive(Filter<'q>),
    Scope,
    Sort(SortKey, bool),
    Ignored,
}

impl<'q> Query<'q> {
    pub(crate) fn parse(text: &'q str) -> Query<'q> {
        let mut q = Query {
            repos: Vec::new(),
            owners: Vec::new(),
            clauses: Vec::new(),
            scope: Scope::default(),
            terms: Vec::new(),
            excluded: Vec::new(),
            sort: SortKey::Created,
            ascending: false,
        };
        let mut scoped = false;
        let mut not = false;
        for token in tokens(text) {
            if token == "NOT" {
                not = true;
                continue;
            }
            let (negated, bare) = match token.strip_prefix('-') {
                Some(rest) if rest.contains(':') => (true, rest),
                _ => (false, token),
            };
            let parsed = bare
                .split_once(':')
                .and_then(|(key, value)| q.qualifier(key, unquote(value), negated));
            match parsed {
                Some(Parsed::Filter(filter)) => q.add(negated, filter),
                Some(Parsed::Positive(filter)) => q.add(false, filter),
                Some(Parsed::Scope) => scoped = true,
                Some(Parsed::Sort(key, ascending)) => (q.sort, q.ascending) = (key, ascending),
                Some(Parsed::Ignored) => {}
                None => {
                    let term = unquote(token).to_lowercase();
                    if !term.is_empty() {
                        if std::mem::take(&mut not) {
                            q.excluded.push(term);
                        } else {
                            q.terms.push(term);
                        }
                    }
                }
            }
        }
        if !scoped {
            q.scope = Scope {
                title: true,
                body: true,
                comments: true,
            };
        }
        q
    }

    fn add(&mut self, negated: bool, filter: Filter<'q>) {
        match (negated, filter) {
            (false, Filter::Repo(name)) => self.repos.push(name),
            (false, Filter::Owner(login)) => self.owners.push(login),
            (negated, filter) => {
                if !negated
                    && let Some(clause) = self
                        .clauses
                        .iter_mut()
                        .find(|c| !c.negated && c.any[0].joins(&filter))
                {
                    clause.any.push(filter);
                } else {
                    self.clauses.push(Clause {
                        negated,
                        any: vec![filter],
                    });
                }
            }
        }
    }

    fn qualifier(&mut self, key: &str, value: &'q str, negated: bool) -> Option<Parsed<'q>> {
        let key = key.to_ascii_lowercase();
        if let Some(role) = person(&key) {
            return Some(Parsed::Filter(Filter::Person(role, value)));
        }
        let filter = match key.as_str() {
            "repo" => Filter::Repo(value),
            "org" | "user" => Filter::Owner(value),
            "head" => Filter::Head(value),
            "base" => Filter::Base(value),
            "team" => Filter::Team(value),
            "team-review-requested" => Filter::TeamReviewRequested(value),
            "label" => Filter::Label(list(value).collect()),
            "milestone" => Filter::Milestone,
            "project" => Filter::Project,
            "language" => Filter::Language(value),
            "comments" => Filter::Count(Count::Comments, count(value)?),
            "reactions" => Filter::Count(Count::Reactions, count(value)?),
            "interactions" => Filter::Count(Count::Interactions, count(value)?),
            "created" => Filter::Date(When::Created, date(value)?),
            "updated" => Filter::Date(When::Updated, date(value)?),
            "closed" => Filter::Date(When::Closed, date(value)?),
            "merged" => Filter::Date(When::Merged, date(value)?),
            "draft" => Filter::Draft(boolean(value)?),
            "archived" => Filter::Archived(boolean(value)?),
            "status" => Filter::Status(match value.to_ascii_lowercase().as_str() {
                "pending" => Status::Pending,
                "success" => Status::Success,
                "failure" => Status::Failure,
                _ => return None,
            }),
            "review" => Filter::Review(match value.to_ascii_lowercase().as_str() {
                "none" => Review::None,
                "required" => Review::Required,
                "approved" => Review::Approved,
                "changes_requested" => Review::ChangesRequested,
                _ => return None,
            }),
            "no" => {
                let missing = match value.to_ascii_lowercase().as_str() {
                    "label" => Missing::Label,
                    "assignee" => Missing::Assignee,
                    "milestone" => Missing::Milestone,
                    "project" => Missing::Project,
                    _ => return None,
                };
                if negated {
                    return Some(Parsed::Ignored);
                }
                Filter::Missing(missing)
            }
            "linked" => match value.to_ascii_lowercase().as_str() {
                "pr" => Filter::LinkedPull,
                "issue" => Filter::LinkedIssue,
                _ => return None,
            },
            "reason" => match value.to_ascii_lowercase().as_str() {
                "completed" => Filter::NotPlanned(false),
                "not planned" | "not_planned" => Filter::NotPlanned(true),
                _ => return None,
            },
            "is" => match (is(value)?, negated) {
                (v, false) | (v @ (Is::Merged | Is::Unmerged), true) => Filter::Is(v),
                (Is::Draft, true) => return Some(Parsed::Positive(Filter::Draft(false))),
                (_, true) => return Some(Parsed::Ignored),
            },
            "type" => match is(value)? {
                Is::Pull | Is::Issue if negated => return Some(Parsed::Ignored),
                v @ (Is::Pull | Is::Issue) => Filter::Is(v),
                _ => return None,
            },
            "state" => match is(value)? {
                v @ (Is::Open | Is::Closed) => Filter::Is(v),
                _ => return None,
            },
            "in" => {
                for field in list(value) {
                    match field.to_ascii_lowercase().as_str() {
                        "title" => self.scope.title = true,
                        "body" => self.scope.body = true,
                        "comments" | "comment" => self.scope.comments = true,
                        _ => {}
                    }
                }
                return Some(Parsed::Scope);
            }
            "sort" => {
                return Some(match sort_key(value) {
                    Some((key, ascending)) => Parsed::Sort(key, ascending),
                    None => Parsed::Ignored,
                });
            }
            _ => return None,
        };
        Some(Parsed::Filter(filter))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_proposals_query_the_client_sends() {
        // Given the query internal/proposals/proposals.go:385 builds
        let text = "repo:octo-org/app is:pr head:prompts/proposal- sort:updated-desc";

        // When it is parsed
        let q = Query::parse(text);

        // Then every part is a qualifier and nothing is left as a term
        assert_eq!(q.repos, ["octo-org/app"]);
        assert_eq!(
            q.clauses,
            [
                Clause {
                    negated: false,
                    any: vec![Filter::Is(Is::Pull)]
                },
                Clause {
                    negated: false,
                    any: vec![Filter::Head("prompts/proposal-")]
                },
            ]
        );
        assert_eq!((q.sort, q.ascending), (SortKey::Updated, false));
        assert_eq!(q.terms, Vec::<String>::new());
        assert_eq!(
            q.scope,
            Scope {
                title: true,
                body: true,
                comments: true
            }
        );
    }

    #[test]
    fn quoted_values_lists_and_unknown_qualifiers() {
        // Given quoted label lists, a quoted phrase, a scope and an unknown qualifier
        let text = r#"label:"good first issue",bug label:Docs "Exact Phrase" in:title color:v1 sort:created-asc"#;

        // When it is parsed
        let q = Query::parse(text);

        // Then each label qualifier is one OR group, and the unknown qualifier is a term
        assert_eq!(
            q.clauses.iter().map(|c| c.any.clone()).collect::<Vec<_>>(),
            [
                vec![Filter::Label(vec!["good first issue", "bug"])],
                vec![Filter::Label(vec!["Docs"])]
            ]
        );
        assert_eq!(q.terms, ["exact phrase", "color:v1"]);
        assert!(q.scope.title && !q.scope.body);
        assert_eq!((q.sort, q.ascending), (SortKey::Created, true));
    }

    #[test]
    fn ranges_read_as_github_documents_them() {
        // Given the range forms of the search syntax docs
        let day = |d: &str| moment(d).map(|m| m.0);
        let cases = [
            (">10", Some(11), None),
            (">=10", Some(10), None),
            ("<10", None, Some(9)),
            ("<=10", None, Some(10)),
            ("10..50", Some(10), Some(50)),
            ("10..*", Some(10), None),
            ("*..10", None, Some(10)),
            ("7", Some(7), Some(7)),
            (">9223372036854775807", Some(i64::MAX), None),
            ("<-9223372036854775808", None, Some(i64::MIN)),
        ];

        // When each is read as a count
        // Then the bounds are inclusive
        for (text, low, high) in cases {
            assert_eq!(count(text), Some(Bounds { low, high }), "{text}");
        }
        assert_eq!(count("many"), None);

        // When a date is read
        // Then a bare day covers the whole UTC day
        let d = date("2019-03-25").unwrap_or_else(|| panic!("a day"));
        assert_eq!(d.low, day("2019-03-25"));
        assert_eq!(d.high, day("2019-03-26").map(|t| t.plus_seconds(-1)));
        let after = date(">2019-03-25").unwrap_or_else(|| panic!("after a day"));
        assert_eq!(after.low, day("2019-03-26"));
        let before = date("<2019-03-25").unwrap_or_else(|| panic!("before a day"));
        assert_eq!(before.high, day("2019-03-25").map(|t| t.plus_seconds(-1)));
    }

    #[test]
    fn negation_sort_keys_and_not() {
        // Given negated qualifiers, a NOT term and a reaction sort key
        let q = Query::parse("-label:bug -author:app/robot -is:pr NOT spec sort:reactions--1-asc");

        // Then each negated qualifier is its own clause and -is:pr is dropped as GitHub drops it
        assert_eq!(
            q.clauses,
            [
                Clause {
                    negated: true,
                    any: vec![Filter::Label(vec!["bug"])]
                },
                Clause {
                    negated: true,
                    any: vec![Filter::Person(Person::Author, "app/robot")]
                },
            ]
        );
        assert_eq!(q.excluded, ["spec"]);
        assert_eq!(q.terms, Vec::<String>::new());
        assert_eq!(
            (q.sort, q.ascending),
            (SortKey::Reactions(Some(ReactionContent::ThumbsDown)), false)
        );
        assert_eq!(
            sort_key("reactions-asc"),
            Some((SortKey::Reactions(None), true))
        );
    }
}
