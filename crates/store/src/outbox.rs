/// One webhook delivery to one App, rendered inside the transaction that caused it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxEntry {
    pub app_id: i64,
    pub installation_id: Option<i64>,
    pub repo_id: Option<i64>,
    pub event: &'static str,
    pub action: Option<&'static str>,
    /// The complete payload, `installation`, `repository`, `organization` and `sender` included.
    pub payload: serde_json::Value,
}

/// An outbox entry of a committed transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct Committed {
    /// Strictly increasing in commit order.
    pub seq: u64,
    /// The [`crate::Store::epoch`] the entry was committed in.
    pub epoch: u64,
    pub entry: OutboxEntry,
}
