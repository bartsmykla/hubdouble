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
    #[error("App {0} does not exist or has no active webhook")]
    NoWebhook(i64),
    #[error("installation {0} does not belong to the App")]
    UnknownInstallation(i64),
    #[error("webhook {0} does not exist")]
    UnknownHook(i64),
}

impl From<HooksError> for ApiError {
    fn from(e: HooksError) -> ApiError {
        match e {
            HooksError::UnknownDelivery(_) | HooksError::UnknownHook(_) => ApiError::not_found(),
            HooksError::NoWebhook(_) | HooksError::UnknownInstallation(_) => {
                ApiError::unprocessable(e.to_string())
            }
            HooksError::NoRuntime | HooksError::Stopped => ApiError::internal(e.to_string()),
        }
    }
}

/// Renders `e`'s payload and resolves its recipients inside the transaction, then pushes one
/// [`hubdouble_store::OutboxEntry`] per recipient with [`TxMut::push_outbox`].
///
/// Besides the Apps below, every active [`crate::Hook`] of the repository and of its owner (or of
/// `org_id`) that subscribes to the event receives it, without `installation`, unless the event
/// names an App or an installation. Of `check_suite` such hooks get only `completed`, of
/// `check_run` only `created` and `completed`, and never an installation-family event.
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

/// An event that names itself in full, for one App, whatever the App subscribes to.
pub struct Injection {
    pub app_id: i64,
    pub installation_id: Option<i64>,
    pub repo_id: Option<i64>,
    pub sender_id: Option<i64>,
    pub event: String,
    pub action: Option<String>,
    pub payload: serde_json::Map<String, serde_json::Value>,
}

/// Queues one delivery of `injection` to its App, signed and logged as every other event.
/// `action`, `installation`, `repository`, `organization` and `sender` are added unless the
/// payload has them.
///
/// # Errors
///
/// [`HooksError::NoWebhook`] when the App does not exist or has no active webhook, and
/// [`HooksError::UnknownInstallation`] when the installation is not one of the App's.
pub fn inject(tx: &mut TxMut<'_>, injection: Injection) -> Result<(), HooksError> {
    crate::emit::inject(tx, injection)
}

/// Queues the `ping` GitHub sends when an App's webhook is set.
pub fn ping(tx: &mut TxMut<'_>, app_id: i64) {
    crate::emit::ping(tx, app_id);
}

/// Queues the `ping` GitHub sends when a repository or organization webhook is created or
/// pinged: `zen`, `hook_id`, `hook`, `repository` or `organization`, and `sender`.
///
/// # Errors
///
/// [`HooksError::UnknownHook`] when the hook or its repository or organization is gone.
pub fn ping_hook(
    tx: &mut TxMut<'_>,
    hook_id: i64,
    sender_id: Option<i64>,
) -> Result<(), HooksError> {
    crate::emit::ping_hook(tx, hook_id, sender_id)
}

/// Queues one delivery of `event` to one repository or organization webhook, whatever it
/// subscribes to, signed and logged as every other event. `repository` or `organization`, and
/// `sender`, are added unless the payload has them.
///
/// # Errors
///
/// [`HooksError::UnknownHook`] when the hook does not exist.
pub fn deliver_to_hook(
    tx: &mut TxMut<'_>,
    hook_id: i64,
    event: &'static str,
    sender_id: Option<i64>,
    payload: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), HooksError> {
    crate::emit::deliver_to_hook(tx, hook_id, event, sender_id, payload)
}

/// Queues a redelivery of a logged attempt: the same GUID and payload, a new delivery ID and
/// `redelivery: true`. `hook_id` is the App's ID for an App's webhook.
///
/// # Errors
///
/// [`HooksError::UnknownDelivery`] when the hook has no such delivery, or when the delivery lost
/// its body to [`Options::log_bytes`].
pub fn redeliver(tx: &mut TxMut<'_>, hook_id: i64, delivery_id: i64) -> Result<(), HooksError> {
    crate::emit::redeliver(tx, hook_id, delivery_id)
}

/// The default of [`Options::queue_bytes`].
pub const QUEUE_BYTES: usize = 32 * 1024 * 1024;

/// The tuning of the delivery runtime.
#[derive(Clone)]
pub struct Options {
    pub workers: usize,
    /// The most committed events that wait for an attempt or are in one.
    pub queue_capacity: usize,
    /// The most payload bytes that committed events hold until their attempt ends; `0` means
    /// [`QUEUE_BYTES`]. An event committed past either bound is logged at once as a failed
    /// delivery with `status_code` 0 and no attempt, so a writer never waits. Such events keep
    /// at most [`Options::log_bytes`] of payload before they are logged; past it the oldest of
    /// them are logged without their request.
    pub queue_bytes: usize,
    /// The timeout of one attempt.
    pub timeout: Duration,
    /// The delivery log ring size per App.
    pub log_capacity: usize,
    /// The most request and response bytes (headers and bodies) that in-flight, finishing and
    /// logged deliveries of every App hold together. Past it the oldest deliveries lose their
    /// request and response and keep their other fields; such a delivery cannot be read in full
    /// or redelivered.
    pub log_bytes: usize,
    /// The most bytes of a receiver's answer body that are read and logged; the rest is dropped.
    pub response_bytes: usize,
    pub connector: Arc<dyn Connector>,
    /// Called once per [`Notice`], with the runtime's lock held, so it must not wait.
    pub observer: Option<Observer>,
}

pub type Observer = Arc<dyn Fn(&Notice<'_>) + Send + Sync>;

/// What the delivery runtime reports to an [`Options::observer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice<'a> {
    /// A committed event got a delivery ID for one App.
    Event {
        app_id: i64,
        delivery_id: i64,
        event: &'a str,
        action: Option<&'a str>,
    },
    /// An attempt finished and is in the App's delivery log.
    Delivery {
        app_id: i64,
        delivery_id: i64,
        event: &'a str,
        action: Option<&'a str>,
        status_code: i64,
    },
}

/// The default of [`Options::response_bytes`].
pub const RESPONSE_BYTES: usize = 4 * 1024;

/// The default of [`Options::log_bytes`].
pub const LOG_BYTES: usize = 8 * 1024 * 1024;

impl Default for Options {
    fn default() -> Options {
        Options {
            workers: 4,
            queue_capacity: 10_000,
            queue_bytes: QUEUE_BYTES,
            timeout: Duration::from_secs(10),
            log_capacity: 50_000,
            log_bytes: LOG_BYTES,
            response_bytes: RESPONSE_BYTES,
            connector: Arc::new(TcpConnector),
            observer: None,
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
    /// The payload bytes that committed events hold until their attempt ends.
    pub waiting_bytes: usize,
    /// The request and response bytes that in-flight, finishing and logged deliveries hold,
    /// bounded by
    /// [`Options::log_bytes`].
    pub log_bytes: usize,
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
