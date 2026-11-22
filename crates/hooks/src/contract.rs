use std::sync::Arc;

use hubdouble_model::{ApiError, Clock, Config, Event};
use hubdouble_store::{Store, TxMut};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HooksError {
    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
}

impl From<HooksError> for ApiError {
    fn from(e: HooksError) -> ApiError {
        match e {
            HooksError::NotImplemented(what) => ApiError::not_implemented(what),
        }
    }
}

/// Renders `e`'s payload and resolves its recipients inside the transaction, then pushes one
/// [`hubdouble_store::OutboxEntry`] per recipient with [`TxMut::push_outbox`].
pub fn emit(tx: &mut TxMut<'_>, e: Event) {
    let _ = (tx, e);
}

/// The running publisher, delivery queue and workers.
#[derive(Clone)]
pub struct Handle {
    store: Store,
}

impl Handle {
    /// Starts the outbox publisher and the delivery workers on the current tokio runtime.
    ///
    /// # Errors
    ///
    /// [`HooksError`] when the workers cannot start.
    pub fn start(
        store: Store,
        clock: Arc<dyn Clock>,
        config: Arc<Config>,
    ) -> Result<Handle, HooksError> {
        let _ = (clock, config);
        Ok(Handle { store })
    }

    /// Returns once every event committed before the call has been delivered and logged.
    ///
    /// # Errors
    ///
    /// [`HooksError`] when draining fails.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "the contract is async and the P0-E body awaits"
    )]
    pub async fn drain(&self) -> Result<(), HooksError> {
        let _ = &self.store;
        Err(HooksError::NotImplemented("hooks/drain"))
    }

    /// Stops the publisher and the workers after their current attempts.
    #[expect(
        clippy::unused_async,
        reason = "the contract is async and the P0-E body awaits"
    )]
    pub async fn shutdown(&self) {}
}
