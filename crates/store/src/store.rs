use std::any::{Any, TypeId};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use hubdouble_model::{ApiError, Config};
use parking_lot::{Mutex, RwLock};
use tokio::sync::Notify;

use crate::counters::Counters;
use crate::deletion::{Deleted, DeletionSubscriber};
use crate::outbox::{Committed, OutboxEntry, OutboxUse};

/// A table the store holds. Each registered type is one table; the store keeps them in a
/// `TypeId` map with typed accessors, so the trait need not be object safe.
pub trait Table: Any + Send + Sync + Clone {
    /// Called by [`Store::restore`] with the table as it was in the snapshot.
    fn restore(&mut self, seeded: &Self) {
        *self = seeded.clone();
    }
}

pub(crate) trait AnyTable: Any + Send + Sync {
    fn clone_box(&self) -> Box<dyn AnyTable>;
    fn restore_dyn(&mut self, seeded: &dyn AnyTable);
}

impl<T: Table> AnyTable for T {
    fn clone_box(&self) -> Box<dyn AnyTable> {
        Box::new(self.clone())
    }

    fn restore_dyn(&mut self, seeded: &dyn AnyTable) {
        if let Some(seeded) = (seeded as &dyn Any).downcast_ref::<T>() {
            self.restore(seeded);
        }
    }
}

type Undo = Box<dyn FnOnce(&mut Tx) + Send>;

/// The state inside the lock: every registered table, and the configuration the store was
/// built with, so code inside a transaction renders absolute URLs without a request.
pub struct Tx {
    tables: HashMap<TypeId, Box<dyn AnyTable>>,
    config: Arc<Config>,
    deletions: Arc<[DeletionSubscriber]>,
}

impl Tx {
    pub(crate) fn deletion_subscribers(&self) -> Arc<[DeletionSubscriber]> {
        Arc::clone(&self.deletions)
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// # Panics
    ///
    /// Panics when `T` was never registered on the [`StoreBuilder`].
    #[must_use]
    pub fn table<T: Table>(&self) -> &T {
        self.try_table::<T>()
            .unwrap_or_else(|| panic!("table {} is not registered", std::any::type_name::<T>()))
    }

    #[must_use]
    pub fn try_table<T: Table>(&self) -> Option<&T> {
        self.tables
            .get(&TypeId::of::<T>())
            .and_then(|t| (&**t as &dyn Any).downcast_ref::<T>())
    }

    pub(crate) fn table_raw_mut<T: Table>(&mut self) -> &mut T {
        self.tables
            .get_mut(&TypeId::of::<T>())
            .and_then(|t| (&mut **t as &mut dyn Any).downcast_mut::<T>())
            .unwrap_or_else(|| panic!("table {} is not registered", std::any::type_name::<T>()))
    }
}

#[derive(Default)]
struct Work {
    undo: Vec<Undo>,
    saved: Vec<TypeId>,
    outbox: Vec<OutboxEntry>,
}

/// A write transaction. Reads go through `Deref<Target = Tx>`; writes only through
/// [`TxMut::table_mut`], so every change records its undo.
pub struct TxMut<'a> {
    tx: &'a mut Tx,
    work: &'a mut Work,
}

impl Deref for TxMut<'_> {
    type Target = Tx;

    fn deref(&self) -> &Tx {
        self.tx
    }
}

impl TxMut<'_> {
    /// The next value of the global `int64` ID sequence.
    pub fn next_id(&mut self) -> i64 {
        self.table_mut::<Counters>().next(crate::Sequence::Id)
    }

    pub fn table_mut<T: Table>(&mut self) -> TableMut<'_, T> {
        TableMut {
            table: self.tx.table_raw_mut::<T>(),
            undo: &mut self.work.undo,
            saved: &mut self.work.saved,
        }
    }

    /// Buffers a webhook delivery; it reaches the outbox only when the transaction commits.
    pub fn push_outbox(&mut self, entry: OutboxEntry) {
        self.work.outbox.push(entry);
    }

    /// Deletes a repository, a team or an account with every row that names it, in this
    /// transaction. An account's repositories and an organization's teams are deleted first,
    /// each as a deletion of its own. Then every [`DeletionSubscriber`] runs, in registration
    /// order, and last the store drops its own rows: the entity, the rows in a deleted
    /// repository, and the references other rows hold, such as a team's repository list or the
    /// head repository of a pull request from a deleted fork, which becomes `None`.
    ///
    /// # Errors
    ///
    /// What a subscriber returns.
    pub fn delete(&mut self, deleted: &Deleted) -> Result<(), ApiError> {
        crate::deletion::delete(self, deleted)
    }

    /// A shorter-lived handle on the same transaction, for a context that holds a `TxMut` by value.
    pub fn reborrow(&mut self) -> TxMut<'_> {
        TxMut {
            tx: self.tx,
            work: self.work,
        }
    }
}

/// Write access to one table inside a [`TxMut`]. Reads go through `Deref<Target = T>`.
pub struct TableMut<'a, T> {
    pub(crate) table: &'a mut T,
    undo: &'a mut Vec<Undo>,
    saved: &'a mut Vec<TypeId>,
}

impl<T> Deref for TableMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.table
    }
}

impl<T: Table> TableMut<'_, T> {
    /// Unrestricted access. The first call in a transaction saves a copy of the whole table for
    /// rollback, so a table with typed writers ([`crate::Rows`], [`Counters`]) should use those.
    pub fn get_mut(&mut self) -> &mut T {
        let id = TypeId::of::<T>();
        if !self.saved.contains(&id) {
            self.saved.push(id);
            let copy = self.table.clone();
            self.undo
                .push(Box::new(move |tx: &mut Tx| *tx.table_raw_mut::<T>() = copy));
        }
        self.table
    }

    pub(crate) fn record(&mut self, undo: impl FnOnce(&mut T) + Send + 'static) {
        self.undo
            .push(Box::new(move |tx: &mut Tx| undo(tx.table_raw_mut::<T>())));
    }
}

struct Rollback<'a> {
    tx: &'a mut Tx,
    work: Work,
    armed: bool,
}

impl Drop for Rollback<'_> {
    fn drop(&mut self) {
        if self.armed {
            while let Some(undo) = self.work.undo.pop() {
                undo(self.tx);
            }
        }
    }
}

struct State {
    tx: Tx,
    next_seq: u64,
}

#[derive(Default)]
struct Outbox {
    entries: BTreeMap<u64, Committed>,
    refused: HashMap<i64, VecDeque<u64>>,
    refused_bytes: usize,
    shed_from: u64,
}

impl Outbox {
    fn push(&mut self, c: Committed) {
        self.entries.insert(c.seq, c);
    }

    fn refuse(&mut self, c: Committed, limit: OutboxUse) {
        let (seq, app) = (c.seq, c.entry.app_id);
        self.refused_bytes += c.entry.payload.len();
        self.push(c);
        let held = self.refused.entry(app).or_default();
        held.push_back(seq);
        if held.len() > limit.refused_per_app
            && let Some(old) = held.pop_front()
            && let Some(old) = self.entries.remove(&old)
        {
            self.refused_bytes -= old.entry.payload.len();
        }
        while self.refused_bytes > limit.refused_bytes {
            let Some((&seq, old)) = self.entries.range_mut(self.shed_from..).next() else {
                break;
            };
            self.shed_from = seq + 1;
            if old.over_budget {
                self.refused_bytes -= old.entry.payload.len();
                old.entry.payload = Bytes::new();
            }
        }
    }

    fn refused_per_app(&self) -> usize {
        self.refused.values().map(VecDeque::len).max().unwrap_or(0)
    }

    fn take(&mut self) -> Vec<Committed> {
        self.shed_from = 0;
        self.refused.clear();
        std::mem::take(&mut self.entries).into_values().collect()
    }

    fn release_refused(&mut self, bytes: usize) {
        self.refused_bytes = self.refused_bytes.saturating_sub(bytes);
    }
}

struct Inner {
    state: RwLock<State>,
    outbox: Mutex<Outbox>,
    outbox_ready: Notify,
    epoch: AtomicU64,
    limit: Mutex<OutboxUse>,
    used: Mutex<OutboxUse>,
}

impl Inner {
    fn admit(&self, bytes: usize) -> bool {
        let limit = *self.limit.lock();
        let mut used = self.used.lock();
        let fits = used.events < limit.events
            && used
                .bytes
                .checked_add(bytes)
                .is_some_and(|b| b <= limit.bytes);
        if fits {
            used.events += 1;
            used.bytes += bytes;
        }
        fits
    }

    fn release(&self, bytes: usize) {
        let mut used = self.used.lock();
        used.events = used.events.saturating_sub(1);
        used.bytes = used.bytes.saturating_sub(bytes);
    }
}

/// The whole state behind one lock. Cloning is cheap and shares the state.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

/// A copy of every table, taken with [`Store::snapshot`].
pub struct Snapshot {
    tables: HashMap<TypeId, Box<dyn AnyTable>>,
}

impl Store {
    /// Runs `f` under the shared lock.
    ///
    /// # Errors
    ///
    /// Returns what `f` returns.
    pub fn view<R>(&self, f: impl FnOnce(&Tx) -> Result<R, ApiError>) -> Result<R, ApiError> {
        f(&self.inner.state.read().tx)
    }

    /// Runs `f` under the exclusive lock and commits when it returns `Ok`. An `Err` or a panic
    /// replays the undo log in reverse, so nothing changes and no outbox entry is kept. Outbox
    /// entries get their commit sequence numbers under the lock.
    ///
    /// # Errors
    ///
    /// Returns what `f` returns.
    pub fn update<R>(
        &self,
        f: impl FnOnce(&mut TxMut<'_>) -> Result<R, ApiError>,
    ) -> Result<R, ApiError> {
        self.write(f, true)
    }

    /// As [`Store::update`], but the outbox buffer is dropped on commit: the seed load and reset only.
    ///
    /// # Errors
    ///
    /// Returns what `f` returns.
    pub fn update_quiet<R>(
        &self,
        f: impl FnOnce(&mut TxMut<'_>) -> Result<R, ApiError>,
    ) -> Result<R, ApiError> {
        self.write(f, false)
    }

    fn write<R>(
        &self,
        f: impl FnOnce(&mut TxMut<'_>) -> Result<R, ApiError>,
        deliver: bool,
    ) -> Result<R, ApiError> {
        let mut state = self.inner.state.write();
        let State { tx, next_seq } = &mut *state;
        let mut guard = Rollback {
            tx,
            work: Work::default(),
            armed: true,
        };
        let result = f(&mut TxMut {
            tx: &mut *guard.tx,
            work: &mut guard.work,
        })?;
        guard.armed = false;
        let entries = std::mem::take(&mut guard.work.outbox);
        drop(guard);
        let delivered = deliver && !entries.is_empty();
        if delivered {
            let epoch = self.inner.epoch.load(Ordering::SeqCst);
            let limit = *self.inner.limit.lock();
            let mut outbox = self.inner.outbox.lock();
            for entry in entries {
                *next_seq += 1;
                let c = Committed {
                    seq: *next_seq,
                    epoch,
                    over_budget: !self.inner.admit(entry.payload.len()),
                    entry,
                };
                if c.over_budget {
                    outbox.refuse(c, limit);
                } else {
                    outbox.push(c);
                }
            }
        }
        drop(state);
        if delivered {
            self.inner.outbox_ready.notify_one();
        }
        Ok(result)
    }

    /// Clones every registered table under the lock; entities are `Arc`s, so the copy is shallow.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let state = self.inner.state.read();
        Snapshot {
            tables: state
                .tx
                .tables
                .iter()
                .map(|(id, t)| (*id, t.clone_box()))
                .collect(),
        }
    }

    /// Resets to `s` under the exclusive lock: each table's [`Table::restore`] decides what comes
    /// back. It also drops every committed outbox entry the publisher has not taken and moves to
    /// a new epoch, so hooks can refuse to start an attempt committed before the reset. The
    /// dropped entries release their share of the outbox limits.
    pub fn restore(&self, s: &Snapshot) {
        let _ = self.restore_epoch(s);
    }

    /// As [`Store::restore`], and returns the epoch this reset moved to, read under the lock, so
    /// a later reset cannot move it first.
    #[must_use]
    pub fn restore_epoch(&self, s: &Snapshot) -> u64 {
        let mut state = self.inner.state.write();
        for (id, table) in &mut state.tx.tables {
            if let Some(seeded) = s.tables.get(id) {
                table.restore_dyn(&**seeded);
            }
        }
        let mut outbox = self.inner.outbox.lock();
        for c in outbox.take() {
            if c.over_budget {
                outbox.release_refused(c.entry.payload.len());
            } else {
                self.inner.release(c.entry.payload.len());
            }
        }
        drop(outbox);
        self.inner.epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Bounds the committed entries that wait for delivery: an entry that would make them more
    /// than `limit.events` entries or `limit.bytes` payload bytes is committed with
    /// [`Committed::over_budget`] set. Over-budget entries keep at most `limit.refused_bytes`
    /// payload bytes until they are released with [`Store::release_refused`], taken or not;
    /// past it the oldest of them still in the outbox lose their payload. At most
    /// `limit.refused_per_app` over-budget entries of one App wait in the outbox; past it the
    /// oldest of them is dropped and gives back its share.
    /// Without a call there is no bound.
    pub fn limit_outbox(&self, limit: OutboxUse) {
        *self.inner.limit.lock() = limit;
    }

    /// Gives back the share of the outbox limits that one committed entry of `bytes` payload
    /// bytes held, once its delivery starts or is dropped.
    pub fn release_outbox(&self, bytes: usize) {
        self.inner.release(bytes);
    }

    /// Gives back the share of [`OutboxUse::refused_bytes`] that one over-budget entry of
    /// `bytes` payload bytes held, once it is logged or dropped. A taken entry keeps its share
    /// until then.
    pub fn release_refused(&self, bytes: usize) {
        self.inner.outbox.lock().release_refused(bytes);
    }

    /// What the committed entries hold now.
    #[must_use]
    pub fn outbox_use(&self) -> OutboxUse {
        let outbox = self.inner.outbox.lock();
        let (refused_bytes, refused_per_app) = (outbox.refused_bytes, outbox.refused_per_app());
        drop(outbox);
        OutboxUse {
            refused_bytes,
            refused_per_app,
            ..*self.inner.used.lock()
        }
    }

    /// Takes every committed outbox entry, in commit sequence order.
    #[must_use]
    pub fn take_outbox(&self) -> Vec<Committed> {
        self.inner.outbox.lock().take()
    }

    /// Wakes the publisher after a commit adds outbox entries; a commit before the wait still wakes it.
    pub async fn outbox_ready(&self) {
        self.inner.outbox_ready.notified().await;
    }

    /// The current state epoch; [`Store::restore`] moves it on.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.inner.epoch.load(Ordering::SeqCst)
    }
}

/// Registers every table before start; no table is added after [`StoreBuilder::build`].
pub struct StoreBuilder {
    tables: HashMap<TypeId, Box<dyn AnyTable>>,
    deletions: Vec<DeletionSubscriber>,
}

impl Default for StoreBuilder {
    fn default() -> StoreBuilder {
        StoreBuilder::new()
    }
}

impl StoreBuilder {
    /// A builder that already holds the frozen entity tables, the counter table and the blob table.
    #[must_use]
    pub fn new() -> StoreBuilder {
        let mut b = StoreBuilder {
            tables: HashMap::new(),
            deletions: Vec::new(),
        };
        crate::tables::register_frozen(&mut b);
        b
    }

    /// # Panics
    ///
    /// Panics when `T` is already registered.
    pub fn register_table<T: Table + Default>(&mut self) -> &mut StoreBuilder {
        let previous = self
            .tables
            .insert(TypeId::of::<T>(), Box::new(T::default()));
        assert!(
            previous.is_none(),
            "table {} is registered twice",
            std::any::type_name::<T>()
        );
        self
    }

    /// Adds a subscriber that [`TxMut::delete`] runs: the crate that registers a table keyed by
    /// a repository, team or account removes its rows of the deleted entity there.
    pub fn on_delete(&mut self, f: DeletionSubscriber) -> &mut StoreBuilder {
        self.deletions.push(f);
        self
    }

    /// As [`StoreBuilder::build_with`] with [`Config::default`], for tests.
    #[must_use]
    pub fn build(self) -> Store {
        self.build_with(Arc::new(Config::default()))
    }

    #[must_use]
    pub fn build_with(self, config: Arc<Config>) -> Store {
        Store {
            inner: Arc::new(Inner {
                state: RwLock::new(State {
                    tx: Tx {
                        tables: self.tables,
                        config,
                        deletions: self.deletions.into(),
                    },
                    next_seq: 0,
                }),
                outbox: Mutex::new(Outbox::default()),
                outbox_ready: Notify::new(),
                epoch: AtomicU64::new(0),
                limit: Mutex::new(OutboxUse {
                    events: usize::MAX,
                    bytes: usize::MAX,
                    refused_bytes: usize::MAX,
                    refused_per_app: usize::MAX,
                }),
                used: Mutex::new(OutboxUse::default()),
            }),
        }
    }
}
