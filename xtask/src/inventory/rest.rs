use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use tree_sitter::Node;

use super::Result;
use super::go::{Source, arguments, assignments, binding_lists, children, scope, walk};

const PREFIX: &str = "//meta:operation ";
const EXTRAS: &[&str] = &[
    "POST /graphql",
    "POST /login/device/code",
    "POST /login/oauth/access_token",
    "GET /login/oauth/authorize",
    "GET /repositories/{repository_id}",
];

type ServiceAliases<'a> = BTreeMap<(usize, &'a str), Vec<(usize, &'a str)>>;

#[derive(Default)]
pub(super) struct Catalog {
    methods: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

impl Catalog {
    pub(super) fn read(parser: &mut tree_sitter::Parser, files: Vec<PathBuf>) -> Result<Self> {
        let mut this = Self::default();
        for path in files {
            if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("_test.go"))
            {
                continue;
            }
            let source = Source::read(parser, path)?;
            let mut routes = Vec::new();
            for node in children(source.root()) {
                match node.kind() {
                    "comment" => {
                        if let Some(route) = source.text(node).strip_prefix(PREFIX) {
                            routes.push(route.to_owned());
                        }
                    }
                    "method_declaration" if !routes.is_empty() => {
                        this.insert(&source, node, std::mem::take(&mut routes))?;
                    }
                    _ => routes.clear(),
                }
            }
        }
        Ok(this)
    }

    fn insert(&mut self, source: &Source, node: Node<'_>, routes: Vec<String>) -> Result<()> {
        let receiver = node
            .child_by_field_name("receiver")
            .ok_or("missing receiver")?;
        let kind = walk(receiver)
            .find(|child| child.kind() == "type_identifier")
            .ok_or("missing receiver type")?;
        let Some(service) = source.text(kind).strip_suffix("Service") else {
            return Ok(());
        };
        let name = node
            .child_by_field_name("name")
            .ok_or("missing method name")?;
        self.methods
            .entry(service.to_owned())
            .or_default()
            .insert(source.text(name).to_owned(), routes);
        Ok(())
    }

    fn service_routes<'a>(&'a self, service: &str, method: &str) -> Option<&'a [String]> {
        self.methods.get(service)?.get(method).map(Vec::as_slice)
    }

    pub(super) fn scan(
        &self,
        source: &Source,
        paths: &BTreeSet<String>,
        routes: &mut BTreeSet<String>,
    ) -> Result<()> {
        let aliases = self.aliases(source);
        for call in walk(source.root()).filter(|node| node.kind() == "call_expression") {
            let Some(function) = call.child_by_field_name("function") else {
                continue;
            };
            let Some((receiver, method)) = source.selector(function) else {
                continue;
            };
            if let Some(service) = self.service(source, receiver, method, &aliases) {
                let candidates = self.service_routes(service, method).ok_or_else(|| {
                    format!(
                        "{}: no operation for {service}.{method}",
                        source.location(call)
                    )
                })?;
                let args = arguments(call);
                routes.extend(
                    choose(source, call, service, method, candidates, &args)?
                        .into_iter()
                        .cloned(),
                );
            }
            if method == "NewRequest" && !matches!(source.text(receiver), "http" | "httptest") {
                scan_request(source, call, paths, routes)?;
            }
            if method == "post"
                && let Some(path) = arguments(call).get(1).and_then(|node| source.string(*node))
                && path.starts_with("/login/")
            {
                routes.insert(match_path(paths, "POST", &path)?);
            }
        }
        if source.path().ends_with("accounts.go") {
            for node in walk(source.root()) {
                if source
                    .string(node)
                    .is_some_and(|path| path.starts_with("/login/oauth/authorize?"))
                {
                    routes.insert(match_path(paths, "GET", "/login/oauth/authorize")?);
                }
            }
        }
        Ok(())
    }

    fn aliases<'a>(&self, source: &'a Source) -> ServiceAliases<'a> {
        let mut found = ServiceAliases::new();
        for (left, right) in walk(source.root()).filter_map(binding_lists) {
            for (name, value) in children(left).zip(children(right)) {
                if let Some((client, service)) = source.selector(value)
                    && self.methods.contains_key(service)
                    && client_receiver(source, client)
                {
                    let key = (scope(name).start_byte(), source.text(name));
                    found
                        .entry(key)
                        .or_default()
                        .push((name.start_byte(), service));
                }
            }
        }
        found
    }

    fn service<'a>(
        &self,
        source: &'a Source,
        receiver: Node<'_>,
        method: &str,
        aliases: &ServiceAliases<'a>,
    ) -> Option<&'a str> {
        if let Some((client, service)) = source.selector(receiver) {
            return (self.methods.contains_key(service)
                && (self.service_routes(service, method).is_some()
                    || client_receiver(source, client)))
            .then_some(service);
        }
        let name = source.text(receiver);
        let local = (scope(receiver).start_byte(), name);
        aliases
            .get(&local)
            .or_else(|| aliases.get(&(source.root().start_byte(), name)))?
            .iter()
            .rev()
            .find(|(before, _)| *before < receiver.start_byte())
            .map(|(_, service)| *service)
    }
}

fn client_receiver(source: &Source, receiver: Node<'_>) -> bool {
    source
        .text(receiver)
        .rsplit('.')
        .next()
        .is_some_and(|name| matches!(name, "client" | "github" | "gh"))
        || (receiver.kind() == "identifier"
            && walk(scope(receiver)).any(|node| {
                matches!(node.kind(), "parameter_declaration" | "var_spec")
                    && node
                        .child_by_field_name("type")
                        .is_some_and(|ty| source.text(ty) == "*github.Client")
                    && children(node).any(|name| source.text(name) == source.text(receiver))
            }))
}

fn choose<'a>(
    source: &Source,
    call: Node<'_>,
    service: &str,
    method: &str,
    routes: &'a [String],
    args: &[Node<'_>],
) -> Result<Vec<&'a String>> {
    if routes.len() == 1 {
        return Ok(routes.iter().collect());
    }
    let (index, parameter) = match (service, method) {
        ("Issues", "ListComments") => (3, "{issue_number}"),
        ("PullRequests", "ListComments") => (3, "{pull_number}"),
        ("Apps", "Get") => (1, "{app_slug}"),
        ("Users", "Get") => (1, "{username}"),
        _ => return Err(format!("{}: ambiguous {service}.{method}", source.location(call)).into()),
    };
    let argument = *args.get(index).ok_or("missing route selector argument")?;
    let empty = match argument.kind() {
        "int_literal" => Some(source.text(argument) == "0"),
        "interpreted_string_literal" | "raw_string_literal" => {
            source.string(argument).map(|s| s.is_empty())
        }
        _ => None,
    };
    Ok(routes
        .iter()
        .filter(|route| empty.is_none_or(|empty| empty != route.contains(parameter)))
        .collect())
}

pub(super) fn paths(root: &Path) -> Result<BTreeSet<String>> {
    let bundle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join(crate::BUNDLE))?)?;
    Ok(crate::operations(&bundle)?
        .into_iter()
        .map(|op| format!("{} {}", crate::METHODS[op.method].0.to_uppercase(), op.path))
        .chain(EXTRAS.iter().map(|route| (*route).to_owned()))
        .collect())
}

fn scan_request(
    source: &Source,
    call: Node<'_>,
    paths: &BTreeSet<String>,
    routes: &mut BTreeSet<String>,
) -> Result<()> {
    let args = arguments(call);
    let [_, method, path, _] = args.as_slice() else {
        return Ok(());
    };
    let methods = values(source, *method, 0);
    let patterns = values(source, *path, 0);
    let mut found = BTreeSet::new();
    for pattern in &patterns {
        let mut matches = false;
        for method in &methods {
            if let Ok(route) = match_path(paths, method, pattern) {
                found.insert(route);
                matches = true;
            }
        }
        if !matches {
            return Err(format!(
                "{}: no operation for {methods:?} {pattern}",
                source.location(call)
            )
            .into());
        }
    }
    if found.is_empty() {
        return Err(format!("{}: unresolved NewRequest", source.location(call)).into());
    }
    routes.extend(found);
    Ok(())
}

pub(super) fn values(source: &Source, node: Node<'_>, depth: u8) -> Vec<String> {
    if let Some(literal) = source.string(node) {
        return vec![literal];
    }
    if depth == 10 {
        return Vec::new();
    }
    match node.kind() {
        "selector_expression" => source
            .text(node)
            .strip_prefix("http.Method")
            .map(str::to_uppercase)
            .into_iter()
            .collect(),
        "binary_expression" => node
            .child_by_field_name("left")
            .map_or_else(Vec::new, |left| values(source, left, depth + 1)),
        "call_expression"
            if node
                .child_by_field_name("function")
                .is_some_and(|f| source.text(f) == "fmt.Sprintf") =>
        {
            arguments(node)
                .first()
                .and_then(|first| source.string(*first))
                .into_iter()
                .collect()
        }
        "identifier" => {
            let before = node.start_byte();
            let name = source.text(node);
            assignments(source, scope(node), name, before)
                .into_iter()
                .flat_map(|value| values(source, value, depth + 1))
                .filter(|value| !value.starts_with(['&', '?']))
                .collect()
        }
        _ => Vec::new(),
    }
}

fn match_path(paths: &BTreeSet<String>, method: &str, pattern: &str) -> Result<String> {
    let path = pattern
        .split('?')
        .next()
        .unwrap_or_default()
        .trim_matches('/');
    let literal = format!("{method} /{path}");
    if paths.contains(&literal) {
        return Ok(literal);
    }
    let segments: Vec<_> = path.split('/').collect();
    let mut candidates = paths.iter().filter(|route| {
        let Some((verb, path)) = route.split_once(' ') else {
            return false;
        };
        verb == method
            && segments.len() == path.trim_matches('/').split('/').count()
            && segments
                .iter()
                .zip(path.trim_matches('/').split('/'))
                .all(|(seen, expected)| {
                    if expected.starts_with('{') {
                        seen.starts_with('%')
                    } else {
                        *seen == expected
                    }
                })
    });
    match (candidates.next(), candidates.next()) {
        (Some(route), None) => Ok(route.clone()),
        _ => Err(format!("no unique operation for {literal}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(client: &str) -> Result<BTreeSet<String>> {
        let dir = tempfile::tempdir()?;
        let upstream = dir.path().join("issues.go");
        std::fs::write(
            &upstream,
            "package github\n//meta:operation GET /repos/{owner}/{repo}/issues/comments\n//meta:operation GET /repos/{owner}/{repo}/issues/{issue_number}/comments\nfunc (s *IssuesService) ListComments() {}\n",
        )?;
        let file = dir.path().join("client.go");
        std::fs::write(
            &file,
            format!("package client\nfunc call() {{ {client} }}\n"),
        )?;
        let mut parser = super::super::go::parser()?;
        let catalog = Catalog::read(&mut parser, vec![upstream])?;
        let source = Source::read(&mut parser, file)?;
        let mut routes = BTreeSet::new();
        catalog.scan(&source, &BTreeSet::new(), &mut routes)?;
        Ok(routes)
    }

    #[test]
    fn zero_selects_repository_comments_and_a_number_selects_issue_comments() {
        for (selector, path) in [
            ("0", "issues/comments"),
            ("7", "issues/{issue_number}/comments"),
        ] {
            let call = format!("client.Issues.ListComments(ctx, owner, repo, {selector}, nil)");
            let actual = scan(&call).unwrap();
            assert_eq!(
                actual,
                BTreeSet::from([format!("GET /repos/{{owner}}/{{repo}}/{path}")])
            );
        }
    }

    #[test]
    fn an_unmapped_service_call_fails_the_scan() {
        assert!(scan("client.Issues.Unmapped(ctx)").is_err());
        assert!(scan("issues := client.Issues; issues.Unmapped(ctx)").is_err());
    }
}
