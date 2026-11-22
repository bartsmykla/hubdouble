use std::sync::Arc;
use std::time::Duration;

use hubdouble_model::{ApiError, Clock, Event};
use hubdouble_store::{Snapshot, Store, TxMut};

use crate::delivery::{Connector, TcpConnector};
use crate::runtime::Shared;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HooksError {
    #[error("the webhook workers need a tokio runtime")]
    NoRuntime,
    #[error("the webhook publisher has stopped")]
    Stopped,
    #[error("delivery {0} does not exist")]
    UnknownDelivery(i64),
}

impl From<HooksError> for ApiError {
    fn from(e: HooksError) -> ApiError {
        match e {
            HooksError::UnknownDelivery(_) => ApiError::not_found(),
            HooksError::NoRuntime | HooksError::Stopped => ApiError::internal(e.to_string()),
        }
    }
}

/// Renders `e`'s payload and resolves its recipients inside the transaction, then pushes one
/// [`hubdouble_store::OutboxEntry`] per recipient with [`TxMut::push_outbox`].
///
/// With `installation_id` set, only that installation's App receives it (and only when
/// `app_id`, if set, names that App). With only `app_id` set, only that App receives it.
/// Otherwise every unsuspended installation on the repository's owner (or on `org_id`) that
/// covers the repository receives it. In every case the App needs an active webhook and must
/// subscribe to the event, except for the installation-family events, which always go out.
/// `action`, `installation`, `repository`, `organization` and `sender` are added unless the
/// payload has them.
pub fn emit(tx: &mut TxMut<'_>, e: Event) {
    crate::emit::emit(tx, e);
}

/// Queues the `ping` GitHub sends when an App's webhook is set.
pub fn ping(tx: &mut TxMut<'_>, app_id: i64) {
    crate::emit::ping(tx, app_id);
}

/// Queues a redelivery of a logged attempt: the same GUID and payload, a new delivery ID and
/// `redelivery: true`.
///
/// # Errors
///
/// [`HooksError::UnknownDelivery`] when the App has no such delivery.
pub fn redeliver(tx: &mut TxMut<'_>, app_id: i64, delivery_id: i64) -> Result<(), HooksError> {
    crate::emit::redeliver(tx, app_id, delivery_id)
}

/// The default of [`Options::queue_bytes`].
pub const QUEUE_BYTES: usize = 32 * 1024 * 1024;

/// The tuning of the delivery runtime.
#[derive(Clone)]
pub struct Options {
    pub workers: usize,
    /// The most committed events that wait for an attempt.
    pub queue_capacity: usize,
    /// The most payload bytes that committed events waiting for an attempt hold; `0` means
    /// [`QUEUE_BYTES`]. An event committed past either bound is logged at once as a failed
    /// delivery with `status_code` 0 and no attempt, so a writer never waits.
    pub queue_bytes: usize,
    /// The timeout of one attempt.
    pub timeout: Duration,
    /// The delivery log ring size per App.
    pub log_capacity: usize,
    pub connector: Arc<dyn Connector>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            workers: 4,
            queue_capacity: 10_000,
            queue_bytes: QUEUE_BYTES,
            timeout: Duration::from_secs(10),
            log_capacity: 50_000,
            connector: Arc::new(TcpConnector),
        }
    }
}

/// A point-in-time view of the delivery runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub queued: usize,
    /// The deepest the worker queue has been.
    pub max_queued: usize,
    pub in_flight: usize,
    /// Deliveries with an ID that are not in the delivery log yet.
    pub unlogged: usize,
    /// The payload bytes that committed events waiting for an attempt hold.
    pub waiting_bytes: usize,
}

/// The running publisher, delivery queue and workers.
#[derive(Clone)]
pub struct Handle {
    pub(crate) shared: Arc<Shared>,
}

impl Handle {
    /// Starts the outbox publisher and the delivery workers on the current tokio runtime.
    ///
    /// # Errors
    ///
    /// [`HooksError`] when the workers cannot start.
    pub fn start(store: Store, clock: Arc<dyn Clock>) -> Result<Handle, HooksError> {
        Handle::start_with(store, clock, Options::default())
    }

    /// As [`Handle::start`], with explicit [`Options`]. Payload URLs come from the store's
    /// [`hubdouble_store::Tx::config`].
    ///
    /// # Errors
    ///
    /// [`HooksError::NoRuntime`] outside a tokio runtime.
    pub fn start_with(
        store: Store,
        clock: Arc<dyn Clock>,
        options: Options,
    ) -> Result<Handle, HooksError> {
        Ok(Handle {
            shared: Shared::start(store, clock, options)?,
        })
    }

    /// Returns once every event committed before the call has left the outbox and the
    /// publisher, the worker queue holds none of them, no attempt for them is in flight and
    /// every attempt is in the delivery log.
    ///
    /// # Errors
    ///
    /// [`HooksError::Stopped`] after [`Handle::shutdown`].
    pub async fn drain(&self) -> Result<(), HooksError> {
        self.shared.drain().await
    }

    /// Restores `snapshot` with [`Store::restore`], which clears the outbox and moves to a new
    /// epoch under the store lock, then clears the worker queue and returns once every attempt
    /// already in flight has ended. No attempt of an older epoch starts or logs after it returns.
    ///
    /// # Errors
    ///
    /// [`HooksError::Stopped`] when the runtime is dropped while it waits.
    pub async fn reset(&self, snapshot: &Snapshot) -> Result<(), HooksError> {
        self.shared.reset(snapshot).await
    }

    #[must_use]
    pub fn stats(&self) -> Stats {
        self.shared.stats()
    }

    /// Stops the publisher and the workers after their current attempts.
    pub async fn shutdown(&self) {
        self.shared.shutdown().await;
    }
}
