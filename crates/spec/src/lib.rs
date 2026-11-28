use std::borrow::Cow;

use percent_encoding::percent_decode_str;

#[rustfmt::skip]
mod ops_gen;

pub use ops_gen::{NODES, OPERATIONS};

/// The SDL that `xtask gen` writes from the two introspection pins below; it is the schema that
/// hubdouble validates and executes documents against.
pub const GRAPHQL_SCHEMA: &str = include_str!("../../../spec/graphql/schema.graphql");

/// The body of live `GET /graphql`, GitHub's full introspection answer, as `xtask graphql-pin`
/// read it, with one trailing newline.
pub const GRAPHQL_INTROSPECTION: &str = include_str!("../../../spec/graphql/introspection.json");

/// The introspection facts that only an introspection `POST /graphql` shows: the schema
/// description, `isRepeatable`, `specifiedByURL` and `isOneOf` values that differ from the
/// default, and the deprecated arguments and input fields with their live positions.
pub const GRAPHQL_INTROSPECTION_EXTRA: &str =
    include_str!("../../../spec/graphql/introspection-extra.json");

/// The path parameters GitHub allows to hold `/`; each matches one or more segments.
pub const SLASH_PARAMS: [&str; 6] = ["ref", "path", "branch", "basehead", "tag", "tree_sha"];

/// The `rest`/`param` value of a [`Node`] without that child.
pub const NONE: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    #[must_use]
    pub fn parse(method: &str) -> Option<Method> {
        Some(match method {
            "GET" | "HEAD" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "PATCH" => Method::Patch,
            "DELETE" => Method::Delete,
            _ => return None,
        })
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

/// One operation of GitHub's REST description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operation {
    /// GitHub's operation ID, such as `pulls/create`.
    pub id: &'static str,
    pub method: Method,
    /// The path template, such as `/repos/{owner}/{repo}/pulls`.
    pub path: &'static str,
    /// The template's parameter names in path order.
    pub params: &'static [&'static str],
    pub category: &'static str,
    pub subcategory: &'static str,
    /// The operation's page in GitHub's docs, which GitHub's errors give as `documentation_url`.
    pub docs: &'static str,
    /// The token families GitHub's docs accept for the operation.
    pub tokens: Tokens,
    /// `false` when GitHub's docs say the operation does not serve public resources without its
    /// permission (`public_read=no` in `docs/inventory/rest-permissions.tsv`); `true` otherwise.
    pub public_read: bool,
}

/// Which token families an operation accepts, from the pinned `spec/github-apps/access.json`.
/// An operation the docs do not describe accepts every family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tokens {
    /// An App installation token (`ghs_`).
    pub installation: bool,
    /// A user access token of a GitHub App (`ghu_`).
    pub user_to_server: bool,
    /// A fine-grained personal access token.
    pub fine_grained: bool,
}

impl Tokens {
    /// Every family, for a route outside the `OpenAPI` bundle.
    pub const ALL: Tokens = Tokens {
        installation: true,
        user_to_server: true,
        fine_grained: true,
    };
}

/// A node of the route segment tree. Index `0` is the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    /// Literal children, sorted by segment.
    pub literals: &'static [(&'static str, u32)],
    /// The child for a one-segment parameter, or [`NONE`].
    pub param: u32,
    /// The child for a parameter of [`SLASH_PARAMS`], or [`NONE`].
    pub rest: u32,
    /// The operations that end here, as indexes into [`OPERATIONS`].
    pub ops: &'static [(Method, u32)],
}

/// A route found by [`lookup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched<'p> {
    /// The index into [`OPERATIONS`].
    pub op: u32,
    /// The decoded parameter values in path order; a slash parameter joins its segments with `/`.
    pub values: Vec<Cow<'p, str>>,
}

/// Finds the operation for an escaped request path such as `/repos/o/r/branches/a%2Fb`.
///
/// The path is split on `/` before each segment is decoded, so `%2F` never separates segments.
/// A literal segment beats a parameter at the same depth, and a slash parameter takes as few
/// segments as it can, so a route with a literal suffix wins.
#[must_use]
pub fn lookup(method: Method, path: &str) -> Option<Matched<'_>> {
    let segments: Vec<&str> = path.strip_prefix('/')?.split('/').collect();
    let mut spans = Vec::with_capacity(8);
    let op = walk(0, &segments, 0, method, &mut spans)?;
    let mut values = Vec::with_capacity(spans.len());
    for (from, to) in spans {
        if to - from == 1 {
            values.push(percent_decode_str(segments[from]).decode_utf8().ok()?);
        } else {
            let mut joined = String::new();
            for (i, s) in segments[from..to].iter().enumerate() {
                if i > 0 {
                    joined.push('/');
                }
                joined.push_str(&percent_decode_str(s).decode_utf8().ok()?);
            }
            values.push(Cow::Owned(joined));
        }
    }
    Some(Matched { op, values })
}

fn walk(
    node: u32,
    segments: &[&str],
    at: usize,
    method: Method,
    spans: &mut Vec<(usize, usize)>,
) -> Option<u32> {
    let n = &NODES[node as usize];
    let Some(segment) = segments.get(at) else {
        return n.ops.iter().find(|(m, _)| *m == method).map(|(_, op)| *op);
    };
    if let Ok(i) = n.literals.binary_search_by(|(lit, _)| lit.cmp(segment))
        && let Some(op) = walk(n.literals[i].1, segments, at + 1, method, spans)
    {
        return Some(op);
    }
    if n.param != NONE && !segment.is_empty() {
        spans.push((at, at + 1));
        if let Some(op) = walk(n.param, segments, at + 1, method, spans) {
            return Some(op);
        }
        spans.pop();
    }
    if n.rest != NONE {
        for end in at + 1..=segments.len() {
            spans.push((at, end));
            if let Some(op) = walk(n.rest, segments, end, method, spans) {
                return Some(op);
            }
            spans.pop();
        }
    }
    None
}
