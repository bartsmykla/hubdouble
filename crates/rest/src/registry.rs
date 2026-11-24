use std::borrow::Cow;
use std::collections::HashMap;

use hubdouble_model::Requirement;
use hubdouble_spec::{Method, OPERATIONS, lookup};

use crate::Handler;
use crate::params::Params;

/// A registered operation.
#[derive(Clone, Copy)]
pub struct Entry {
    pub operation_id: &'static str,
    pub requirement: Requirement,
    pub handler: Handler,
    /// The handler reads its own credential, so the server skips GitHub token
    /// authentication and runs it as `Actor::Anonymous`. See [`Registry::extra_authenticating`].
    pub authenticates: bool,
    /// A `GET` that changes state, so the server runs it in a write transaction. See
    /// [`Registry::extra_writing`].
    pub writes: bool,
    /// The operation's page in GitHub's docs, empty for a route outside the `OpenAPI` bundle.
    pub documentation_url: &'static str,
}

enum Segment {
    Literal(String),
    Param(Box<str>),
}

struct Extra {
    method: Method,
    path: String,
    segments: Vec<Segment>,
    entry: Entry,
}

/// A request's operation, found by [`Registry::route`]. `entry` is `None` for an operation of
/// the table that no track implements yet.
pub struct Route<'r> {
    pub operation_id: &'static str,
    pub entry: Option<&'r Entry>,
    pub params: Params,
}

/// One row of [`Registry::operations`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationInfo<'r> {
    pub operation_id: &'static str,
    pub method: Method,
    pub path: &'r str,
    pub category: &'static str,
    /// `false` for an operation of the `OpenAPI` table that answers `501`.
    pub registered: bool,
    /// `true` for a route outside the `OpenAPI` bundle, added with [`Registry::extra`].
    pub extra: bool,
}

/// Every REST operation: the whole `OpenAPI` table, each with or without a handler, plus extras.
pub struct Registry {
    handlers: Vec<Option<Entry>>,
    by_id: HashMap<&'static str, usize>,
    extras: Vec<Extra>,
}

impl Default for Registry {
    fn default() -> Registry {
        Registry::new()
    }
}

impl Registry {
    #[must_use]
    pub fn new() -> Registry {
        Registry {
            handlers: vec![None; OPERATIONS.len()],
            by_id: OPERATIONS
                .iter()
                .enumerate()
                .map(|(i, op)| (op.id, i))
                .collect(),
            extras: Vec::new(),
        }
    }

    /// Implements an operation of the `OpenAPI` table by GitHub's ID (`pulls/create`).
    ///
    /// # Panics
    ///
    /// Panics on an unknown or already registered operation.
    pub fn handle(&mut self, operation_id: &str, req: Requirement, h: Handler) {
        let Some(&i) = self.by_id.get(operation_id) else {
            panic!("unknown operation {operation_id}");
        };
        let slot = &mut self.handlers[i];
        assert!(
            slot.is_none(),
            "operation {operation_id} is registered twice"
        );
        *slot = Some(Entry {
            operation_id: OPERATIONS[i].id,
            requirement: req,
            handler: h,
            authenticates: false,
            writes: false,
            documentation_url: OPERATIONS[i].docs,
        });
    }

    /// [`Registry::extra`] for a `GET` that GitHub answers with a state change, such as
    /// `/orgs/{org}/sso`, which records SSO and redirects.
    ///
    /// # Panics
    ///
    /// As [`Registry::extra`].
    pub fn extra_writing(
        &mut self,
        method: http::Method,
        path: &str,
        operation_id: &'static str,
        req: Requirement,
        h: Handler,
    ) {
        self.extra(method, path, operation_id, req, h);
        if let Some(last) = self.extras.last_mut() {
            last.entry.writes = true;
        }
    }

    /// [`Registry::extra`] for a route whose bearer is not a GitHub token, such as the Actions
    /// runtime token of `/_actions/token`. The handler checks the `Authorization` header itself.
    ///
    /// # Panics
    ///
    /// As [`Registry::extra`].
    pub fn extra_authenticating(
        &mut self,
        method: http::Method,
        path: &str,
        operation_id: &'static str,
        req: Requirement,
        h: Handler,
    ) {
        self.extra(method, path, operation_id, req, h);
        if let Some(last) = self.extras.last_mut() {
            last.entry.authenticates = true;
        }
    }

    /// Adds a route GitHub serves outside its REST description, under hubdouble's own operation
    /// ID (`oauth/authorize`). `path` is a template such as `/orgs/{org}/sso`.
    ///
    /// # Panics
    ///
    /// Panics on an unsupported method, an operation ID already in use, or a route already taken.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the frozen contract takes the method by value"
    )]
    pub fn extra(
        &mut self,
        method: http::Method,
        path: &str,
        operation_id: &'static str,
        req: Requirement,
        h: Handler,
    ) {
        let Some(m) = Method::parse(method.as_str()).filter(|_| method != http::Method::HEAD)
        else {
            panic!("unsupported method {method} for {operation_id}");
        };
        let taken = self.by_id.contains_key(operation_id)
            || self
                .extras
                .iter()
                .any(|e| e.entry.operation_id == operation_id);
        assert!(!taken, "operation {operation_id} is registered twice");
        assert!(
            !self.extras.iter().any(|e| e.method == m && e.path == path),
            "route {method} {path} is registered twice"
        );
        let segments = path
            .trim_start_matches('/')
            .split('/')
            .map(
                |s| match s.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                    Some(name) => Segment::Param(name.into()),
                    None => Segment::Literal(s.to_owned()),
                },
            )
            .collect();
        let entry = Entry {
            operation_id,
            requirement: req,
            handler: h,
            authenticates: false,
            writes: false,
            documentation_url: "",
        };
        self.extras.push(Extra {
            method: m,
            path: path.to_owned(),
            segments,
            entry,
        });
    }

    /// Finds the operation for a method and an escaped path (no query, no API prefix).
    #[must_use]
    pub fn route(&self, method: &http::Method, path: &str) -> Option<Route<'_>> {
        let m = Method::parse(method.as_str())?;
        if let Some(route) = self.route_extra(m, path) {
            return Some(route);
        }
        let found = lookup(m, path)?;
        let i = found.op as usize;
        let op = &OPERATIONS[i];
        let pairs = op
            .params
            .iter()
            .zip(found.values)
            .map(|(n, v)| (Cow::Borrowed(*n), v.into_owned()))
            .collect();
        Some(Route {
            operation_id: op.id,
            entry: self.handlers[i].as_ref(),
            params: Params::new(pairs),
        })
    }

    fn route_extra(&self, m: Method, path: &str) -> Option<Route<'_>> {
        let segments: Vec<&str> = path.strip_prefix('/')?.split('/').collect();
        'extras: for e in self
            .extras
            .iter()
            .filter(|e| e.method == m && e.segments.len() == segments.len())
        {
            let mut pairs = Vec::new();
            for (want, got) in e.segments.iter().zip(&segments) {
                match want {
                    Segment::Literal(l) if l == got => {}
                    Segment::Param(name) if !got.is_empty() => {
                        let Ok(value) = percent_encoding::percent_decode_str(got).decode_utf8()
                        else {
                            continue 'extras;
                        };
                        pairs.push((Cow::Owned(name.to_string()), value.into_owned()));
                    }
                    _ => continue 'extras,
                }
            }
            return Some(Route {
                operation_id: e.entry.operation_id,
                entry: Some(&e.entry),
                params: Params::new(pairs),
            });
        }
        None
    }

    /// Every operation of the table, then every extra.
    pub fn operations(&self) -> impl Iterator<Item = OperationInfo<'_>> {
        let table = OPERATIONS
            .iter()
            .zip(&self.handlers)
            .map(|(op, h)| OperationInfo {
                operation_id: op.id,
                method: op.method,
                path: op.path,
                category: op.category,
                registered: h.is_some(),
                extra: false,
            });
        let extras = self.extras.iter().map(|e| OperationInfo {
            operation_id: e.entry.operation_id,
            method: e.method,
            path: &e.path,
            category: e.entry.operation_id.split('/').next().unwrap_or_default(),
            registered: true,
            extra: true,
        });
        table.chain(extras)
    }

    /// Every registered operation with its entry, for tests that walk the registry.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.handlers
            .iter()
            .flatten()
            .chain(self.extras.iter().map(|e| &e.entry))
    }
}
