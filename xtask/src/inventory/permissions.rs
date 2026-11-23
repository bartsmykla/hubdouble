use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use tree_sitter::Node;

use super::Result;
use super::go::{Source, arguments, children, walk};
use super::seed;

const LEVELS: [&str; 3] = ["read", "write", "admin"];
const STRUCT: &str = "InstallationPermissions";

pub(super) struct Permissions {
    fields: BTreeMap<String, String>,
    needed: BTreeMap<String, (usize, String)>,
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
        })
    }

    pub(super) fn scan(&mut self, source: &Source) -> Result<()> {
        let excluded = source
            .path()
            .file_name()
            .is_some_and(|name| name.to_str().is_some_and(|name| name.ends_with("_test.go")))
            || source
                .path()
                .components()
                .any(|part| part.as_os_str() == "testdata");
        if excluded {
            return Ok(());
        }
        for node in walk(source.root()) {
            match node.kind() {
                "composite_literal" if is_struct(source, node) => {
                    self.scan_struct(source, node)?;
                }
                "literal_value" => self.scan_map(source, node),
                _ => (),
            }
        }
        Ok(())
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

    fn scan_struct(&mut self, source: &Source, node: Node<'_>) -> Result<()> {
        for element in node
            .child_by_field_name("body")
            .into_iter()
            .flat_map(elements)
        {
            let read = || -> Option<(String, usize)> {
                (element.kind() == "keyed_element").then_some(())?;
                let field = source.text(element.child_by_field_name("key")?);
                let permission = self.fields.get(field)?.clone();
                Some((
                    permission,
                    rank(&level(source, element.child_by_field_name("value")?)?)?,
                ))
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

    fn scan_map(&mut self, source: &Source, node: Node<'_>) {
        let pairs: Option<Vec<_>> = elements(node)
            .map(|element| {
                (element.kind() == "keyed_element").then_some(())?;
                let key = source.string(unwrap(element.child_by_field_name("key")?))?;
                let level = source.string(unwrap(element.child_by_field_name("value")?))?;
                Some((key, rank(&level)?, element))
            })
            .collect();
        let Some(pairs) = pairs else { return };
        if pairs
            .iter()
            .any(|(key, _, _)| self.fields.values().any(|name| name == key))
        {
            for (permission, needed, element) in pairs {
                self.need(permission, needed, source.location(element));
            }
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

fn is_struct(source: &Source, node: Node<'_>) -> bool {
    node.child_by_field_name("type").is_some_and(|kind| {
        let kind = source.text(kind);
        kind == STRUCT || kind.ends_with(&format!(".{STRUCT}"))
    })
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

fn level(source: &Source, node: Node<'_>) -> Option<String> {
    let node = unwrap(node);
    if node.kind() == "call_expression" {
        match arguments(node).as_slice() {
            [argument] => source.string(*argument),
            _ => None,
        }
    } else {
        source.string(node)
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

    fn missing(client: &str) -> Vec<(String, String, &'static str)> {
        let dir = tempfile::tempdir().unwrap();
        let mut parser = super::super::go::parser().unwrap();
        std::fs::write(dir.path().join("apps.go"), GO_GITHUB).unwrap();
        std::fs::write(dir.path().join("client.go"), client).unwrap();
        let go_github = Source::read(&mut parser, dir.path().join("apps.go")).unwrap();
        let mut permissions = Permissions::read(&go_github).unwrap();
        let client = Source::read(&mut parser, dir.path().join("client.go")).unwrap();
        permissions.scan(&client).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        permissions.missing(&seed::entries(root).unwrap()).unwrap()
    }

    fn client(extra: &str) -> String {
        format!(
            "package client\n\n\
             var scope = &github.InstallationPermissions{{Contents: new(\"read\")}}\n\
             var floor = map[string]string{{\"actions\": \"write\"{extra}}}\n"
        )
    }

    #[test]
    fn a_fake_permission_fails_the_seed_check() {
        // Given
        let covered = client("");
        let fake = client(r#", "fake_permission": "write""#);

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
}
