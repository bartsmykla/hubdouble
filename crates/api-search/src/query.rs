#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Is {
    Pull,
    Issue,
    Open,
    Closed,
    Merged,
    Draft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortKey {
    Created,
    Updated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Query<'q> {
    pub(crate) repos: Vec<&'q str>,
    pub(crate) owners: Vec<&'q str>,
    pub(crate) is: Vec<Is>,
    pub(crate) heads: Vec<&'q str>,
    pub(crate) bases: Vec<&'q str>,
    pub(crate) authors: Vec<&'q str>,
    pub(crate) labels: Vec<Vec<&'q str>>,
    pub(crate) in_title: bool,
    pub(crate) in_body: bool,
    pub(crate) terms: Vec<String>,
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

impl<'q> Query<'q> {
    pub(crate) fn parse(text: &'q str) -> Query<'q> {
        let mut q = Query {
            repos: Vec::new(),
            owners: Vec::new(),
            is: Vec::new(),
            heads: Vec::new(),
            bases: Vec::new(),
            authors: Vec::new(),
            labels: Vec::new(),
            in_title: false,
            in_body: false,
            terms: Vec::new(),
            sort: SortKey::Created,
            ascending: false,
        };
        let mut scoped = false;
        for token in tokens(text) {
            let known = token
                .split_once(':')
                .is_some_and(|(key, value)| q.qualifier(key, value, &mut scoped));
            if !known {
                let term = unquote(token);
                if !term.is_empty() {
                    q.terms.push(term.to_lowercase());
                }
            }
        }
        if !scoped {
            q.in_title = true;
            q.in_body = true;
        }
        q
    }

    fn qualifier(&mut self, key: &str, value: &'q str, scoped: &mut bool) -> bool {
        let value = unquote(value);
        let is = |v: &str| match v.to_ascii_lowercase().as_str() {
            "pr" => Some(Is::Pull),
            "issue" => Some(Is::Issue),
            "open" => Some(Is::Open),
            "closed" => Some(Is::Closed),
            "merged" => Some(Is::Merged),
            "draft" => Some(Is::Draft),
            _ => None,
        };
        match key.to_ascii_lowercase().as_str() {
            "repo" => self.repos.push(value),
            "org" | "user" => self.owners.push(value),
            "head" => self.heads.push(value),
            "base" => self.bases.push(value),
            "author" => self.authors.push(value),
            "label" => self.labels.push(list(value).collect()),
            "is" => match is(value) {
                Some(v) => self.is.push(v),
                None => return false,
            },
            "type" => match is(value) {
                Some(v @ (Is::Pull | Is::Issue)) => self.is.push(v),
                _ => return false,
            },
            "state" => match is(value) {
                Some(v @ (Is::Open | Is::Closed)) => self.is.push(v),
                _ => return false,
            },
            "in" => {
                *scoped = true;
                for field in list(value) {
                    match field.to_ascii_lowercase().as_str() {
                        "title" => self.in_title = true,
                        "body" => self.in_body = true,
                        _ => {}
                    }
                }
            }
            "sort" => {
                let (field, order) = value.rsplit_once('-').unwrap_or((value, "desc"));
                let sort = match field.to_ascii_lowercase().as_str() {
                    "created" => SortKey::Created,
                    "updated" => SortKey::Updated,
                    _ => return true,
                };
                self.sort = sort;
                self.ascending = order.eq_ignore_ascii_case("asc");
            }
            _ => return false,
        }
        true
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
        assert_eq!(q.is, [Is::Pull]);
        assert_eq!(q.heads, ["prompts/proposal-"]);
        assert_eq!((q.sort, q.ascending), (SortKey::Updated, false));
        assert_eq!(q.terms, Vec::<String>::new());
        assert!(q.in_title && q.in_body);
    }

    #[test]
    fn quoted_values_lists_and_unknown_qualifiers() {
        // Given quoted label lists, a quoted phrase, a scope and an unknown qualifier
        let text = r#"label:"good first issue",bug label:Docs "Exact Phrase" in:title milestone:v1 sort:created-asc"#;

        // When it is parsed
        let q = Query::parse(text);

        // Then each label qualifier is one OR group, and the unknown qualifier is a term
        assert_eq!(q.labels, [vec!["good first issue", "bug"], vec!["Docs"]]);
        assert_eq!(q.terms, ["exact phrase", "milestone:v1"]);
        assert!(q.in_title && !q.in_body);
        assert_eq!((q.sort, q.ascending), (SortKey::Created, true));
    }
}
