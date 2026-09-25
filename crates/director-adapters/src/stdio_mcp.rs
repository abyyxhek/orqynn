//! A generic stdio JSON-RPC client for MCP tool servers.
//!
//! Both substrates Director talks to — handoff-mcp and ai-memory — are MCP
//! servers reached the same way: spawn the binary, speak line-delimited
//! JSON-RPC 2.0 over its stdin/stdout. That protocol is substrate-independent,
//! so it lives here once rather than being duplicated per adapter.
//!
//! What is *not* here is anything substrate-specific. An adapter owns its own
//! spawn arguments, its own environment, and its own identity handling;
//! [`StdioMcpTransport`] only speaks the protocol.
//!
//! ## Error envelope
//!
//! Every `tools/call` result is `{content:[{type:"text", text:"…"}]}`. When the
//! tool failed, the same shape carries `isError:true`. This is where a
//! substrate's failure vocabulary becomes a Rust error: the transport converts
//! `isError` into [`TransportError::ToolFailed`] without losing the message.

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// Protocol version we advertise at handshake. The server negotiates and
/// answers with its own; we send a version we know.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Errors the stdio transport itself can produce.
///
/// Kept generic on purpose: these are protocol failures, not substrate
/// failures, so the wording never names a server.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The server binary could not be started.
    #[error("could not spawn {path}: {source}")]
    SpawnFailed {
        /// The binary path we tried to launch.
        path: String,
        /// Why the spawn failed.
        source: std::io::Error,
    },
    /// The server closed or produced malformed JSON-RPC.
    #[error("malformed JSON-RPC response: {0}")]
    Malformed(String),
    /// A tool call returned `isError`.
    #[error("tool failed: {0}")]
    ToolFailed(String),
    /// A tool result did not contain any content.
    #[error("the server returned no content")]
    NoContent,
}

/// A request we send over the wire.
#[derive(Debug, Serialize)]
struct Request {
    /// Fixed JSON-RPC version marker.
    jsonrpc: &'static str,
    /// The caller-allocated request id, matched against the reply.
    id: u64,
    /// The MCP method being invoked.
    method: String,
    /// Its arguments, if any.
    params: Value,
}

/// A notification (no id, no reply expected).
#[derive(Debug, Serialize)]
struct Notification {
    /// Fixed JSON-RPC version marker.
    jsonrpc: &'static str,
    /// The MCP method being notified.
    method: String,
}

/// The envelope a `tools/call` result arrives in.
#[derive(Debug, Deserialize)]
struct ToolResult {
    /// Content blocks the tool returned; empty means no content.
    #[serde(default)]
    content: Vec<ContentBlock>,
    /// True when the tool itself reports the call failed.
    #[serde(default)]
    is_error: bool,
}

/// One block inside a tool result.
#[derive(Debug, Deserialize)]
struct ContentBlock {
    /// The block type, e.g. `"text"`.
    kind: String,
    /// The block's text payload, when it carries one.
    #[serde(default)]
    text: Option<String>,
}

/// One half of the raw JSON-RPC reply: either a result or an error.
#[derive(Debug, Deserialize)]
struct Reply {
    /// The id matching the request, when the server echoes one.
    id: Option<u64>,
    /// The successful result payload, when this is not an error reply.
    result: Option<Value>,
    /// The JSON-RPC error object, when the call failed at the protocol level.
    error: Option<JsonRpcError>,
}

/// A protocol-level error object.
#[derive(Debug, Deserialize)]
struct JsonRpcError {
    /// The human-oriented error message.
    message: String,
}

/// A live JSON-RPC connection to one MCP server child process.
pub struct StdioMcpTransport {
    /// The spawned server process.
    child: Child,
    /// Our write end of its stdin.
    stdin: ChildStdin,
    /// Buffered read end of its stdout.
    stdout: BufReader<ChildStdout>,
    /// The next request id to allocate.
    next_id: AtomicU64,
}

impl std::fmt::Debug for StdioMcpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioMcpTransport")
            .field("next_id", &self.next_id.load(Ordering::Relaxed))
            .finish()
    }
}

impl StdioMcpTransport {
    /// Spawn `binary` with `args`, speaking MCP over stdio.
    ///
    /// `extra_env` is applied to the child's environment and `env_remove` is
    /// dropped from it; both are substrate-specific concerns the caller owns.
    /// `cwd` sets the child's working directory. `client_name` identifies us in
    /// the `initialize` handshake.
    pub async fn spawn(
        binary: &str,
        args: &[&str],
        extra_env: &[(String, String)],
        env_remove: &[String],
        cwd: &std::path::Path,
        client_name: &str,
    ) -> Result<Self, TransportError> {
        let mut command = tokio::process::Command::new(binary);
        // Order matters: `Command` applies removals after sets, so anything a
        // caller wants cleared must be removed *before* it is set — otherwise
        // the removal also deletes the value the caller just installed.
        for key in env_remove {
            command.env_remove(key);
        }
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .current_dir(cwd);

        let mut child = command
            .spawn()
            .map_err(|source| TransportError::SpawnFailed {
                path: binary.to_string(),
                source,
            })?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let mut transport = StdioMcpTransport {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: AtomicU64::new(1),
        };

        transport.handshake(client_name).await?;
        Ok(transport)
    }

    /// `initialize` → `notifications/initialized`.
    async fn handshake(&mut self, client_name: &str) -> Result<(), TransportError> {
        let params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": client_name,
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        // The server's reply is checked but its protocol version is not
        // enforced: we send a version we support and accept whatever it offers.
        let _reply = self.request("initialize", params).await?;
        self.notify("notifications/initialized").await?;
        Ok(())
    }

    /// Send a notification and flush.
    async fn notify(&mut self, method: &str) -> Result<(), TransportError> {
        let notification = Notification {
            jsonrpc: "2.0",
            method: method.to_string(),
        };
        let line = serde_json::to_string(&notification).expect("notification serializes");
        self.write_line(&line).await
    }

    /// Send a request and await its reply.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, TransportError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Request {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };
        let line = serde_json::to_string(&request).expect("request serializes");
        self.write_line(&line).await?;

        loop {
            let mut line = String::new();
            let n = self
                .stdout
                .read_line(&mut line)
                .await
                .map_err(|e| TransportError::Malformed(format!("read failed: {e}")))?;
            if n == 0 {
                return Err(TransportError::Malformed(
                    "server closed the connection".to_string(),
                ));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let reply: Reply = serde_json::from_str(trimmed)
                .map_err(|e| TransportError::Malformed(format!("{e}: {trimmed}")))?;

            // The server emits replies in order; anything carrying our id is
            // the answer. Anything else is a notification we did not ask for
            // and can safely ignore for now.
            if reply.id != Some(id) {
                continue;
            }
            if let Some(err) = reply.error {
                return Err(TransportError::Malformed(format!(
                    "JSON-RPC error: {}",
                    err.message
                )));
            }
            return Ok(reply.result.unwrap_or(Value::Null));
        }
    }

    async fn write_line(&mut self, line: &str) -> Result<(), TransportError> {
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| TransportError::Malformed(format!("write failed: {e}")))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| TransportError::Malformed(format!("write failed: {e}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| TransportError::Malformed(format!("flush failed: {e}")))?;
        Ok(())
    }

    /// Call an MCP tool by name with the given JSON arguments.
    ///
    /// Returns the tool's textual content on success. When the tool reports
    /// `isError`, this is [`Err(TransportError::ToolFailed)`] — the one place a
    /// substrate failure becomes a Rust error the caller can match on.
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<String, TransportError> {
        let params = serde_json::json!({ "name": name, "arguments": arguments });
        let result = self.request("tools/call", params).await?;
        let envelope: ToolResult = serde_json::from_value(result)
            .map_err(|e| TransportError::Malformed(format!("bad result envelope: {e}")))?;

        if envelope.is_error {
            return Err(TransportError::ToolFailed(
                envelope
                    .content
                    .into_iter()
                    .next()
                    .and_then(|c| c.text)
                    .unwrap_or_else(|| "unknown error".to_string()),
            ));
        }

        envelope
            .content
            .into_iter()
            .next()
            .and_then(|c| if c.kind == "text" { c.text } else { None })
            .ok_or(TransportError::NoContent)
    }

    /// Shut the server down.
    pub async fn shutdown(&mut self) {
        // Best effort: a tool server has no mandatory shutdown handshake, and
        // the process takes its queues when the pipe closes.
        let _ = self.notify("shutdown").await;
        let _ = self.stdin.shutdown().await;
        let _ = self.child.kill().await;
    }
}

impl Drop for StdioMcpTransport {
    fn drop(&mut self) {
        // If the async runtime is gone, `kill` on the raw handle is the
        // remaining option; leaving an orphan server is a resource leak, not
        // a correctness problem.
        let _ = self.child.start_kill();
    }
}
