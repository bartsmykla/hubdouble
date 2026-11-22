use hubdouble_model as m;
use serde::Serialize;

use crate::{SimpleUser, Urls, escape};

/// GitHub's `release-asset`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReleaseAsset {
    pub url: String,
    pub browser_download_url: String,
    pub id: i64,
    pub node_id: String,
    pub name: String,
    pub label: Option<String>,
    pub state: &'static str,
    pub content_type: String,
    pub size: i64,
    pub digest: Option<String>,
    pub download_count: i64,
    pub created_at: m::Timestamp,
    pub updated_at: m::Timestamp,
    pub uploader: Option<SimpleUser>,
}

impl ReleaseAsset {
    #[must_use]
    pub fn new(
        urls: &Urls,
        a: &m::ReleaseAsset,
        full_name: &str,
        tag: &str,
        uploader: Option<SimpleUser>,
    ) -> ReleaseAsset {
        ReleaseAsset {
            url: format!("{}/repos/{full_name}/releases/assets/{}", urls.api, a.id),
            browser_download_url: format!(
                "{}/{full_name}/releases/download/{}/{}",
                urls.web,
                escape(tag),
                escape(&a.name)
            ),
            id: a.id,
            node_id: a.node_id.clone(),
            name: a.name.clone(),
            label: a.label.clone(),
            state: a.state.as_str(),
            content_type: a.content_type.clone(),
            size: a.size,
            digest: a.digest.clone(),
            download_count: a.download_count,
            created_at: a.created_at,
            updated_at: a.updated_at,
            uploader,
        }
    }
}

/// GitHub's `release`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Release {
    pub url: String,
    pub html_url: String,
    pub assets_url: String,
    pub upload_url: String,
    pub tarball_url: Option<String>,
    pub zipball_url: Option<String>,
    pub id: i64,
    pub node_id: String,
    pub tag_name: String,
    pub target_commitish: String,
    pub name: Option<String>,
    pub body: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub immutable: bool,
    pub created_at: m::Timestamp,
    pub published_at: Option<m::Timestamp>,
    pub updated_at: Option<m::Timestamp>,
    pub author: SimpleUser,
    pub assets: Vec<ReleaseAsset>,
}

impl Release {
    #[must_use]
    pub fn new(
        urls: &Urls,
        r: &m::Release,
        full_name: &str,
        author: SimpleUser,
        assets: Vec<ReleaseAsset>,
    ) -> Release {
        let api = format!("{}/repos/{full_name}/releases/{}", urls.api, r.id);
        let tag = escape(&r.tag_name);
        Release {
            html_url: format!("{}/{full_name}/releases/tag/{tag}", urls.web),
            assets_url: format!("{api}/assets"),
            upload_url: format!("{api}/assets{{?name,label}}"),
            tarball_url: Some(format!("{}/repos/{full_name}/tarball/{tag}", urls.api)),
            zipball_url: Some(format!("{}/repos/{full_name}/zipball/{tag}", urls.api)),
            url: api,
            id: r.id,
            node_id: r.node_id.clone(),
            tag_name: r.tag_name.clone(),
            target_commitish: r.target_commitish.clone(),
            name: r.name.clone(),
            body: r.body.clone(),
            draft: r.draft,
            prerelease: r.prerelease,
            immutable: false,
            created_at: r.created_at,
            published_at: r.published_at,
            updated_at: Some(r.updated_at),
            author,
            assets,
        }
    }
}
