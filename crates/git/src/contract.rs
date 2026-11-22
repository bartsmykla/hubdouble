use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use hubdouble_model::{ApiError, Timestamp};
use hubdouble_store::{StoreBuilder, Tx, TxMut};
use serde::{Serialize, Serializer};

/// A SHA-1 git object ID.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; 20]);

impl ObjectId {
    /// The all-zero ID git writes for a missing side of a ref change.
    pub const NULL: ObjectId = ObjectId([0; 20]);
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({self})")
    }
}

impl FromStr for ObjectId {
    type Err = GitError;

    /// Parses 40 hex digits, either case.
    fn from_str(s: &str) -> Result<ObjectId, GitError> {
        let bytes = s.as_bytes();
        if bytes.len() != 40 {
            return Err(GitError::BadObjectId);
        }
        let mut out = [0u8; 20];
        let hex = |c: u8| char::from(c).to_digit(16).ok_or(GitError::BadObjectId);
        for (o, [hi, lo]) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
            *o = u8::try_from(hex(*hi)? * 16 + hex(*lo)?).map_err(|_| GitError::BadObjectId)?;
        }
        Ok(ObjectId(out))
    }
}

impl Serialize for ObjectId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ObjectKind {
    Blob,
    Tree,
    Commit,
    Tag,
}

/// A tree entry mode, written as git writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryMode {
    /// `100644`
    File,
    /// `100755`
    Executable,
    /// `120000`
    Symlink,
    /// `040000`
    Tree,
    /// `160000`
    Submodule,
}

impl EntryMode {
    /// The octal text REST and GraphQL answer (`100644`, `040000`, ...).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EntryMode::File => "100644",
            EntryMode::Executable => "100755",
            EntryMode::Symlink => "120000",
            EntryMode::Tree => "040000",
            EntryMode::Submodule => "160000",
        }
    }
}

/// A person and time in a commit or tag; GraphQL's `GitActor`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub time: Timestamp,
    /// The time zone offset in minutes east of UTC.
    pub offset_minutes: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    pub mode: EntryMode,
    pub id: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: Signature,
    pub committer: Signature,
    pub message: String,
    /// The armored `gpgsig` header, when the commit is signed.
    pub gpg_signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub target: ObjectId,
    pub target_kind: ObjectKind,
    pub name: String,
    pub tagger: Option<Signature>,
    pub message: String,
    pub gpg_signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectData {
    Blob(Arc<[u8]>),
    Tree(Arc<[TreeEntry]>),
    Commit(Arc<Commit>),
    Tag(Arc<Tag>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub id: ObjectId,
    pub data: ObjectData,
}

/// A ref by its full name (`refs/heads/main`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Ref {
    pub name: String,
    pub target: ObjectId,
}

/// One ref update, as subscribers see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefChange {
    pub repo_id: i64,
    pub name: String,
    pub old: Option<ObjectId>,
    pub new: Option<ObjectId>,
    pub forced: bool,
}

/// A change to one path of a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Write {
        path: String,
        data: Arc<[u8]>,
        mode: EntryMode,
    },
    Delete {
        path: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileStatus {
    Added,
    Removed,
    Modified,
    Renamed,
}

impl FileStatus {
    /// The value REST answers in `files[].status`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FileStatus::Added => "added",
            FileStatus::Removed => "removed",
            FileStatus::Modified => "modified",
            FileStatus::Renamed => "renamed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub previous_path: Option<String>,
    pub status: FileStatus,
    /// The blob on the head side, or the base side for a removed file.
    pub blob: ObjectId,
    pub additions: u64,
    pub deletions: u64,
    /// The unified diff hunks; `None` for a binary file.
    pub patch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub merge_base: Option<ObjectId>,
    pub ahead_by: u64,
    pub behind_by: u64,
    /// The commits on head that base lacks, oldest first.
    pub commits: Vec<(ObjectId, Arc<Commit>)>,
    pub files: Vec<FileDiff>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeResult {
    pub commit: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitError {
    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
    #[error("not a 40-hex object ID")]
    BadObjectId,
    #[error("object or ref not found")]
    NotFound,
    #[error("the object is not the expected kind")]
    WrongKind,
    #[error("the ref already exists")]
    RefExists,
    #[error("the ref is not at the expected commit")]
    Stale,
    #[error("the update is not a fast-forward")]
    NotFastForward,
    #[error("the base already holds the head")]
    NothingToMerge,
    #[error("merge conflict")]
    Conflict { paths: Vec<String> },
    #[error("invalid: {0}")]
    Invalid(String),
}

impl From<GitError> for ApiError {
    fn from(e: GitError) -> ApiError {
        match e {
            GitError::NotImplemented(what) => ApiError::not_implemented(what),
            GitError::NotFound => ApiError::not_found(),
            GitError::Conflict { .. } => ApiError::conflict("Merge conflict"),
            GitError::RefExists => ApiError::unprocessable("Reference already exists"),
            GitError::NotFastForward => ApiError::unprocessable("Update is not a fast forward"),
            other => ApiError::unprocessable(other.to_string()),
        }
    }
}

/// The value behind GraphQL `Ref`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefNode {
    pub repo_id: i64,
    pub name: String,
    pub target: ObjectId,
}

/// The value behind GraphQL `Commit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitNode {
    pub repo_id: i64,
    pub id: ObjectId,
    pub commit: Arc<Commit>,
}

/// The value behind GraphQL `Tree`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    pub repo_id: i64,
    pub id: ObjectId,
    pub entries: Arc<[TreeEntry]>,
}

/// The value behind GraphQL `TreeEntry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntryNode {
    pub repo_id: i64,
    /// The path of the entry from the tree root.
    pub path: String,
    pub entry: TreeEntry,
}

/// The value behind GraphQL `Blob`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobNode {
    pub repo_id: i64,
    pub id: ObjectId,
    pub data: Arc<[u8]>,
}

/// The value behind GraphQL `Tag`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagNode {
    pub repo_id: i64,
    pub id: ObjectId,
    pub tag: Arc<Tag>,
}

/// GitHub's commit signature verification states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SignatureState {
    Valid,
    Unsigned,
    UnknownKey,
    Invalid,
}

impl SignatureState {
    /// The value REST answers in `verification.reason`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SignatureState::Valid => "valid",
            SignatureState::Unsigned => "unsigned",
            SignatureState::UnknownKey => "unknown_key",
            SignatureState::Invalid => "invalid",
        }
    }
}

/// The value behind GraphQL `GitSignature` (`GpgSignature` and the others).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureNode {
    pub repo_id: i64,
    pub commit: ObjectId,
    pub state: SignatureState,
    pub signature: String,
    pub payload: String,
    pub was_signed_by_github: bool,
}

pub type RefSubscriber = fn(&mut TxMut<'_>, &Git, &RefChange) -> Result<(), ApiError>;

/// Collects ref subscribers at start; [`GitBuilder::build`] freezes them.
pub struct GitBuilder {
    subscribers: Vec<RefSubscriber>,
}

impl GitBuilder {
    /// Registers git's tables (the refs) on the store builder.
    #[must_use]
    pub fn new(store: &mut StoreBuilder) -> GitBuilder {
        let _ = store;
        GitBuilder {
            subscribers: Vec::new(),
        }
    }

    /// Runs `f` inside the transaction of every ref update, in registration order.
    pub fn on_ref_update(&mut self, f: RefSubscriber) {
        self.subscribers.push(f);
    }

    #[must_use]
    pub fn build(self) -> Git {
        Git {
            inner: Arc::new(Inner {
                subscribers: self.subscribers,
            }),
        }
    }
}

struct Inner {
    subscribers: Vec<RefSubscriber>,
}

/// One per server: objects shared per fork network outside the store lock, refs per repository
/// in a store table. Cloning is cheap and shares the state.
#[derive(Clone)]
pub struct Git {
    inner: Arc<Inner>,
}

fn todo(what: &'static str) -> GitError {
    GitError::NotImplemented(what)
}

impl Git {
    /// The number of registered ref subscribers.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.inner.subscribers.len()
    }

    /// # Errors
    ///
    /// [`GitError::NotFound`] for a missing ref.
    pub fn resolve_ref(&self, tx: &Tx, repo: i64, name: &str) -> Result<ObjectId, GitError> {
        let _ = (tx, repo, name);
        Err(todo("git/resolve_ref"))
    }

    #[must_use]
    pub fn refs(&self, tx: &Tx, repo: i64, prefix: &str) -> Vec<Ref> {
        let _ = (tx, repo, prefix);
        Vec::new()
    }

    /// Every ref change goes through here, and runs the subscribers in the same transaction.
    ///
    /// # Errors
    ///
    /// [`GitError::Stale`] when `old` does not match, [`GitError::NotFastForward`] without `force`.
    pub fn update_ref(
        &self,
        tx: &mut TxMut<'_>,
        repo: i64,
        name: &str,
        old: Option<ObjectId>,
        new: Option<ObjectId>,
        force: bool,
    ) -> Result<(), GitError> {
        let _ = (tx, repo, name, old, new, force);
        Err(todo("git/update_ref"))
    }

    /// # Errors
    ///
    /// [`GitError::NotFound`] when the repository's fork network lacks the object.
    pub fn object(&self, tx: &Tx, repo: i64, id: ObjectId) -> Result<Object, GitError> {
        let _ = (tx, repo, id);
        Err(todo("git/object"))
    }

    /// Resolves `main:docs/a.md`, `HEAD:`, a ref or an object ID.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] when the expression names nothing.
    pub fn expression(&self, tx: &Tx, repo: i64, expr: &str) -> Result<Object, GitError> {
        let _ = (tx, repo, expr);
        Err(todo("git/expression"))
    }

    /// # Errors
    ///
    /// [`GitError`] when the write fails.
    pub fn write_blob(&self, tx: &Tx, repo: i64, data: &[u8]) -> Result<ObjectId, GitError> {
        let _ = (tx, repo, data);
        Err(todo("git/write_blob"))
    }

    /// # Errors
    ///
    /// [`GitError::NotFound`] when an entry names a missing object.
    pub fn write_tree(
        &self,
        tx: &Tx,
        repo: i64,
        entries: &[TreeEntry],
    ) -> Result<ObjectId, GitError> {
        let _ = (tx, repo, entries);
        Err(todo("git/write_tree"))
    }

    /// # Errors
    ///
    /// [`GitError::NotFound`] when the tree or a parent is missing.
    pub fn write_commit(&self, tx: &Tx, repo: i64, commit: &Commit) -> Result<ObjectId, GitError> {
        let _ = (tx, repo, commit);
        Err(todo("git/write_commit"))
    }

    /// # Errors
    ///
    /// [`GitError::NotFound`] when the target is missing.
    pub fn write_tag(&self, tx: &Tx, repo: i64, tag: &Tag) -> Result<ObjectId, GitError> {
        let _ = (tx, repo, tag);
        Err(todo("git/write_tag"))
    }

    /// Writes a commit of `changes` on `parent` and moves `branch` to it; `sign` signs it with the web-flow key.
    ///
    /// # Errors
    ///
    /// [`GitError::Stale`] when `branch` is not at `parent`.
    #[expect(
        clippy::too_many_arguments,
        reason = "the frozen contract of plan section 6.4"
    )]
    pub fn commit(
        &self,
        tx: &mut TxMut<'_>,
        repo: i64,
        branch: &str,
        parent: Option<ObjectId>,
        changes: &[FileChange],
        author: &Signature,
        committer: &Signature,
        sign: bool,
    ) -> Result<ObjectId, GitError> {
        let _ = (tx, repo, branch, parent, changes, author, committer, sign);
        Err(todo("git/commit"))
    }

    /// The merge base, ahead and behind counts, commits and files with patches.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] when either commit is missing.
    pub fn compare(
        &self,
        tx: &Tx,
        repo: i64,
        base: ObjectId,
        head: ObjectId,
    ) -> Result<Comparison, GitError> {
        let _ = (tx, repo, base, head);
        Err(todo("git/compare"))
    }

    /// Merges `head` into the branch `base` with a web-flow signed merge commit.
    ///
    /// # Errors
    ///
    /// [`GitError::NothingToMerge`] when base already holds head, [`GitError::Conflict`] on a conflict.
    pub fn merge(
        &self,
        tx: &mut TxMut<'_>,
        repo: i64,
        base: &str,
        head: &str,
        msg: &str,
    ) -> Result<MergeResult, GitError> {
        let _ = (tx, repo, base, head, msg);
        Err(todo("git/merge"))
    }

    /// # Errors
    ///
    /// [`GitError::NotFound`] when either commit is missing.
    pub fn mergeable(
        &self,
        tx: &Tx,
        repo: i64,
        base: ObjectId,
        head: ObjectId,
    ) -> Result<bool, GitError> {
        let _ = (tx, repo, base, head);
        Err(todo("git/mergeable"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_ids_round_trip_as_lowercase_hex() {
        let text = "0123456789abcdef0123456789ABCDEF01234567";
        let id: ObjectId = text.parse().unwrap();
        assert_eq!(id.to_string(), text.to_ascii_lowercase());
        assert!("0123".parse::<ObjectId>().is_err());
        assert!("g".repeat(40).parse::<ObjectId>().is_err());
    }
}
