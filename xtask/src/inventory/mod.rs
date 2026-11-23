mod go;
mod graphql;
mod permissions;
mod rest;
mod seed;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const SMOKE_REST_ROUTES: &[&str] = &[
    "GET /app",
    "GET /app/hook/deliveries",
    "GET /app/hook/deliveries/{delivery_id}",
    "GET /repos/{owner}/{repo}/actions/workflows/{workflow_id}/runs",
    "GET /repos/{owner}/{repo}/issues/comments/{comment_id}/reactions",
    "POST /repos/{owner}/{repo}/issues/{issue_number}/comments",
];

pub(super) fn run(root: &Path, args: &[String]) -> Result<()> {
    let (checkout, names, check) = options(args)?;
    let names = match names {
        Some(path) => parse_names(&fs::read_to_string(path)?)?,
        None => Vec::new(),
    };
    let (module, version) = module_version(&fs::read_to_string(checkout.join("go.mod"))?)?;
    let module = cache_module(root, &module, &version)?;
    let mut parser = go::parser()?;
    seed::check(root, &checkout, &mut parser, &names)?;
    let catalog = rest::Catalog::read(&mut parser, go::files(&module.join("github"))?)?;
    let go_github = go::Source::read(&mut parser, module.join("github/apps.go"))?;
    let mut permissions = permissions::Permissions::read(&go_github)?;
    let paths = rest::paths(root)?;
    let mut routes = BTreeSet::new();
    let mut documents = BTreeMap::new();
    let mut errors = Vec::new();
    for path in go::files(&checkout)? {
        match go::Source::read(&mut parser, path) {
            Ok(source) => {
                for outcome in [
                    catalog.scan(&source, &paths, &mut routes),
                    graphql::scan(&source, &mut documents),
                ] {
                    if let Err(error) = outcome {
                        errors.push(error.to_string());
                    }
                }
                permissions.collect(&source);
            }
            Err(error) => errors.push(error.to_string()),
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("\n").into());
    }
    permissions.resolve(&mut parser)?;
    permissions.check(root)?;
    let inventory = root.join("docs/inventory");
    let fields = graphql::fields(&inventory, &documents)?;
    let mut output = BTreeMap::from([
        (PathBuf::from("rest-routes.txt"), route_list(routes)),
        (PathBuf::from("graphql-fields.tsv"), fields),
    ]);
    output.extend(documents.into_iter().map(|(name, document)| {
        (
            Path::new("graphql").join(format!("{name}.graphql")),
            document,
        )
    }));
    for content in output.values_mut() {
        *content = rename(content, &names);
    }
    save(&inventory, &output, check)?;
    eprintln!("xtask: {} GraphQL documents", output.len() - 2);
    Ok(())
}

fn options(args: &[String]) -> Result<(PathBuf, Option<PathBuf>, bool)> {
    let mut check = false;
    let mut checkout = None;
    let mut names = None;
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        match arg {
            "--check" if !check => check = true,
            "--client" if checkout.is_none() => checkout = args.next().map(PathBuf::from),
            "--names" if names.is_none() => {
                names = Some(args.next().ok_or("--names requires <file>")?.into());
            }
            _ => return Err(format!("unknown inventory argument: {arg}").into()),
        }
    }
    Ok((
        checkout.ok_or("inventory requires --client <path>")?,
        names,
        check,
    ))
}

fn parse_names(text: &str) -> Result<Vec<(String, String)>> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| match line.split_once("==>") {
            Some((from, to)) if !from.is_empty() => Ok((from.to_owned(), to.to_owned())),
            _ => Err(format!("names: expected a from==>to line: {line:?}").into()),
        })
        .collect()
}

fn rename(text: &str, names: &[(String, String)]) -> String {
    names
        .iter()
        .fold(text.to_owned(), |text, (from, to)| text.replace(from, to))
}

fn module_version(gomod: &str) -> Result<(String, String)> {
    let entries: Vec<_> = gomod
        .lines()
        .filter_map(|line| {
            let line = line.split("//").next()?;
            let mut words = line.split_whitespace().peekable();
            if words.peek() == Some(&"require") {
                words.next();
            }
            let module = words.next()?;
            let major = module
                .strip_prefix("github.com/google/go-github/v")?
                .parse::<u32>()
                .ok()?;
            let version = words.next()?;
            (version.starts_with(&format!("v{major}."))
                && version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".+-".contains(&b)))
            .then_some((module.to_owned(), version.to_owned()))
        })
        .collect();
    match entries.as_slice() {
        [entry] => Ok(entry.clone()),
        _ => Err("go.mod must pin one version of github.com/google/go-github".into()),
    }
}

fn cache_module(root: &Path, module: &str, version: &str) -> Result<PathBuf> {
    let cache = root.join("target/go-github");
    let extracted = cache.join(format!("{module}@{version}"));
    if !extracted.join("github/github.go").is_file() {
        fs::create_dir_all(&cache)?;
        let archive = cache.join(format!("{version}.zip"));
        if !archive.is_file() {
            let partial = archive.with_extension("part");
            let url = format!("https://proxy.golang.org/{module}/@v/{version}.zip");
            command(
                Command::new("curl")
                    .args(["-fsSL", "--output"])
                    .arg(&partial)
                    .arg(url),
            )?;
            fs::rename(partial, &archive)?;
        }
        command(
            Command::new("unzip")
                .arg("-oq")
                .arg(archive)
                .arg("-d")
                .arg(&cache),
        )?;
    }
    Ok(extracted)
}

fn command(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        return Err(format!("{command:?}: {status}").into());
    }
    Ok(())
}

fn route_list(mut routes: BTreeSet<String>) -> String {
    routes.extend(SMOKE_REST_ROUTES.iter().map(|route| (*route).to_owned()));
    let mut routes: Vec<_> = routes.into_iter().collect();
    routes.sort_unstable_by(|a, b| route_key(a).cmp(&route_key(b)));
    routes.join("\n") + "\n"
}

fn route_key(route: &str) -> (&str, &str) {
    let (method, path) = route.split_once(' ').unwrap_or_default();
    (path, method)
}

fn save(root: &Path, output: &BTreeMap<PathBuf, String>, check: bool) -> Result<()> {
    let mut changes = Vec::new();
    for (relative, content) in output {
        let path = root.join(relative);
        match fs::read_to_string(&path) {
            Ok(old) if old == *content => continue,
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        if check {
            changes.push(relative.clone());
        } else {
            fs::write(path, content)?;
        }
    }
    for entry in fs::read_dir(root.join("graphql"))? {
        let entry = entry?;
        let relative = Path::new("graphql").join(entry.file_name());
        if relative.extension().is_some_and(|ext| ext == "graphql")
            && !output.contains_key(&relative)
        {
            if check {
                changes.push(relative);
            } else {
                fs::remove_file(entry.path())?;
            }
        }
    }
    changes.sort_unstable();
    if changes.is_empty() {
        Ok(())
    } else {
        Err(format!("inventory differs: {changes:?}").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_inventory_contains_the_smoke_reactions_route() {
        let route = "GET /repos/{owner}/{repo}/issues/comments/{comment_id}/reactions";
        let generated = route_list(BTreeSet::from(["GET /app".to_owned()]));
        assert!(generated.lines().any(|line| line == route));
    }

    #[test]
    fn names_apply_in_file_order_and_reject_lines_without_a_source() {
        let names = parse_names("refs/tags/old/==>refs/tags/mid/\n\nmid==>new\n").unwrap();
        assert_eq!(
            rename(r#"refs(refPrefix: "refs/tags/old/")"#, &names),
            r#"refs(refPrefix: "refs/tags/new/")"#
        );
        assert!(parse_names("no separator\n").is_err());
        assert!(parse_names("==>empty source\n").is_err());
    }

    #[test]
    fn check_reports_added_changed_and_removed_files_without_writes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("graphql")).unwrap();
        fs::write(root.join("graphql/old.graphql"), "old").unwrap();
        fs::write(root.join("rest-routes.txt"), "old").unwrap();
        let expected = BTreeMap::from([
            (PathBuf::from("graphql/new.graphql"), "new".to_owned()),
            (PathBuf::from("rest-routes.txt"), "new".to_owned()),
        ]);
        assert!(save(root, &expected, true).is_err());
        assert_eq!(
            fs::read_to_string(root.join("rest-routes.txt")).unwrap(),
            "old"
        );
        assert!(root.join("graphql/old.graphql").exists());
        assert!(!root.join("graphql/new.graphql").exists());
        save(root, &expected, false).unwrap();
        assert!(save(root, &expected, true).is_ok());
        assert!(!root.join("graphql/old.graphql").exists());
    }
}
