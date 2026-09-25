//! handoff-mcp's connection, on top of the shared stdio transport.
//!
//! ## What is substrate-specific here
//!
//! The JSON-RPC plumbing is generic to any MCP server and lives in
//! [`crate::stdio_mcp`]. This module adds the one thing that is genuinely
//! handoff-mcp's own: **identity**. handoff-mcp holds a single process-wide
//! agent identity (`router::set_agent_id`), established by `handoff_load_context`
//! from the `CLAUDE_SESSION_ID` environment variable. There is no per-request
//! agent id, so this wrapper *bakes one identity into the child's environment
//! at spawn time* rather than passing it per call. Verified: with the variable
//! set, `load_context` reports that exact string back as `agent_id`.
//!
//! The consequence is a real constraint, not a limitation of this code: **one
//! adapter instance speaks as one agent**. A Director process multiplexing
//! several agents needs one child process per agent identity. The pool that
//! manages that is Phase 8.
//!
//! ## Error envelope
//!
//! [`TransportError`] is re-exported from [`crate::stdio_mcp`] so the handoff
//! adapter's error vocabulary stays one type across both layers; its
//! `ToolFailed` variant is where a substrate failure becomes a Rust error.

use std::path::Path;

use director_domain::ids::AgentId;

use crate::stdio_mcp::StdioMcpTransport;

// Re-exported so the handoff adapter's error type and its callers can name the
// transport's failure vocabulary without reaching into the shared module.
pub use crate::stdio_mcp::TransportError;

/// Env var handoff-mcp reads to derive a stable agent identity.
pub(crate) const SESSION_ID_ENV: &str = "CLAUDE_SESSION_ID";

/// A live JSON-RPC connection to one handoff-mcp child process.
///
/// Thin by design: it is the shared transport plus the baked-in identity that
/// handoff-mcp's process-global agent model requires.
pub struct McpTransport {
    /// The protocol layer, owned so `Drop` still reaps the child.
    inner: StdioMcpTransport,
    /// The identity baked into this connection's environment at spawn time.
    agent_id: AgentId,
}

impl std::fmt::Debug for McpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTransport")
            .field("agent_id", &self.agent_id)
            .finish()
    }
}

impl McpTransport {
    /// The identity this connection speaks as.
    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    /// Spawn the server, speaking as `agent_id`.
    ///
    /// `binary` is the path to `handoff-mcp(.exe)`. `project_dir` is only used to
    /// set the child's working directory; every tool call also passes it
    /// explicitly because the server resolves projects from arguments, not cwd.
    pub async fn spawn(
        binary: &str,
        agent_id: AgentId,
        project_dir: &Path,
    ) -> Result<Self, crate::stdio_mcp::TransportError> {
        let transport = StdioMcpTransport::spawn(
            binary,
            &[],
            &[(SESSION_ID_ENV.to_string(), agent_id.as_str().to_string())],
            // handoff-mcp uses CLAUDE_SESSION_ID for its own identity; make
            // sure no ambient value from the parent environment wins. The
            // shared transport removes before it sets, so this ordering is what
            // makes ours stick.
            &[SESSION_ID_ENV.to_string()],
            project_dir,
            "director-brain-handoff",
        )
        .await?;

        Ok(McpTransport {
            inner: transport,
            agent_id,
        })
    }

    /// Call a handoff-mcp tool by name with the given JSON arguments.
    ///
    /// Returns the tool's textual content on success. When the tool reports
    /// `isError`, this is [`Err(crate::stdio_mcp::TransportError::ToolFailed)`] — the one place a
    /// substrate failure becomes a Director error.
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, crate::stdio_mcp::TransportError> {
        self.inner.call_tool(name, arguments).await
    }

    /// Shut the server down.
    pub async fn shutdown(&mut self) {
        self.inner.shutdown().await;
    }
}
