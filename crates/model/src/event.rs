/// A webhook event before hooks resolves its recipients.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub name: &'static str,
    pub action: Option<&'static str>,
    pub repo_id: Option<i64>,
    pub org_id: Option<i64>,
    pub sender_id: i64,
    /// Set with `installation_id` for installation-family events: only that App gets it, for that installation.
    pub app_id: Option<i64>,
    pub installation_id: Option<i64>,
    /// Built from hubdouble-render structs; hooks adds `installation`, `repository`, `organization` and `sender`.
    pub payload: serde_json::Map<String, serde_json::Value>,
}
