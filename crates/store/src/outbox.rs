use bytes::Bytes;

/// One webhook delivery to one App, rendered inside the transaction that caused it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxEntry {
    pub app_id: i64,
    pub installation_id: Option<i64>,
    pub repo_id: Option<i64>,
    pub event: &'static str,
    pub action: Option<&'static str>,
    /// The complete payload as the JSON body, `installation`, `repository`, `organization` and
    /// `sender` included.
    pub payload: Bytes,
    /// The `X-GitHub-Delivery` GUID of the delivery a redelivery repeats; `None` for a new one.
    pub redelivery_of: Option<String>,
}

/// An outbox entry of a committed transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct Committed {
    /// Strictly increasing in commit order.
    pub seq: u64,
    /// The [`crate::Store::epoch`] the entry was committed in.
    pub epoch: u64,
    /// The entry did not fit the limits of [`crate::Store::limit_outbox`] at commit. It holds
    /// no share of them; any other entry holds one until [`crate::Store::release_outbox`].
    /// Such an entry keeps its payload only while it is among the newest over-budget entries
    /// within [`OutboxUse::refused_bytes`]; otherwise its payload is empty.
    pub over_budget: bool,
    pub entry: OutboxEntry,
}

/// What committed entries that fit the limits hold until they are released.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutboxUse {
    pub events: usize,
    pub bytes: usize,
    /// The payload bytes that over-budget entries keep until [`crate::Store::take_outbox`].
    pub refused_bytes: usize,
}
