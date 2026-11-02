//! A handler's connection lifecycle: schemas alone, a ready client, or connection on first use.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::OnceCell;

use super::McpHandler;
use crate::tool::ToolError;
use crate::tool::mcp::{McpClient, McpClientPool, McpServerEntry};

pub(super) enum McpConnection {
    SpecOnly,
    Ready(Arc<McpClient>),
    Deferred {
        entry: Box<McpServerEntry>,
        propagate_trace_context: bool,
        client: OnceCell<Arc<McpClient>>,
    },
}

impl McpHandler {
    #[must_use]
    pub const fn discovered_tool_spec_only() -> Self {
        Self {
            connection: McpConnection::SpecOnly,
        }
    }

    #[must_use]
    pub fn tool_call(client: Arc<McpClient>) -> Self {
        Self {
            connection: McpConnection::Ready(client),
        }
    }

    /// A retained listing needs schemas now, but a connection only if a tool is called.
    pub(crate) fn deferred(entry: McpServerEntry, propagate_trace_context: bool) -> Self {
        Self {
            connection: McpConnection::Deferred {
                entry: Box::new(entry),
                propagate_trace_context,
                client: OnceCell::new(),
            },
        }
    }
}

impl McpConnection {
    pub(super) async fn client(&self, server_label: &str) -> Result<&Arc<McpClient>, ToolError> {
        match self {
            Self::SpecOnly => Err(ToolError::Config(
                "MCP tool spec-only handler cannot execute tools".to_owned(),
            )),
            Self::Ready(client) => Ok(client),
            Self::Deferred {
                entry,
                propagate_trace_context,
                client,
            } => {
                client
                    .get_or_try_init(|| async {
                        let servers = HashMap::from([(server_label.to_owned(), entry.as_ref().clone())]);
                        let pool = if *propagate_trace_context {
                            McpClientPool::from_config(servers).await
                        } else {
                            McpClientPool::from_request_entries(servers).await
                        };
                        pool.get(server_label).cloned().ok_or_else(|| {
                            ToolError::Execution(format!(
                                "MCP server '{server_label}' failed to connect: {}",
                                pool.connection_error(server_label)
                                    .unwrap_or("unknown connection error")
                            ))
                        })
                    })
                    .await
            }
        }
    }
}
