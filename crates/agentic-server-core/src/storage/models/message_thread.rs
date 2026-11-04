//! Database row for a Messages thread checkpoint and its continuation lease.

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct MessageThread {
    pub checkpoint: Option<String>,
    pub state: String,
    pub lease_until: i64,
}
