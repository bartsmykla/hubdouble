use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hubdouble_auth::Actor;
use hubdouble_git::{Commit, Git, ObjectData, ObjectId, ObjectKind, Signature, SignatureState};
use hubdouble_model::node_id::{self, Part};
use hubdouble_model::{Account, ApiError, FieldError, Repo, Timestamp};
use hubdouble_render::{SimpleUser, Urls};
use hubdouble_rest::Ctx;
use hubdouble_store::{Accounts, Repos, Tx};
use serde::Serialize;

const BASE64_LINE: usize = 60;
const MAX_FORK_DEPTH: usize = 64;

pub(crate) struct Links {
    pub api: String,
    pub web: String,
}

impl Links {
    pub fn new(tx: &Tx, urls: &Urls, repo: &Repo) -> Result<Links, ApiError> {
        let owner = tx
            .table::<Accounts>()
            .get(repo.owner_id)
            .ok_or_else(ApiError::not_found)?;
        let full_name = format!("{}/{}", owner.login, repo.name);
        Ok(Links {
            api: format!("{}/repos/{full_name}", urls.api),
            web: format!("{}/{full_name}", urls.web),
        })
    }

    pub fn of(ctx: &Ctx<'_>) -> Result<Links, ApiError> {
        Links::new(ctx.tx(), ctx.urls(), ctx.repo()?)
    }

    pub fn object(&self, kind: ObjectKind, id: ObjectId) -> String {
        let dir = match kind {
            ObjectKind::Blob => "blobs",
            ObjectKind::Tree => "trees",
            ObjectKind::Commit => "commits",
            ObjectKind::Tag => "tags",
        };
        format!("{}/git/{dir}/{id}", self.api)
    }
}

pub(crate) fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

fn object_prefix(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "B",
        ObjectKind::Tree => "TR",
        ObjectKind::Commit => "C",
        ObjectKind::Tag => "TAG",
    }
}

pub(crate) fn object_node_id(kind: ObjectKind, repo_id: i64, id: ObjectId) -> String {
    node_id::encode(
        object_prefix(kind),
        &[Part::Int(0), Part::Int(repo_id), Part::Str(id.to_string())],
    )
}

pub(crate) fn ref_node_id(repo_id: i64, name: &str) -> String {
    node_id::encode(
        "REF",
        &[Part::Int(0), Part::Int(repo_id), Part::Str(name.to_owned())],
    )
}

/// Base64 in lines of 60 characters, each ending in a newline, as GitHub writes the `content`
/// of a blob or a file.
#[must_use]
pub fn wrapped_base64(data: &[u8]) -> String {
    let encoded = STANDARD.encode(data);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / BASE64_LINE + 1);
    for line in encoded.as_bytes().chunks(BASE64_LINE) {
        out.push_str(&String::from_utf8_lossy(line));
        out.push('\n');
    }
    out
}

/// Decodes base64 that may hold whitespace and line breaks, as a client sends a blob or file;
/// `None` when it is not base64.
#[must_use]
pub fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD.decode(compact).ok()
}

pub(crate) fn accepts(ctx: &Ctx<'_>, name: &str) -> bool {
    ctx.header("accept").is_some_and(|accept| {
        accept
            .split(',')
            .filter_map(|r| r.split(';').next())
            .map(|r| r.trim().trim_end_matches("+json"))
            .filter_map(|r| r.strip_prefix("application/vnd.github."))
            .any(|r| r == name || r.strip_prefix("v3.") == Some(name))
    })
}

pub(crate) fn unprocessable(resource: &str, field: &str, message: &str) -> ApiError {
    ApiError::unprocessable(message).with_error(FieldError {
        resource: resource.to_owned(),
        field: field.to_owned(),
        code: "invalid".to_owned(),
        message: Some(message.to_owned()),
        value: None,
    })
}

pub(crate) fn parse_oid(text: &str) -> Option<ObjectId> {
    text.parse().ok()
}

pub(crate) fn sender(actor: &Actor) -> Result<&Arc<Account>, ApiError> {
    actor
        .account()
        .ok_or_else(|| ApiError::forbidden("Resource not accessible by integration"))
}

pub(crate) fn signature_of(account: &Account, time: Timestamp) -> Signature {
    Signature {
        name: account
            .name
            .clone()
            .unwrap_or_else(|| account.login.clone()),
        email: account.email.clone().unwrap_or_else(|| {
            format!("{}+{}@users.noreply.github.com", account.id, account.login)
        }),
        time,
        offset_minutes: 0,
    }
}

pub(crate) fn account_by_email<'t>(tx: &'t Tx, email: &str) -> Option<&'t Arc<Account>> {
    let local = email.strip_suffix("@users.noreply.github.com");
    let login = local.map(|l| l.split_once('+').map_or(l, |(_, login)| login));
    tx.table::<Accounts>().iter().find(|a| {
        a.email
            .as_deref()
            .is_some_and(|e| e.eq_ignore_ascii_case(email))
            || login.is_some_and(|l| a.login.eq_ignore_ascii_case(l))
    })
}

fn network_root(tx: &Tx, repo: i64) -> i64 {
    let repos = tx.table::<Repos>();
    let mut id = repo;
    for _ in 0..MAX_FORK_DEPTH {
        match repos.get(id).and_then(|r| r.fork_of) {
            Some(parent) if parent != id => id = parent,
            _ => break,
        }
    }
    id
}

pub(crate) fn network_repo<'t>(tx: &'t Tx, repo: &Repo, owner: &str) -> Option<&'t Arc<Repo>> {
    let account = tx.table::<Accounts>().by_login(owner)?;
    let root = network_root(tx, repo.id);
    tx.table::<Repos>()
        .iter()
        .find(|r| r.owner_id == account.id && network_root(tx, r.id) == root)
}

pub(crate) fn commit_at(
    git: &Git,
    tx: &Tx,
    repo: i64,
    rev: &str,
) -> Option<(ObjectId, Arc<Commit>)> {
    let object = git.expression(tx, repo, rev).ok()?;
    let id = git.peel_to_commit(tx, repo, object.id).ok()?;
    match git.object(tx, repo, id).ok()?.data {
        ObjectData::Commit(c) => Some((id, c)),
        _ => None,
    }
}

#[derive(Serialize)]
pub(crate) struct GitPerson {
    pub name: String,
    pub email: String,
    pub date: Timestamp,
}

impl From<&Signature> for GitPerson {
    fn from(s: &Signature) -> GitPerson {
        GitPerson {
            name: s.name.clone(),
            email: s.email.clone(),
            date: s.time,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct Verification {
    pub verified: bool,
    pub reason: &'static str,
    pub signature: Option<String>,
    pub payload: Option<String>,
    pub verified_at: Option<Timestamp>,
}

impl Verification {
    pub fn of_commit(git: &Git, repo: i64, id: ObjectId, commit: &Commit) -> Verification {
        let state = git.verification(commit);
        let node = git.signature(repo, id, commit);
        let verified = state == SignatureState::Valid;
        Verification {
            verified,
            reason: state.as_str(),
            signature: node.as_ref().map(|n| n.signature.clone()),
            payload: node.map(|n| n.payload),
            verified_at: verified.then_some(commit.committer.time),
        }
    }

    pub fn unsigned() -> Verification {
        Verification {
            verified: false,
            reason: SignatureState::Unsigned.as_str(),
            signature: None,
            payload: None,
            verified_at: None,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct ObjectLink {
    pub sha: ObjectId,
    pub url: String,
}

#[derive(Serialize)]
pub(crate) struct ParentLink {
    pub sha: ObjectId,
    pub url: String,
    pub html_url: String,
}

pub(crate) fn user_of(tx: &Tx, urls: &Urls, s: &Signature) -> Option<SimpleUser> {
    account_by_email(tx, &s.email).map(|a| SimpleUser::new(urls, a))
}
