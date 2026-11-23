use std::collections::HashMap;

use url::Url;

pub(super) type SubmoduleUrls = HashMap<String, String>;

pub(super) fn parse(bytes: &[u8]) -> Option<SubmoduleUrls> {
    let config = gix_config::File::try_from(std::str::from_utf8(bytes).ok()?).ok()?;
    Some(
        config
            .sections_by_name("submodule")?
            .filter_map(|section| {
                let name = section.header().subsection_name()?;
                let path = config.string_by("submodule", Some(name), "path")?;
                let url = config.string_by("submodule", Some(name), "url")?;
                let path = String::from_utf8(path.into()).ok()?;
                let url = String::from_utf8(url.into()).ok()?;
                (!url.is_empty()).then_some((path, url))
            })
            .collect(),
    )
}

pub(super) fn github_repository(clone_url: &str, parent_web: &str) -> Option<String> {
    if clone_url.starts_with("./") || clone_url.starts_with("../") {
        let parent = Url::parse(&format!("{parent_web}/")).ok()?;
        return repository_path(parent.join(clone_url).ok()?.path());
    }
    if let Some((remote, path)) = clone_url.split_once(':')
        && remote
            .rsplit('@')
            .next()?
            .eq_ignore_ascii_case("github.com")
    {
        return repository_path(path);
    }
    let url = Url::parse(clone_url).ok()?;
    if url.host_str()?.eq_ignore_ascii_case("github.com") {
        return repository_path(url.path());
    }
    None
}

fn repository_path(path: &str) -> Option<String> {
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/')).then(|| path.to_owned())
}
