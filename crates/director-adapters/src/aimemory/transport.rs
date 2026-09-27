//! ai-memory's connection, on top of the shared stdio transport.
//!
//! ## What is substrate-specific here
//!
//! Almost nothing, and that is the point. The JSON-RPC plumbing is generic and
//! lives in [`crate::stdio_mcp`]; handoff-mcp needed a wrapper because it bakes
//! a *process-wide agent identity* into the child's environment. ai-memory has
//! no such notion: every call is scoped by arguments, not by the process it
//! arrives in.
//!
//! The one thing this module owns is therefore the **spawn shape**: which
//! subcommand runs the MCP server, and which data directory it keeps its wiki
//! and index in. The data dir matters more than it looks — Director points it
//! at a scratch directory in tests and a dedicated one in production, so the
//! adapter's pages never collide with the operator's real memory.
//!
//! ## Static-client scoping
//!
//! ai-memory routes project scope from, in order: explicit `workspace` +
//! `project` arguments, the caller's working directory marker file, or an
//! active-project pointer keyed by *session* identity. Director is a static
//! MCP client — it spawns a child and speaks JSON-RPC, and there is no
//! lifecycle-hook session id bridged onto those requests — so only the first
//! of those is available to it. The adapter therefore sends `workspace` and
//! `project` on **every** call, which also pins the read path to exactly one
//! project: no fallback to some other project's memory, ever.

use std::path::Path;

use crate::stdio_mcp::StdioMcpTransport;

/// Re-exported so the adapter's error vocabulary and its callers can name the
/// transport's failure type without reaching into the shared module.
pub use crate::stdio_mcp::TransportError;

/// The subcommand that runs the MCP server over stdio.
const SERVE_ARGS: [&str; 3] = ["serve", "--transport", "stdio"];

/// Env var ai-memory's config loader reads as a data-directory override.
const DATA_DIR_ENV: &str = "AI_MEMORY_DATA_DIR";

/// A live JSON-RPC connection to one ai-memory child process.
///
/// Thin by design: the shared transport plus the spawn shape. No identity, no
/// per-call state — the adapter owns scoping, this owns the pipe.
pub struct MemoryTransport {
    /// The protocol layer, owned so `Drop` still reaps the child.
    inner: StdioMcpTransport,
}

impl std::fmt::Debug for MemoryTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryTransport").finish_non_exhaustive()
    }
}

impl MemoryTransport {
    /// Spawn the server with its wiki and index rooted at `data_dir`.
    ///
    /// `data_dir` is also the child's working directory. Passing it twice — as
    /// `--data-dir` and as `AI_MEMORY_DATA_DIR` — is deliberate: the flag is
    /// what the CLI parses, and the env var is what any code path that reads
    /// config outside the CLI sees, so setting only one leaves the other half
    /// of the server looking at a different directory.
    pub async fn spawn(binary: &str, data_dir: &Path) -> Result<Self, TransportError> {
        let data_dir_str = data_dir.to_string_lossy().into_owned();
        let inner = StdioMcpTransport::spawn(
            binary,
            &SERVE_ARGS,
            &[(DATA_DIR_ENV.to_string(), data_dir_str)],
            &[],
            data_dir,
            "director-brain-aimemory",
        )
        .await?;
        Ok(MemoryTransport { inner })
    }

    /// Call an ai-memory tool by name with the given JSON arguments.
    ///
    /// Returns the tool's textual content on success. When the tool reports
    /// `isError`, this is [`Err(TransportError::ToolFailed)`] — the one place a
    /// substrate failure becomes a Director error.
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, TransportError> {
        self.inner.call_tool(name, arguments).await
    }

    /// Shut the server down.
    pub async fn shutdown(&mut self) {
        self.inner.shutdown().await;
    }
}
