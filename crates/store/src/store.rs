use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hubdouble_model::ApiError;
use parking_lot::{Mutex, RwLock};
use tokio::sync::Notify;

use crate::counters::Counters;
use crate::outbox::{Committed, OutboxEntry};

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

/// The state inside the lock: every registered table.
pub struct Tx {
    tables: HashMap<TypeId, Box<dyn AnyTable>>,
}

impl Tx {
    /// # Panics
    ///
    /// Panics when `T` was never registered on the [`StoreBuilder`].
    #[must_use]
    pub fn table<T: Table>(&self) -> &T {
        self.tables
            .get(&TypeId::of::<T>())
            .and_then(|t| (&**t as &dyn Any).downcast_ref::<T>())
            .unwrap_or_else(|| panic!("table {} is not registered", std::any::type_name::<T>()))
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

struct Inner {
    state: RwLock<State>,
    outbox: Mutex<Vec<Committed>>,
    outbox_ready: Notify,
    epoch: AtomicU64,
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
            let mut outbox = self.inner.outbox.lock();
            for entry in entries {
                *next_seq += 1;
                outbox.push(Committed {
                    seq: *next_seq,
                    epoch,
                    entry,
                });
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
    /// a new epoch, so hooks can refuse to start an attempt committed before the reset.
    pub fn restore(&self, s: &Snapshot) {
        let mut state = self.inner.state.write();
        for (id, table) in &mut state.tx.tables {
            if let Some(seeded) = s.tables.get(id) {
                table.restore_dyn(&**seeded);
            }
        }
        self.inner.outbox.lock().clear();
        self.inner.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Takes every committed outbox entry, in commit sequence order.
    #[must_use]
    pub fn take_outbox(&self) -> Vec<Committed> {
        std::mem::take(&mut *self.inner.outbox.lock())
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

    #[must_use]
    pub fn build(self) -> Store {
        Store {
            inner: Arc::new(Inner {
                state: RwLock::new(State {
                    tx: Tx {
                        tables: self.tables,
                    },
                    next_seq: 0,
                }),
                outbox: Mutex::new(Vec::new()),
                outbox_ready: Notify::new(),
                epoch: AtomicU64::new(0),
            }),
        }
    }
}
