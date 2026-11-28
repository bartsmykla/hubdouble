use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use hubdouble_model::{ApiError, Clock, Event, Repo, SystemClock, Timestamp};
use hubdouble_store::{Accounts, Repos, Store, StoreBuilder, Tx, TxMut};
use parking_lot::{Mutex, RwLock};
use serde::{Serialize, Serializer};

use crate::odb::Odb;
use crate::sign::WebFlow;
use crate::{diff, events, format, history, merge, refs, tree};

/// A SHA-1 git object ID.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; 20]);

impl ObjectId {
    /// The all-zero ID git writes for a missing side of a ref change.
    pub const NULL: ObjectId = ObjectId([0; 20]);

    fn hex(&self) -> [u8; 40] {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut hex = [0; 40];
        for (pair, b) in hex.as_chunks_mut::<2>().0.iter_mut().zip(self.0) {
            *pair = [DIGITS[usize::from(b >> 4)], DIGITS[usize::from(b & 0xf)]];
        }
        hex
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(std::str::from_utf8(&self.hex()).map_err(|_| fmt::Error)?)
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
        let hex = |c: u8| char::from(c).to_digit(16);
        for (o, [hi, lo]) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
            let byte = hex(*hi).zip(hex(*lo)).map(|(hi, lo)| hi * 16 + lo);
            match byte.and_then(|b| u8::try_from(b).ok()) {
                Some(b) => *o = b,
                None => return Err(GitError::BadObjectId),
            }
        }
        Ok(ObjectId(out))
    }
}

impl Serialize for ObjectId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let hex = self.hex();
        s.serialize_str(std::str::from_utf8(&hex).map_err(serde::ser::Error::custom)?)
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

impl Signature {
    /// The fixed identity and time of seeded commits and tags (plan 4.6).
    #[must_use]
    pub fn seed() -> Signature {
        Signature {
            name: "hubdouble".to_owned(),
            email: "seed@hubdouble.invalid".to_owned(),
            time: Timestamp::SEED,
            offset_minutes: 0,
        }
    }

    /// `GitHub <noreply@github.com>`, the committer of every commit GitHub writes itself.
    #[must_use]
    pub fn web_flow(time: Timestamp) -> Signature {
        Signature {
            name: "GitHub".to_owned(),
            email: "noreply@github.com".to_owned(),
            time,
            offset_minutes: 0,
        }
    }
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

impl ObjectData {
    #[must_use]
    pub fn kind(&self) -> ObjectKind {
        match self {
            ObjectData::Blob(_) => ObjectKind::Blob,
            ObjectData::Tree(_) => ObjectKind::Tree,
            ObjectData::Commit(_) => ObjectKind::Commit,
            ObjectData::Tag(_) => ObjectKind::Tag,
        }
    }
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
    /// The account ID of the actor, the `sender` and `pusher` of the `push` event.
    pub sender: i64,
    pub name: String,
    pub old: Option<ObjectId>,
    pub new: Option<ObjectId>,
    /// The update was not a fast-forward, as the `push` payload's `forced`.
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
    /// The unified diff hunks without a trailing newline, as GitHub's `patch`; `None` for a
    /// binary file and for a file whose content did not change (a pure rename or mode change).
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
    /// A ref subscriber refused the update; the transaction rolls back with its error.
    #[error("a ref subscriber refused the update: {}", .0.message)]
    Rejected(ApiError),
    #[error("deflate failed: {0}")]
    Compress(String),
    #[error("binary delta failed: {0}")]
    Delta(String),
}

impl From<GitError> for ApiError {
    fn from(e: GitError) -> ApiError {
        match e {
            GitError::NotImplemented(what) => ApiError::not_implemented(what),
            GitError::NotFound => ApiError::not_found(),
            GitError::Conflict { .. } => ApiError::conflict("Merge conflict"),
            GitError::RefExists => ApiError::unprocessable("Reference already exists"),
            GitError::NotFastForward => ApiError::unprocessable("Update is not a fast forward"),
            GitError::Rejected(e) => e,
            GitError::Compress(e) | GitError::Delta(e) => ApiError::internal(e),
            other => ApiError::unprocessable(other.to_string()),
        }
    }
}

/// The value behind GraphQL `Ref`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefNode {
    pub repo_id: i64,
    /// The full name, such as `refs/tags/agent/v5.0.0`.
    pub name: String,
    /// The part of `name` that GraphQL `Ref.name` leaves out: the `refPrefix` a
    /// `refs` listing asked for, else `refs/heads/`, `refs/tags/` or `refs/`.
    pub prefix: String,
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

/// Work a read asks for on one subject ID and the generation of the subject that the read saw,
/// as a GitHub read starts a background job. The store lock is released between the read and
/// the job, so the job checks that the generation is still the subject's.
pub type ReadJob = fn(&mut TxMut<'_>, &Git, i64, u64) -> Result<(), ApiError>;

/// The jobs that the reads of one request ask for. The request runs them with
/// [`ReadJobs::run`] after its reads and before it answers. A request that fails drops them
/// without a run.
#[derive(Default)]
pub struct ReadJobs {
    jobs: Mutex<Vec<(ReadJob, i64, u64)>>,
}

impl ReadJobs {
    /// Asks for `job` on `id` as it was at `generation`. A run takes a job once per subject and
    /// generation, however many reads of the request asked for it.
    pub fn ask(&self, job: ReadJob, id: i64, generation: u64) {
        self.jobs.lock().push((job, id, generation));
    }

    /// Runs the asked jobs in one write of `store`, and takes no lock when there are none.
    ///
    /// # Errors
    ///
    /// The first error of a job; the write then rolls back.
    pub fn run(self, git: &Git, store: &Store) -> Result<(), ApiError> {
        let mut jobs = self.jobs.into_inner();
        if jobs.is_empty() {
            return Ok(());
        }
        jobs.sort_by_key(|&(_, id, generation)| (id, generation));
        jobs.dedup_by(|a, b| a.1 == b.1 && a.2 == b.2 && std::ptr::fn_addr_eq(a.0, b.0));
        store.update(|tx| {
            for (job, id, generation) in &jobs {
                job(tx, git, *id, *generation)?;
            }
            Ok(())
        })
    }
}

/// Collects ref subscribers at start; [`GitBuilder::build`] freezes them.
pub struct GitBuilder {
    subscribers: Vec<RefSubscriber>,
    clock: Arc<dyn Clock>,
}

impl GitBuilder {
    /// Registers git's tables (the refs) on the store builder.
    #[must_use]
    pub fn new(store: &mut StoreBuilder) -> GitBuilder {
        store.register_table::<refs::Refs>();
        GitBuilder {
            subscribers: Vec::new(),
            clock: Arc::new(SystemClock),
        }
    }

    /// Runs `f` inside the transaction of every ref update, in registration order.
    pub fn on_ref_update(&mut self, f: RefSubscriber) {
        self.subscribers.push(f);
    }

    /// The clock of merge commits; the system clock by default.
    pub fn clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
    }

    #[must_use]
    pub fn build(self) -> Git {
        Git {
            inner: Arc::new(Inner {
                subscribers: self.subscribers,
                networks: RwLock::new(HashMap::new()),
                web_flow: WebFlow::new(),
                clock: self.clock,
            }),
        }
    }
}

pub(crate) struct Inner {
    subscribers: Vec<RefSubscriber>,
    networks: RwLock<HashMap<i64, Arc<Odb>>>,
    web_flow: Result<WebFlow, GitError>,
    clock: Arc<dyn Clock>,
}

/// One per server: objects shared per fork network outside the store lock, refs per repository
/// in a store table. Cloning is cheap and shares the state.
#[derive(Clone)]
pub struct Git {
    pub(crate) inner: Arc<Inner>,
}

impl Git {
    /// The time of the server clock, for ref subscribers that stamp what a ref move changes.
    #[must_use]
    pub fn now(&self) -> Timestamp {
        self.inner.clock.now()
    }

    /// The number of registered ref subscribers.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.inner.subscribers.len()
    }

    pub(crate) fn odb(&self, tx: &Tx, repo: i64) -> Arc<Odb> {
        let root = refs::network_root(tx, repo);
        if let Some(odb) = self.inner.networks.read().get(&root) {
            return odb.clone();
        }
        self.inner.networks.write().entry(root).or_default().clone()
    }

    /// The ref's own target, not peeled. Takes a full name (`refs/heads/main`), a short one
    /// (`heads/main`, `main`, `v1.0`, tags before branches as git does) or `HEAD` for the
    /// default branch.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] for a missing ref.
    pub fn resolve_ref(&self, tx: &Tx, repo: i64, name: &str) -> Result<ObjectId, GitError> {
        match refs::resolve(tx, repo, name) {
            Some(id) => Ok(id),
            None => Err(GitError::NotFound),
        }
    }

    /// The refs whose full name starts with `prefix`, sorted by name; a prefix without `refs/`
    /// gets it added, and an empty prefix lists every ref.
    #[must_use]
    pub fn refs(&self, tx: &Tx, repo: i64, prefix: &str) -> Vec<Ref> {
        refs::list(tx, repo, prefix)
    }

    /// Every ref change goes through here, and runs the subscribers in the same transaction.
    /// `sender` is the account ID of the actor. `old` is the value the caller expects, `None`
    /// meaning the ref must not exist; `new` `None` deletes the ref. Setting a ref to its
    /// current value changes nothing. Each change of a branch or a tag, a delete too, sets the
    /// repository's `pushed_at` to now and leaves its `updated_at`, as a push on GitHub does.
    ///
    /// # Errors
    ///
    /// [`GitError::Stale`] when `old` does not match, [`GitError::NotFastForward`] without `force`.
    #[expect(
        clippy::too_many_arguments,
        reason = "the frozen git contract plus the sender"
    )]
    pub fn update_ref(
        &self,
        tx: &mut TxMut<'_>,
        repo: i64,
        sender: i64,
        name: &str,
        old: Option<ObjectId>,
        new: Option<ObjectId>,
        force: bool,
    ) -> Result<(), GitError> {
        if !refs::valid_name(name) {
            return Err(GitError::Invalid(format!("{name} is not a valid ref name")));
        }
        let current = refs::get(tx, repo, name);
        match old {
            None if current.is_some() => return Err(GitError::RefExists),
            Some(o) if current != Some(o) => return Err(GitError::Stale),
            _ => {}
        }
        if current == new {
            return match new {
                Some(_) => Ok(()),
                None => Err(GitError::NotFound),
            };
        }
        let odb = self.odb(tx, repo);
        if let Some(n) = new {
            let data = odb.get(n)?;
            if name.starts_with("refs/heads/") && !matches!(data, ObjectData::Commit(_)) {
                return Err(GitError::WrongKind);
            }
        }
        let forced = match (current, new) {
            (Some(o), Some(n)) => !history::fast_forward(&odb, o, n)?,
            _ => false,
        };
        if forced && !force {
            return Err(GitError::NotFastForward);
        }
        refs::set(tx, repo, name, new);
        let now = Some(self.now());
        if (name.starts_with("refs/heads/") || name.starts_with("refs/tags/"))
            && let Some(r) = tx.table::<Repos>().get(repo)
            && r.pushed_at != now
        {
            let mut r = Repo::clone(r);
            r.pushed_at = now;
            tx.table_mut::<Repos>()
                .replace(r)
                .map_err(|e| GitError::Rejected(e.into()))?;
        }
        let change = RefChange {
            repo_id: repo,
            sender,
            name: name.to_owned(),
            old: current,
            new,
            forced,
        };
        for event in events::for_change(tx, &odb, &change)? {
            hubdouble_hooks::emit(tx, event);
        }
        for subscriber in &self.inner.subscribers {
            subscriber(tx, self, &change).map_err(GitError::Rejected)?;
        }
        Ok(())
    }

    /// The `push` event that `change` delivers, without moving any ref; `None` for a ref that
    /// is neither a branch nor a tag.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] when the repository's fork network lacks a commit of the change.
    pub fn push_event(&self, tx: &Tx, change: &RefChange) -> Result<Option<Event>, GitError> {
        events::push(tx, &self.odb(tx, change.repo_id), change)
    }

    /// # Errors
    ///
    /// [`GitError::NotFound`] when the repository's fork network lacks the object.
    pub fn object(&self, tx: &Tx, repo: i64, id: ObjectId) -> Result<Object, GitError> {
        let data = self.odb(tx, repo).get(id)?;
        Ok(Object { id, data })
    }

    /// Resolves `main:docs/a.md`, `HEAD:`, a ref or an object ID. A ref or ID alone answers the
    /// object itself (an annotated tag stays a tag); with `:` it reads the path from that tree,
    /// or from the tree of the commit it peels to.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] when the expression names nothing, [`GitError::WrongKind`] when the
    /// part before `:` is neither a tree nor peels to a commit.
    pub fn expression(&self, tx: &Tx, repo: i64, expr: &str) -> Result<Object, GitError> {
        let odb = self.odb(tx, repo);
        let Some((rev, path)) = expr.split_once(':') else {
            let id = self.rev(tx, repo, &odb, expr)?;
            return Ok(Object {
                id,
                data: odb.get(id)?,
            });
        };
        let base = self.rev(tx, repo, &odb, rev)?;
        let root = match odb.get(base)? {
            ObjectData::Tree(_) => base,
            _ => odb.peel_commit(base)?.1.tree,
        };
        let id = tree::lookup(&odb, root, path)?;
        Ok(Object {
            id,
            data: odb.get(id)?,
        })
    }

    fn rev(&self, tx: &Tx, repo: i64, odb: &Odb, rev: &str) -> Result<ObjectId, GitError> {
        if let Ok(id) = rev.parse::<ObjectId>()
            && odb.contains(id)
        {
            return Ok(id);
        }
        self.resolve_ref(tx, repo, rev)
    }

    /// Peels tags until a commit and answers its ID.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] for a missing object, [`GitError::WrongKind`] when it does not peel to a commit.
    pub fn peel_to_commit(&self, tx: &Tx, repo: i64, id: ObjectId) -> Result<ObjectId, GitError> {
        Ok(self.odb(tx, repo).peel_commit(id)?.0)
    }

    /// Whether commit `ancestor` is in the history of commit `of`, itself included.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] for a missing commit on the walk.
    pub fn is_ancestor(
        &self,
        tx: &Tx,
        repo: i64,
        ancestor: ObjectId,
        of: ObjectId,
    ) -> Result<bool, GitError> {
        history::is_ancestor(&self.odb(tx, repo), ancestor, of)
    }

    /// The history from `head`, newest first: by committer time like `git log`, or along first
    /// parents only; at most `limit` commits.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] when a commit is missing.
    pub fn log(
        &self,
        tx: &Tx,
        repo: i64,
        head: ObjectId,
        first_parent: bool,
        limit: usize,
    ) -> Result<Vec<(ObjectId, Arc<Commit>)>, GitError> {
        let odb = self.odb(tx, repo);
        let (head, _) = odb.peel_commit(head)?;
        history::log(&odb, head, first_parent, limit)
    }

    /// # Errors
    ///
    /// [`GitError`] when the write fails.
    pub fn write_blob(&self, tx: &Tx, repo: i64, data: &[u8]) -> Result<ObjectId, GitError> {
        self.odb(tx, repo).put(ObjectData::Blob(data.into()))
    }

    /// Sorts `entries` in git's order before it writes them.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] when an entry names a missing object.
    pub fn write_tree(
        &self,
        tx: &Tx,
        repo: i64,
        entries: &[TreeEntry],
    ) -> Result<ObjectId, GitError> {
        let odb = self.odb(tx, repo);
        tree::write_entries(&odb, entries.to_vec())
    }

    /// # Errors
    ///
    /// [`GitError::NotFound`] when the tree or a parent is missing.
    pub fn write_commit(&self, tx: &Tx, repo: i64, commit: &Commit) -> Result<ObjectId, GitError> {
        let odb = self.odb(tx, repo);
        odb.tree(commit.tree)?;
        for parent in &commit.parents {
            odb.commit(*parent)?;
        }
        odb.put(ObjectData::Commit(Arc::new(commit.clone())))
    }

    /// # Errors
    ///
    /// [`GitError::NotFound`] when the target is missing.
    pub fn write_tag(&self, tx: &Tx, repo: i64, tag: &Tag) -> Result<ObjectId, GitError> {
        let odb = self.odb(tx, repo);
        if odb.get(tag.target)?.kind() != tag.target_kind {
            return Err(GitError::WrongKind);
        }
        odb.put(ObjectData::Tag(Arc::new(tag.clone())))
    }

    /// Writes a commit of `changes` on `parent` and moves `branch` (a branch name or a full ref
    /// name) to it as `sender`; `sign` signs it with the web-flow key.
    ///
    /// # Errors
    ///
    /// [`GitError::Stale`] when `branch` is not at `parent`, [`GitError::Invalid`] for a bad path.
    #[expect(clippy::too_many_arguments, reason = "the frozen git contract")]
    pub fn commit(
        &self,
        tx: &mut TxMut<'_>,
        repo: i64,
        sender: i64,
        branch: &str,
        parent: Option<ObjectId>,
        changes: &[FileChange],
        message: &str,
        author: &Signature,
        committer: &Signature,
        sign: bool,
    ) -> Result<ObjectId, GitError> {
        let name = refs::branch_ref(branch);
        if refs::get(tx, repo, &name) != parent {
            return Err(GitError::Stale);
        }
        let odb = self.odb(tx, repo);
        let base = match parent {
            Some(p) => Some(odb.commit(p)?.tree),
            None => None,
        };
        let mut commit = Commit {
            tree: tree::apply(&odb, base, changes)?,
            parents: parent.into_iter().collect(),
            author: author.clone(),
            committer: committer.clone(),
            message: message.to_owned(),
            gpg_signature: None,
        };
        if sign {
            self.sign(&mut commit)?;
        }
        let id = odb.put(ObjectData::Commit(Arc::new(commit)))?;
        self.update_ref(tx, repo, sender, &name, parent, Some(id), false)?;
        Ok(id)
    }

    /// Replaces `commit`'s signature with one from the web-flow key, as GitHub signs a commit
    /// it writes for a bot.
    ///
    /// # Errors
    ///
    /// The key's load or signing error.
    pub fn sign(&self, commit: &mut Commit) -> Result<(), GitError> {
        let key = self.inner.web_flow.as_ref().map_err(Clone::clone)?;
        commit.gpg_signature = None;
        let payload = crate::encode::commit_bytes(commit)?;
        commit.gpg_signature = Some(key.sign(&payload, commit.committer.time)?);
        Ok(())
    }

    /// GitHub's verification state of `commit`'s signature against the web-flow key.
    #[must_use]
    pub fn verification(&self, commit: &Commit) -> SignatureState {
        let Some(signature) = &commit.gpg_signature else {
            return SignatureState::Unsigned;
        };
        let Ok(key) = &self.inner.web_flow else {
            return SignatureState::UnknownKey;
        };
        let unsigned = Commit {
            gpg_signature: None,
            ..commit.clone()
        };
        match crate::encode::commit_bytes(&unsigned) {
            Ok(payload) => key.verify(signature, &payload),
            Err(_) => SignatureState::Invalid,
        }
    }

    /// The GraphQL `GitSignature` of a signed commit; `None` for an unsigned one.
    #[must_use]
    pub fn signature(&self, repo: i64, id: ObjectId, commit: &Commit) -> Option<SignatureNode> {
        let signature = commit.gpg_signature.clone()?;
        let state = self.verification(commit);
        let unsigned = Commit {
            gpg_signature: None,
            ..commit.clone()
        };
        let payload = crate::encode::commit_bytes(&unsigned).ok()?;
        Some(SignatureNode {
            repo_id: repo,
            commit: id,
            state,
            signature,
            payload: String::from_utf8_lossy(&payload).into_owned(),
            was_signed_by_github: state == SignatureState::Valid,
        })
    }

    /// The armored public web-flow key, as `GET /users/web-flow/gpg_keys` lists it.
    ///
    /// # Errors
    ///
    /// [`GitError::Invalid`] when the key could not be made.
    pub fn web_flow_public_key(&self) -> Result<String, GitError> {
        self.inner.web_flow.as_ref().map_err(Clone::clone)?.public()
    }

    /// The merge base, ahead and behind counts, commits and files with patches. Tags peel to
    /// their commits. Files compare the merge base with head, as GitHub's three-dot compare;
    /// without a merge base they compare base with head.
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
        let odb = self.odb(tx, repo);
        let (base, base_commit) = odb.peel_commit(base)?;
        let (head, head_commit) = odb.peel_commit(head)?;
        let behind_set = history::ancestors(&odb, base)?;
        let ahead_set = history::ancestors(&odb, head)?;
        let merge_base = history::best_common(&odb, &behind_set, &ahead_set)?;
        let from = match merge_base {
            Some(m) => odb.commit(m)?.tree,
            None => base_commit.tree,
        };
        let changes = diff::tree_changes(&odb, Some(from), Some(head_commit.tree))?;
        Ok(Comparison {
            merge_base,
            ahead_by: count(ahead_set.difference(&behind_set).count()),
            behind_by: count(behind_set.difference(&ahead_set).count()),
            commits: history::compare_order(&odb, head, &behind_set)?,
            files: diff::file_diffs(&odb, &changes)?,
        })
    }

    /// The changes from the commit `base` (the empty tree when `None`) to the commit `head` as
    /// GitHub answers `application/vnd.github.diff`: `git diff` text in path order, with
    /// exact renames, `Binary files ... differ`, and git's automatic abbreviation for the
    /// repository network's object count.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] when either commit is missing.
    pub fn diff(
        &self,
        tx: &Tx,
        repo: i64,
        base: Option<ObjectId>,
        head: ObjectId,
    ) -> Result<String, GitError> {
        let odb = self.odb(tx, repo);
        let files = Self::files(&odb, base, head)?;
        format::diff_text(&files, format::abbrev(odb.len()), false)
    }

    /// Each commit, in the order given, as a `git format-patch` mail against its first
    /// parent, as GitHub answers `application/vnd.github.patch`; two or more commits are
    /// numbered `[PATCH n/m]`.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] when a commit is missing, and [`GitError::Compress`] when zlib
    /// fails on a binary side, so no patch carries a broken stream.
    pub fn patch(&self, tx: &Tx, repo: i64, commits: &[ObjectId]) -> Result<String, GitError> {
        let odb = self.odb(tx, repo);
        let abbrev = format::abbrev(odb.len());
        let mut mails = Vec::with_capacity(commits.len());
        for (i, &id) in commits.iter().enumerate() {
            let (id, commit) = odb.peel_commit(id)?;
            let files = Self::files(&odb, commit.parents.first().copied(), id)?;
            let number = (commits.len() > 1).then_some((i + 1, commits.len()));
            mails.push(format::mail(id, &commit, number, &files, abbrev)?);
        }
        Ok(mails.join("\n"))
    }

    fn files(
        odb: &Odb,
        base: Option<ObjectId>,
        head: ObjectId,
    ) -> Result<Vec<format::File>, GitError> {
        let from = base.map(|b| odb.peel_commit(b)).transpose()?;
        let (_, to) = odb.peel_commit(head)?;
        let changes = diff::tree_changes(odb, from.map(|(_, c)| c.tree), Some(to.tree))?;
        format::files(odb, &changes)
    }

    /// Merges `head` (a branch, ref or commit ID) into the branch `base` with a web-flow signed
    /// merge commit whose first parent is base, then moves base through [`Git::update_ref`].
    /// The author is the `sender` account and the committer is GitHub, at the clock's time.
    ///
    /// # Errors
    ///
    /// [`GitError::NothingToMerge`] when base already holds head, [`GitError::Conflict`] on a
    /// conflict, [`GitError::NotFound`] when the sender account is missing.
    pub fn merge(
        &self,
        tx: &mut TxMut<'_>,
        repo: i64,
        sender: i64,
        base: &str,
        head: &str,
        msg: &str,
    ) -> Result<MergeResult, GitError> {
        let name = refs::branch_ref(base);
        let base_ref = refs::get(tx, repo, &name).ok_or(GitError::NotFound)?;
        let odb = self.odb(tx, repo);
        let (base_id, base_commit) = odb.peel_commit(base_ref)?;
        let (head_id, head_commit) = odb.peel_commit(self.rev(tx, repo, &odb, head)?)?;
        if history::is_ancestor(&odb, head_id, base_id)? {
            return Err(GitError::NothingToMerge);
        }
        let id = self.merge_commit(
            tx,
            &odb,
            sender,
            (base_id, &base_commit),
            (head_id, &head_commit),
            msg,
        )?;
        self.update_ref(tx, repo, sender, &name, Some(base_ref), Some(id), false)?;
        Ok(MergeResult { commit: id })
    }

    /// Writes the test merge commit that GitHub keeps for an open pull request at
    /// `refs/pull/{n}/merge`: `head` merged into `base` with the message
    /// `Merge {head} into {base}`, the `author` account as author and a web-flow signature, at
    /// the clock's time. It moves no ref. `None` when the merge has a conflict.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] when either commit or the author account is missing.
    pub fn test_merge(
        &self,
        tx: &Tx,
        repo: i64,
        author: i64,
        base: ObjectId,
        head: ObjectId,
    ) -> Result<Option<ObjectId>, GitError> {
        let odb = self.odb(tx, repo);
        let (base, base_commit) = odb.peel_commit(base)?;
        let (head, head_commit) = odb.peel_commit(head)?;
        let msg = format!("Merge {head} into {base}");
        match self.merge_commit(
            tx,
            &odb,
            author,
            (base, &base_commit),
            (head, &head_commit),
            &msg,
        ) {
            Ok(id) => Ok(Some(id)),
            Err(GitError::Conflict { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn merge_commit(
        &self,
        tx: &Tx,
        odb: &Odb,
        author: i64,
        (base_id, base_commit): (ObjectId, &Commit),
        (head_id, head_commit): (ObjectId, &Commit),
        msg: &str,
    ) -> Result<ObjectId, GitError> {
        let origin = match history::merge_base(odb, base_id, head_id)? {
            Some(m) => Some(odb.commit(m)?.tree),
            None => None,
        };
        let tree = merge::trees(
            odb,
            origin,
            Some(base_commit.tree),
            Some(head_commit.tree),
            true,
        )?;
        let tree = match tree {
            Some(t) => t,
            None => tree::empty(odb)?,
        };
        let now = self.inner.clock.now();
        let account = tx
            .table::<Accounts>()
            .get(author)
            .ok_or(GitError::NotFound)?;
        let author = Signature {
            name: account
                .name
                .clone()
                .unwrap_or_else(|| account.login.clone()),
            email: account.email.clone().unwrap_or_else(|| {
                format!("{}+{}@users.noreply.github.com", account.id, account.login)
            }),
            time: now,
            offset_minutes: 0,
        };
        let mut commit = Commit {
            tree,
            parents: vec![base_id, head_id],
            author,
            committer: Signature::web_flow(now),
            message: msg.to_owned(),
            gpg_signature: None,
        };
        self.sign(&mut commit)?;
        odb.put(ObjectData::Commit(Arc::new(commit)))
    }

    /// Whether merging `head` into `base` has no conflict; `true` when base already holds head.
    ///
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
        let odb = self.odb(tx, repo);
        let (base, base_commit) = odb.peel_commit(base)?;
        let (head, head_commit) = odb.peel_commit(head)?;
        if history::is_ancestor(&odb, head, base)? {
            return Ok(true);
        }
        let origin = match history::merge_base(&odb, base, head)? {
            Some(m) => Some(odb.commit(m)?.tree),
            None => None,
        };
        match merge::trees(
            &odb,
            origin,
            Some(base_commit.tree),
            Some(head_commit.tree),
            false,
        ) {
            Ok(_) => Ok(true),
            Err(GitError::Conflict { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
