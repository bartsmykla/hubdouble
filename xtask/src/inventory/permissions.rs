use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tree_sitter::{Node, Parser};

use super::Result;
use super::go::{Source, arguments, assignments, children, scope, walk};
use super::seed;

const LEVELS: [&str; 3] = ["read", "write", "admin"];
const STRUCT: &str = "InstallationPermissions";
const MINT: &str = "CreateInstallationToken";
const FIELD: &str = "Permissions";
const DEPTH: usize = 8;

pub(super) struct Permissions {
    fields: BTreeMap<String, String>,
    needed: BTreeMap<String, (usize, String)>,
    constants: BTreeMap<(PathBuf, String), Option<String>>,
    carriers: BTreeSet<String>,
    packages: BTreeMap<PathBuf, Vec<PathBuf>>,
    relevant: BTreeSet<PathBuf>,
}

struct Package<'p> {
    dir: &'p Path,
    sources: &'p [Source],
}

impl Permissions {
    pub(super) fn read(go_github: &Source) -> Result<Self> {
        let fields: BTreeMap<_, _> = walk(go_github.root())
            .filter(|node| {
                node.kind() == "type_spec"
                    && node
                        .child_by_field_name("name")
                        .is_some_and(|name| go_github.text(name) == STRUCT)
            })
            .filter_map(|node| node.child_by_field_name("type"))
            .flat_map(walk)
            .filter(|node| node.kind() == "field_declaration")
            .filter_map(|field| {
                let name = go_github.text(field.child_by_field_name("name")?);
                let tag = go_github.string(field.child_by_field_name("tag")?)?;
                let json = tag.split_once("json:\"")?.1.split(['"', ',']).next()?;
                Some((name.to_owned(), json.to_owned()))
            })
            .collect();
        if fields.is_empty() {
            return Err(format!("{}: no {STRUCT} struct", go_github.path().display()).into());
        }
        Ok(Self {
            fields,
            needed: BTreeMap::new(),
            constants: BTreeMap::new(),
            carriers: BTreeSet::new(),
            packages: BTreeMap::new(),
            relevant: BTreeSet::new(),
        })
    }

    pub(super) fn collect(&mut self, source: &Source) {
        if excluded(source.path()) {
            return;
        }
        let dir = source.path().parent().unwrap_or(Path::new("")).to_owned();
        for node in walk(source.root()) {
            match node.kind() {
                "const_spec" => self.constant(source, &dir, node),
                "function_declaration" | "method_declaration" => self.mint(source, node),
                _ => (),
            }
        }
        if source.text(source.root()).contains(FIELD) {
            self.relevant.insert(dir.clone());
        }
        self.packages
            .entry(dir)
            .or_default()
            .push(source.path().to_owned());
    }

    pub(super) fn resolve(&mut self, parser: &mut Parser) -> Result<()> {
        if self.carriers.is_empty() {
            return Err(format!("no client function calls {MINT}").into());
        }
        let mut errors = Vec::new();
        for dir in std::mem::take(&mut self.relevant) {
            let sources = self.packages[&dir]
                .iter()
                .map(|path| Source::read(parser, path.clone()))
                .collect::<Result<Vec<_>>>()?;
            let package = Package {
                dir: &dir,
                sources: &sources,
            };
            for source in &sources {
                for node in walk(source.root()) {
                    if node.kind() != "composite_literal" {
                        continue;
                    }
                    let outcome = if is_struct(source, node) {
                        self.scan_struct(&package, source, node)
                    } else if self.carries(source, node) {
                        self.scan_carrier(&package, source, node)
                    } else {
                        Ok(())
                    };
                    if let Err(error) = outcome {
                        errors.push(error);
                    }
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n").into())
        }
    }

    pub(super) fn check(&self, root: &Path) -> Result<()> {
        let missing = self.missing(&seed::entries(root)?)?;
        if missing.is_empty() {
            return Ok(());
        }
        let lines: Vec<_> = missing
            .iter()
            .map(|(installation, permission, level)| {
                let location = &self.needed[permission].1;
                format!("installation {installation}: {permission}: {level} ({location})")
            })
            .collect();
        Err(format!(
            "the seed's installation lacks permissions the client mints:\n{}",
            lines.join("\n")
        )
        .into())
    }

    fn missing(&self, entries: &[Value]) -> Result<Vec<(String, String, &'static str)>> {
        let installations: Vec<_> = entries
            .iter()
            .filter(|entry| entry["kind"] == "installation")
            .collect();
        if installations.is_empty() {
            return Err("the seed has no installation".into());
        }
        let mut missing = Vec::new();
        for installation in installations {
            for (permission, (needed, _)) in &self.needed {
                let held = installation["permissions"][permission.as_str()]
                    .as_str()
                    .and_then(rank);
                if held.is_none_or(|held| held < *needed) {
                    missing.push((
                        installation["id"].to_string(),
                        permission.clone(),
                        LEVELS[*needed],
                    ));
                }
            }
        }
        Ok(missing)
    }

    fn constant(&mut self, source: &Source, dir: &Path, node: Node<'_>) {
        let Some(values) = node.child_by_field_name("value") else {
            return;
        };
        let names = children(node).filter(|child| child.kind() == "identifier");
        for (name, value) in names.zip(children(values)) {
            let value = source.string(value);
            self.constants
                .entry((dir.to_owned(), source.text(name).to_owned()))
                .and_modify(|held| {
                    if *held != value {
                        *held = None;
                    }
                })
                .or_insert(value);
        }
    }

    fn mint(&mut self, source: &Source, function: Node<'_>) {
        let calls = function.child_by_field_name("body").is_some_and(|body| {
            walk(body).any(|node| {
                node.kind() == "call_expression"
                    && node
                        .child_by_field_name("function")
                        .is_some_and(|f| last(source.text(f)) == MINT)
            })
        });
        if !calls {
            return;
        }
        let types = function
            .child_by_field_name("parameters")
            .into_iter()
            .flat_map(children)
            .filter_map(|parameter| parameter.child_by_field_name("type"))
            .map(|kind| last(source.text(kind).trim_start_matches('*')).to_owned());
        self.carriers.extend(types);
    }

    fn carries(&self, source: &Source, node: Node<'_>) -> bool {
        node.child_by_field_name("type")
            .is_some_and(|kind| self.carriers.contains(last(source.text(kind))))
    }

    fn scan_struct(
        &mut self,
        package: &Package<'_>,
        source: &Source,
        node: Node<'_>,
    ) -> std::result::Result<(), String> {
        for element in node
            .child_by_field_name("body")
            .into_iter()
            .flat_map(elements)
        {
            let read = || -> Option<(String, usize)> {
                (element.kind() == "keyed_element").then_some(())?;
                let field = source.text(element.child_by_field_name("key")?);
                let permission = self.fields.get(field)?.clone();
                let value = element.child_by_field_name("value")?;
                Some((permission, rank(&self.level(package, source, value)?)?))
            };
            let (permission, needed) = read().ok_or_else(|| {
                format!(
                    "{}: cannot read the {STRUCT} element {}",
                    source.location(element),
                    source.text(element)
                )
            })?;
            self.need(permission, needed, source.location(element));
        }
        Ok(())
    }

    fn scan_carrier(
        &mut self,
        package: &Package<'_>,
        source: &Source,
        node: Node<'_>,
    ) -> std::result::Result<(), String> {
        for element in node
            .child_by_field_name("body")
            .into_iter()
            .flat_map(elements)
            .filter(|element| element.kind() == "keyed_element")
        {
            let key = element.child_by_field_name("key").map(unwrap);
            if key.is_some_and(|key| source.text(key) == FIELD)
                && let Some(value) = element.child_by_field_name("value")
            {
                self.grant(package, source, value, 0)?;
            }
        }
        Ok(())
    }

    fn grant(
        &mut self,
        package: &Package<'_>,
        source: &Source,
        node: Node<'_>,
        depth: usize,
    ) -> std::result::Result<(), String> {
        let node = unwrap(node);
        let unresolved = || {
            format!(
                "{}: cannot resolve the grant {}",
                source.location(node),
                source.text(node)
            )
        };
        if depth > DEPTH {
            return Err(unresolved());
        }
        match node.kind() {
            "composite_literal" => match node.child_by_field_name("body") {
                Some(body) => self.pairs(package, source, body, depth),
                None => Err(unresolved()),
            },
            "literal_value" => self.pairs(package, source, node, depth),
            "unary_expression" | "index_expression" => match node.child_by_field_name("operand") {
                Some(operand) => self.grant(package, source, operand, depth + 1),
                None => Err(unresolved()),
            },
            "parenthesized_expression" => match node.named_child(0) {
                Some(inner) => self.grant(package, source, inner, depth + 1),
                None => Err(unresolved()),
            },
            "call_expression" => {
                let clone = node
                    .child_by_field_name("function")
                    .is_some_and(|f| source.text(f) == "maps.Clone");
                match arguments(node).as_slice() {
                    [argument] if clone => self.grant(package, source, *argument, depth + 1),
                    _ => Err(unresolved()),
                }
            }
            "identifier" => {
                let name = source.text(node);
                let local = assignments(source, scope(node), name, node.start_byte());
                if let Some(value) = local.last() {
                    return self.grant(package, source, *value, depth + 1);
                }
                for other in package.sources {
                    let declared = children(other.root())
                        .filter(|top| top.kind() == "var_declaration")
                        .flat_map(|top| assignments(other, top, name, usize::MAX))
                        .next();
                    if let Some(value) = declared {
                        return self.grant(package, other, value, depth + 1);
                    }
                }
                Err(unresolved())
            }
            _ => Err(unresolved()),
        }
    }

    fn pairs(
        &mut self,
        package: &Package<'_>,
        source: &Source,
        body: Node<'_>,
        depth: usize,
    ) -> std::result::Result<(), String> {
        for element in elements(body) {
            let unreadable = || {
                format!(
                    "{}: cannot read the grant element {}",
                    source.location(element),
                    source.text(element)
                )
            };
            if element.kind() != "keyed_element" {
                return Err(unreadable());
            }
            let value = element
                .child_by_field_name("value")
                .map(unwrap)
                .ok_or_else(unreadable)?;
            if matches!(value.kind(), "literal_value" | "composite_literal") {
                self.grant(package, source, value, depth + 1)?;
                continue;
            }
            let key = element
                .child_by_field_name("key")
                .and_then(|key| self.string(package, source, unwrap(key)))
                .ok_or_else(unreadable)?;
            let needed = self
                .string(package, source, value)
                .as_deref()
                .and_then(rank)
                .ok_or_else(unreadable)?;
            self.need(key, needed, source.location(element));
        }
        Ok(())
    }

    fn level(&self, package: &Package<'_>, source: &Source, node: Node<'_>) -> Option<String> {
        let node = unwrap(node);
        if node.kind() == "call_expression" {
            match arguments(node).as_slice() {
                [argument] => self.string(package, source, *argument),
                _ => None,
            }
        } else {
            self.string(package, source, node)
        }
    }

    fn string(&self, package: &Package<'_>, source: &Source, node: Node<'_>) -> Option<String> {
        match node.kind() {
            "identifier" => self
                .constants
                .get(&(package.dir.to_owned(), source.text(node).to_owned()))
                .cloned()
                .flatten(),
            "selector_expression" => {
                let (operand, field) = source.selector(node)?;
                let prefix = source.text(operand);
                let mut values = self
                    .constants
                    .iter()
                    .filter(|((dir, name), _)| {
                        name == field && dir.file_name().is_some_and(|d| d == prefix)
                    })
                    .map(|(_, value)| value.clone());
                let value = values.next()??;
                values
                    .all(|other| other.as_ref() == Some(&value))
                    .then_some(value)
            }
            _ => source.string(node),
        }
    }

    fn need(&mut self, permission: String, needed: usize, location: String) {
        let entry = self
            .needed
            .entry(permission)
            .or_insert((needed, location.clone()));
        if needed > entry.0 {
            *entry = (needed, location);
        }
    }
}

fn excluded(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_str().is_some_and(|name| name.ends_with("_test.go")))
        || path.components().any(|part| part.as_os_str() == "testdata")
}

fn last(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or(path)
}

fn is_struct(source: &Source, node: Node<'_>) -> bool {
    node.child_by_field_name("type")
        .is_some_and(|kind| last(source.text(kind)) == STRUCT)
}

fn elements(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    children(node).filter(|node| node.kind() != "comment")
}

fn unwrap(node: Node<'_>) -> Node<'_> {
    if node.kind() == "literal_element" {
        node.named_child(0).unwrap_or(node)
    } else {
        node
    }
}

fn rank(level: &str) -> Option<usize> {
    LEVELS.iter().position(|known| *known == level)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GO_GITHUB: &str = r#"package github

type InstallationPermissions struct {
	Actions  *string `json:"actions,omitempty"`
	Contents *string `json:"contents,omitempty"`
}
"#;

    const MINTER: &str = "package client\n\n\
        type Scope struct{ Permissions map[string]string }\n\n\
        func mint(want Scope) { apps.CreateInstallationToken(ctx, 1, nil) }\n";

    fn scanned(client: &str) -> Result<Permissions> {
        let dir = tempfile::tempdir().unwrap();
        let mut parser = super::super::go::parser().unwrap();
        std::fs::write(dir.path().join("apps.go"), GO_GITHUB).unwrap();
        let go_github = Source::read(&mut parser, dir.path().join("apps.go")).unwrap();
        let mut permissions = Permissions::read(&go_github).unwrap();
        let code = dir.path().join("code");
        std::fs::create_dir(&code).unwrap();
        std::fs::write(code.join("mint.go"), MINTER).unwrap();
        std::fs::write(code.join("client.go"), client).unwrap();
        for name in ["mint.go", "client.go"] {
            permissions.collect(&Source::read(&mut parser, code.join(name)).unwrap());
        }
        permissions.resolve(&mut parser)?;
        Ok(permissions)
    }

    fn missing(client: &str) -> Vec<(String, String, &'static str)> {
        let permissions = scanned(client).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        permissions.missing(&seed::entries(root).unwrap()).unwrap()
    }

    fn client(grant: &str) -> String {
        format!(
            "package client\n\n\
             var scope = &github.InstallationPermissions{{Contents: new(\"read\")}}\n\
             var floor = Scope{{Permissions: {grant}}}\n"
        )
    }

    #[test]
    fn a_fake_permission_fails_the_seed_check() {
        // Given
        let covered = client(r#"map[string]string{"actions": "write"}"#);
        let fake = client(r#"map[string]string{"actions": "write", "fake_permission": "write"}"#);

        // When
        let missing = (missing(&covered), missing(&fake));

        // Then
        assert_eq!(missing.0, []);
        assert_eq!(
            missing.1,
            [("962416".to_owned(), "fake_permission".to_owned(), "write")]
        );
    }

    #[test]
    fn a_level_above_the_seed_grant_fails_the_seed_check() {
        // Given
        let client = "package client\n\n\
                      var scope = github.InstallationPermissions{Contents: github.Ptr(\"admin\")}\n";

        // When
        let missing = missing(client);

        // Then
        assert_eq!(
            missing,
            [("962416".to_owned(), "contents".to_owned(), "admin")]
        );
    }

    #[test]
    fn a_named_constant_in_a_grant_map_is_resolved() {
        // Given a grant whose key and level are constants, reached through a cloned package map
        let client = "package client\n\n\
                      const fake = \"fake_permission\"\n\
                      const admin = \"admin\"\n\
                      var floor = map[string]string{\"actions\": \"write\", fake: admin}\n\
                      func grant() Scope { return Scope{Permissions: maps.Clone(floor)} }\n";

        // When
        let missing = missing(client);

        // Then
        assert_eq!(
            missing,
            [("962416".to_owned(), "fake_permission".to_owned(), "admin")]
        );
    }

    #[test]
    fn a_grant_map_of_only_unknown_names_is_checked() {
        // Given a map of maps indexed into a local before the grant
        let client = "package client\n\n\
                      var byDecision = map[string]map[string]string{\"a\": {\"fake_permission\": \"write\"}}\n\
                      func grant(d string) Scope {\n\
                      \tpermissions, _ := byDecision[d]\n\
                      \treturn Scope{Permissions: maps.Clone(permissions)}\n\
                      }\n";

        // When
        let missing = missing(client);

        // Then
        assert_eq!(
            missing,
            [("962416".to_owned(), "fake_permission".to_owned(), "write")]
        );
    }

    #[test]
    fn an_unresolvable_grant_fails_with_its_file_and_line() {
        // Given a grant level that no constant names, and a grant map that a call builds
        let level = client(r#"map[string]string{"actions": elsewhere.Level}"#);
        let built = client("permissionsFor(\"gate\")");

        for grant in [level, built] {
            // When
            let error = scanned(&grant).err().map(|e| e.to_string());

            // Then
            assert!(
                error.as_deref().is_some_and(|e| e.contains("client.go:4:")),
                "{error:?}"
            );
        }
    }
}
