//! A generic stdio JSON-RPC client for MCP tool servers.
//!
//! Both substrates Orqyn talks to — handoff-mcp and ai-memory — are MCP
//! servers reached the same way: spawn the binary, speak line-delimited
//! JSON-RPC 2.0 over its stdin/stdout. That protocol is substrate-independent,
//! so it lives here once rather than being duplicated per adapter.
//!
//! What is *not* here is anything substrate-specific. An adapter owns its own
//! spawn arguments, its own environment, and its own identity handling;
//! [`StdioMcpTransport`] only speaks the protocol.
//!
//! ## Nothing waits forever
//!
//! A child process is the easiest way in a program to introduce a wait that
//! never ends: `read_line` on a pipe whose writer has gone quiet, a write to a
//! pipe nobody is draining, a `kill().await` on a process that ignores the
//! signal. Every blocking operation here is therefore bounded by
//! [`McpTimeouts`] and every expiry is reported as [`TransportError::Timeout`]
//! — a timeout is never reported as malformed input, and never as an empty or
//! successful response.
//!
//! ## A failed transport is failed, not merely unlucky
//!
//! A timeout part-way through a read leaves the stream in a state this client
//! cannot reason about: the buffer may hold the tail of a line whose head is
//! gone. Rather than guess, the transport **poisons itself** — it records why,
//! terminates the child, and refuses every subsequent request without touching
//! the wire. That is what makes a late reply impossible to mistake for the
//! answer to a later call, which id correlation alone does not fully prevent:
//! correlation protects against the wrong *id*, not against a stream that is
//! no longer aligned to a line boundary.
//!
//! ## Diagnostics
//!
//! The child's stderr is captured into a bounded tail and kept separate from the
//! JSON-RPC stream. It is never logged automatically and never mixed into a
//! protocol message — a server that writes a warning to stderr must not break a
//! call, and a caller that needs the text can ask for it deliberately.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};

/// Protocol version we advertise at handshake. The server negotiates and
/// answers with its own; we send a version we know.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// How much of the child's stderr is retained for diagnostics.
///
/// Bounded on purpose: a server in a loop would otherwise grow this without
/// limit. Only the most recent bytes are kept, because the thing that explains
/// a failure is nearly always the last thing printed before it.
const STDERR_TAIL_BYTES: usize = 64 * 1024;

/// Which operation ran out of time.
///
/// Reported rather than flattened into one error so a caller can tell a server
/// that never started talking from one that started and then stalled, and can
/// retry accordingly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Spawning the child or completing `initialize`.
    Handshake,
    /// Writing a request or notification to the child's stdin.
    Write,
    /// Waiting for a `tools/call` reply.
    ToolCall,
    /// Stopping the child.
    Shutdown,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Phase::Handshake => "the MCP handshake",
            Phase::Write => "writing to the MCP server",
            Phase::ToolCall => "an MCP tool call",
            Phase::Shutdown => "shutting the MCP server down",
        })
    }
}

/// How long each part of the MCP lifecycle may take.
///
/// One configuration type for the whole substrate path, deliberately: the
/// adapter's lock and the transport's reads are stages of one operation, and a
/// caller who has to configure them in two places will configure one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpTimeouts {
    /// Spawn plus `initialize` and `notifications/initialized`.
    pub handshake: Duration,
    /// One `tools/call`, write and reply together.
    pub tool_call: Duration,
    /// A single write and flush to the child's stdin.
    pub write: Duration,
    /// The shutdown notification and the child's exit.
    pub shutdown: Duration,
    /// How long a caller waits to *become* the caller that talks to the server.
    ///
    /// Not a transport concern — the transport is single-tenant and never takes a
    /// lock — but it lives here because it bounds the same operation from the
    /// other end. See [`crate::stdio_mcp`]'s callers for what it guarantees.
    pub lock: Duration,
}

/// Defaults are deliberately generous: a timeout that fires during a legitimate
/// full-text query or a large task listing would be reported as a server
/// failure when the server is working fine. The point of these bounds is to
/// catch a *wedged* server, not to police a slow one.
///
/// # This is not an end-to-end deadline
///
/// The stages stack. A caller that acquires the adapter's lock at the last
/// moment still waits its full `tool_call` budget afterwards, so the worst case
/// for one adapter call is `lock + tool_call`, and nothing here enforces a single
/// deadline across both. That is deliberate: a shared deadline would mean one
/// caller's latency budget silently shortening another's request window, which
/// is a worse failure than a slightly looser bound.
impl Default for McpTimeouts {
    fn default() -> Self {
        McpTimeouts {
            // A server that cannot handshake in half a minute is not going to.
            handshake: Duration::from_secs(30),
            // The widest bound, because this is where real work happens: an
            // FTS query or a full task tree on a large project is legitimately
            // slow.
            tool_call: Duration::from_secs(120),
            // A write to a local pipe blocks only if the child has stopped
            // reading, which is already a failure.
            write: Duration::from_secs(30),
            // Shutdown is best effort by design; a server that will not go
            // quietly is killed rather than waited on.
            shutdown: Duration::from_secs(5),
            // Shorter than `tool_call` on purpose. Only one request can be in
            // flight per transport, so a caller that cannot get the lock is
            // queueing behind somebody else's whole request; it should be told
            // that within half a tool call rather than silently inheriting the
            // full budget and appearing to hang.
            lock: Duration::from_secs(30),
        }
    }
}

impl McpTimeouts {
    /// Timeouts short enough to exercise every failure path in a test without
    /// the test taking minutes. Not for production use.
    pub fn fast() -> Self {
        McpTimeouts {
            handshake: Duration::from_millis(600),
            tool_call: Duration::from_millis(600),
            write: Duration::from_millis(600),
            shutdown: Duration::from_millis(300),
            lock: Duration::from_millis(300),
        }
    }

    /// Bounds sized for Orqyn's checkpoint publication path.
    ///
    /// Every per-operation bound sits under `MEMORY_PUBLISH_TIMEOUT` (10s, in
    /// `director-app`) so the transport reports its own timeout and poisons the
    /// connection deliberately, rather than being cancelled part-way through a
    /// read by the application layer's outer bound.
    pub fn for_checkpoint_publication() -> Self {
        McpTimeouts {
            handshake: Duration::from_secs(5),
            tool_call: Duration::from_secs(8),
            write: Duration::from_secs(2),
            shutdown: Duration::from_secs(3),
            lock: Duration::from_secs(2),
        }
    }

    /// True when every bound is non-zero, which `StdioMcpTransport::spawn`
    /// requires: a zero bound would make the transport refuse everything.
    pub fn is_usable(&self) -> bool {
        !self.handshake.is_zero()
            && !self.tool_call.is_zero()
            && !self.write.is_zero()
            && !self.shutdown.is_zero()
            && !self.lock.is_zero()
    }
}

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
    /// The server produced something that is not JSON-RPC.
    ///
    /// Only for genuinely unreadable output. A timeout is never reported here,
    /// and neither is a JSON-RPC error object — that has its own variant so
    /// its code survives.
    #[error("malformed JSON-RPC response: {0}")]
    Malformed(String),
    /// A tool call returned `isError`.
    #[error("tool failed: {0}")]
    ToolFailed(String),
    /// A tool result did not contain any content.
    #[error("the server returned no content")]
    NoContent,
    /// The server closed the connection before answering.
    #[error("the MCP server closed the connection: {0}")]
    Disconnected(String),
    /// An operation exceeded its bound and was abandoned.
    #[error("{phase} did not finish within {limit:?}")]
    Timeout {
        /// Which operation ran out of time.
        phase: Phase,
        /// The bound that elapsed.
        limit: Duration,
    },
    /// The transport can no longer be used and was terminated.
    ///
    /// Returned by every call after the transport has poisoned itself, so a
    /// caller learns that it must re-spawn rather than retrying against a
    /// stream nobody is reading.
    #[error("the MCP transport was abandoned and cannot be reused: {reason}")]
    Desynchronized {
        /// What ended the transport's usability.
        reason: String,
    },
    /// The server answered with a JSON-RPC error object.
    ///
    /// Carries the numeric code. That code is the only machine-readable
    /// classification a server gives, and folding it into a string message —
    /// as this transport used to — throws away the one part a caller could act
    /// on.
    #[error("the MCP server returned error {code}: {message}")]
    Protocol {
        /// The JSON-RPC error code.
        code: i64,
        /// The server's message.
        message: String,
    },
    /// A configured timeout was zero, which would refuse every operation.
    #[error("the MCP timeouts are unusable: {0}")]
    InvalidTimeouts(String),
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
    /// The block type, e.g. `"text"`. Serialized as `type` by the MCP spec and
    /// by both substrates; `kind` is accepted as an alias for a client that
    /// sends the older name.
    #[serde(rename = "type", alias = "kind")]
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
    /// The machine-readable classification the server chose.
    #[serde(default)]
    code: i64,
    /// The human-oriented error message.
    message: String,
}

/// The most recent bytes a child wrote to stderr.
///
/// A ring, not a buffer: the tail is what explains a failure, and an unbounded
/// buffer would be a leak in a process whose whole job is talking to servers
/// that might be looping.
#[derive(Debug, Default)]
struct StderrTail {
    bytes: VecDeque<u8>,
}

impl StderrTail {
    fn push(&mut self, chunk: &[u8]) {
        for byte in chunk {
            if self.bytes.len() == STDERR_TAIL_BYTES {
                self.bytes.pop_front();
            }
            self.bytes.push_back(*byte);
        }
    }

    fn text(&self) -> String {
        let contiguous: Vec<u8> = self.bytes.iter().copied().collect();
        String::from_utf8_lossy(&contiguous).into_owned()
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }
}

/// Drain a child's stderr into a bounded tail, forever, until the pipe closes.
///
/// Draining continuously is not optional: a child that fills an undrained
/// stderr pipe blocks on its next write, and the symptom would be a server that
/// appears to hang.
fn capture_stderr(stderr: ChildStderr, tail: Arc<StdMutex<StderrTail>>) {
    use tokio::io::AsyncReadExt;
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut guard) = tail.lock() {
                        guard.push(&buf[..n]);
                    }
                }
            }
        }
    });
}

/// A live JSON-RPC connection to one MCP server child process.
///
/// Single-tenant by construction: every operation takes `&mut self`, so two
/// calls can never interleave on one stream and a reply can never be attributed
/// to the wrong request. Adapters that need to be shared put a lock in front,
/// and that lock is bounded by the tool-call timeout this transport enforces.
pub struct StdioMcpTransport {
    /// The spawned server process.
    child: Child,
    /// Our write end of its stdin.
    stdin: ChildStdin,
    /// Buffered read end of its stdout.
    stdout: BufReader<ChildStdout>,
    /// The next request id to allocate.
    next_id: AtomicU64,
    /// The bounds applied to every blocking operation.
    timeouts: McpTimeouts,
    /// Set once the stream can no longer be trusted. After this is set every
    /// operation fails immediately rather than touching the wire.
    failed: Option<String>,
    /// The child's stderr, retained separately from the protocol stream.
    stderr_tail: Arc<StdMutex<StderrTail>>,
}

impl std::fmt::Debug for StdioMcpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately omits the stderr tail: it is arbitrary text from a
        // third party and has no business in a debug dump.
        f.debug_struct("StdioMcpTransport")
            .field("next_id", &self.next_id.load(Ordering::Relaxed))
            .field("timeouts", &self.timeouts)
            .field("failed", &self.failed)
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
    ///
    /// Uses [`McpTimeouts::default`]. Use [`Self::spawn_with_timeouts`] to
    /// change the bounds.
    pub async fn spawn(
        binary: &str,
        args: &[&str],
        extra_env: &[(String, String)],
        env_remove: &[String],
        cwd: &std::path::Path,
        client_name: &str,
    ) -> Result<Self, TransportError> {
        Self::spawn_with_timeouts(
            binary,
            args,
            extra_env,
            env_remove,
            cwd,
            client_name,
            McpTimeouts::default(),
        )
        .await
    }

    /// [`Self::spawn`] with explicit bounds on every blocking operation.
    pub async fn spawn_with_timeouts(
        binary: &str,
        args: &[&str],
        extra_env: &[(String, String)],
        env_remove: &[String],
        cwd: &std::path::Path,
        client_name: &str,
        timeouts: McpTimeouts,
    ) -> Result<Self, TransportError> {
        if !timeouts.is_usable() {
            return Err(TransportError::InvalidTimeouts(
                "every bound must be greater than zero".into(),
            ));
        }

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
            // Piped rather than null so a server's diagnostics survive, and
            // drained into a bounded tail. Writing to stdout would corrupt the
            // JSON-RPC stream.
            .stderr(Stdio::piped())
            .current_dir(cwd);

        let mut child = command
            .spawn()
            .map_err(|source| TransportError::SpawnFailed {
                path: binary.to_string(),
                source,
            })?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stderr_tail = Arc::new(StdMutex::new(StderrTail::default()));
        capture_stderr(stderr, Arc::clone(&stderr_tail));

        let mut transport = StdioMcpTransport {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: AtomicU64::new(1),
            timeouts,
            failed: None,
            stderr_tail,
        };

        transport.handshake(client_name).await?;
        Ok(transport)
    }

    /// `initialize` → `notifications/initialized`.
    ///
    /// The whole exchange is bounded by [`McpTimeouts::handshake`], because a
    /// server that never answers `initialize` is the most common way this
    /// transport used to hang forever.
    async fn handshake(&mut self, client_name: &str) -> Result<(), TransportError> {
        let params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": client_name,
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        let limit = self.timeouts.handshake;
        let this = &mut *self;
        match tokio::time::timeout(limit, async {
            // The server's reply is checked but its protocol version is not
            // enforced: we send a version we support and accept whatever it
            // offers.
            let _reply = this.request_inner("initialize", params).await?;
            this.notify_inner("notifications/initialized").await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                self.abandon(format!(
                    "the server did not complete the handshake within {limit:?}"
                ));
                Err(TransportError::Timeout {
                    phase: Phase::Handshake,
                    limit,
                })
            }
        }
    }

    /// Give up on this transport: record why, and stop the child.
    ///
    /// Called for every outcome that leaves the stream ambiguous — a timeout, a
    /// closed pipe, unreadable output. Once this has run, the transport refuses
    /// further work, because there is no longer a well-defined place to read
    /// the next reply from.
    fn abandon(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        if self.failed.is_none() {
            self.failed = Some(reason);
        }
        // Non-blocking kill: the child may be wedged, and waiting for it to
        // exit is exactly the wait this transport exists to avoid. `wait` in
        // `shutdown` reaps it when the caller gets there.
        let _ = self.child.start_kill();
    }

    /// True when this transport has poisoned itself and must be re-spawned.
    pub fn is_failed(&self) -> bool {
        self.failed.is_some()
    }

    /// Why the transport gave up, if it did.
    pub fn failure_reason(&self) -> Option<String> {
        self.failed.clone()
    }

    /// The child's most recent stderr output, kept out of the protocol stream.
    ///
    /// Never logged automatically. Bounded to the last
    /// [`STDERR_TAIL_BYTES`]; a server writing more than that loses the
    /// beginning, which is deliberate.
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|guard| guard.text())
            .unwrap_or_default()
    }

    /// How many bytes of stderr are retained.
    pub fn stderr_tail_len(&self) -> usize {
        self.stderr_tail
            .lock()
            .map(|guard| guard.len())
            .unwrap_or(0)
    }

    /// The bounds in force.
    pub fn timeouts(&self) -> McpTimeouts {
        self.timeouts
    }

    /// Refuse to proceed if this transport has already given up.
    fn ensure_usable(&self) -> Result<(), TransportError> {
        match &self.failed {
            Some(reason) => Err(TransportError::Desynchronized {
                reason: reason.clone(),
            }),
            None => Ok(()),
        }
    }

    /// Send a notification and flush.
    async fn notify_inner(&mut self, method: &str) -> Result<(), TransportError> {
        let notification = Notification {
            jsonrpc: "2.0",
            method: method.to_string(),
        };
        let line = serde_json::to_string(&notification).expect("notification serializes");
        self.write_line_inner(&line).await
    }

    /// Send a request and await its reply, unbounded.
    ///
    /// Only ever called inside a [`tokio::time::timeout`]. Kept separate from
    /// the bounded wrapper so the handshake can bound several operations as one
    /// unit while `call_tool` bounds one.
    async fn request_inner(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Value, TransportError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Request {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };
        let line = serde_json::to_string(&request).expect("request serializes");
        self.write_line_inner(&line).await?;

        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).await.map_err(|e| {
                self.abandon(format!("read failed: {e}"));
                TransportError::Malformed(format!("read failed: {e}"))
            })?;
            if n == 0 {
                // EOF. The child is gone; nothing further can be read.
                self.abandon("the server closed the connection");
                return Err(TransportError::Disconnected(
                    "the server closed the connection before answering".into(),
                ));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let reply: Reply = match serde_json::from_str(trimmed) {
                Ok(reply) => reply,
                Err(e) => {
                    self.abandon(format!("unparseable line: {e}"));
                    return Err(TransportError::Malformed(format!("{e}: {trimmed}")));
                }
            };

            // The server emits replies in order; anything carrying our id is
            // the answer. Anything else is a notification we did not ask for
            // and can safely ignore for now.
            if reply.id != Some(id) {
                continue;
            }
            if let Some(err) = reply.error {
                // Not a desync: the server answered the id we asked about, so
                // the stream is still aligned. The code is preserved because it
                // is the only part a caller can branch on.
                return Err(TransportError::Protocol {
                    code: err.code,
                    message: err.message,
                });
            }
            return Ok(reply.result.unwrap_or(Value::Null));
        }
    }

    /// Write one line, bounded by [`McpTimeouts::write`].
    async fn write_line_inner(&mut self, line: &str) -> Result<(), TransportError> {
        let limit = self.timeouts.write;
        let write = async {
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
                .map_err(|e| TransportError::Malformed(format!("flush failed: {e}")))
        };

        match tokio::time::timeout(limit, write).await {
            Ok(result) => {
                if result.is_err() {
                    // A failed write means the child is gone or the pipe is
                    // broken; either way the stream is finished.
                    self.abandon("writing to the server failed");
                }
                result
            }
            Err(_) => {
                // A partial write leaves an unterminated line in the child's
                // input, so everything after it would be unparseable. Give up.
                self.abandon(format!("a write did not finish within {limit:?}"));
                Err(TransportError::Timeout {
                    phase: Phase::Write,
                    limit,
                })
            }
        }
    }

    /// Call an MCP tool by name with the given JSON arguments.
    ///
    /// Returns the tool's textual content on success. When the tool reports
    /// `isError`, this is [`Err(TransportError::ToolFailed)`] — the one place a
    /// substrate failure becomes a Rust error the caller can match on.
    ///
    /// Bounded end to end by [`McpTimeouts::tool_call`]. A server that accepts
    /// the request and never answers produces
    /// [`Err(TransportError::Timeout)`], and the transport is abandoned
    /// afterwards so the undelivered reply cannot be mistaken for a later
    /// call's answer.
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<String, TransportError> {
        self.ensure_usable()?;
        let limit = self.timeouts.tool_call;
        let this = &mut *self;
        let params = serde_json::json!({ "name": name, "arguments": arguments });

        let outcome =
            match tokio::time::timeout(limit, this.request_inner("tools/call", params)).await {
                Ok(result) => result,
                Err(_) => {
                    self.abandon(format!("the server did not answer within {limit:?}"));
                    return Err(TransportError::Timeout {
                        phase: Phase::ToolCall,
                        limit,
                    });
                }
            };

        let result = outcome?;
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

    /// Shut the server down, within [`McpTimeouts::shutdown`].
    ///
    /// Bounded at every step. The child is killed rather than waited on, and
    /// then reaped under a short timeout — a server that ignores both the
    /// notification and the kill cannot hold the caller here, because the
    /// timeout is what ends this method and the `Drop` impl is what handles the
    /// process afterwards.
    pub async fn shutdown(&mut self) {
        let limit = self.timeouts.shutdown;

        let _ = tokio::time::timeout(limit, self.notify_inner("shutdown")).await;
        let _ = tokio::time::timeout(limit, self.stdin.shutdown()).await;
        // `start_kill` sends the signal without waiting; `kill().await` would
        // wait for the process to actually die, which is the unbounded wait.
        let _ = self.child.start_kill();
        // Reap when it happens to happen, so a normal server does not linger as
        // a zombie. A server that will not die is simply left to `Drop`.
        let _ = tokio::time::timeout(limit, self.child.wait()).await;
        self.failed = Some("the transport was shut down".into());
    }
}

impl Drop for StdioMcpTransport {
    fn drop(&mut self) {
        // `start_kill` does not need a runtime, so this still works during a
        // runtime shutdown. It is a signal, not a wait: a process that ignores
        // it is reaped by the OS when its last handle closes, which is the most
        // that can be promised from a destructor.
        let _ = self.child.start_kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_timeouts_are_all_positive() {
        let t = McpTimeouts::default();
        assert!(t.is_usable());
        assert!(t.handshake > Duration::ZERO);
        assert!(t.tool_call > Duration::ZERO);
        assert!(t.write > Duration::ZERO);
        assert!(t.shutdown > Duration::ZERO);
        assert!(
            t.lock > Duration::ZERO,
            "the lock bound is positive by default"
        );
    }

    #[test]
    fn the_fast_timeouts_are_short_enough_to_test_and_still_positive() {
        let t = McpTimeouts::fast();
        assert!(t.is_usable());
        assert!(t.tool_call <= Duration::from_secs(1));
    }

    #[test]
    fn a_zero_bound_makes_the_timeouts_unusable() {
        let zero = McpTimeouts {
            tool_call: Duration::ZERO,
            ..McpTimeouts::default()
        };
        assert!(!zero.is_usable());
    }

    #[test]
    fn every_phase_names_itself_in_the_error_text() {
        // A caller reading only the message has to be able to tell these apart.
        assert!(Phase::Handshake.to_string().contains("handshake"));
        assert!(Phase::Write.to_string().contains("writing"));
        assert!(Phase::ToolCall.to_string().contains("tool call"));
        assert!(Phase::Shutdown.to_string().contains("shutting"));
    }

    #[test]
    fn a_json_rpc_error_keeps_its_code_and_message() {
        let json = r#"{"id":7,"error":{"code":-32601,"message":"Method not found"}}"#;
        let reply: Reply = serde_json::from_str(json).expect("parses");
        let err = reply.error.expect("an error object");
        assert_eq!(err.code, -32601, "the code must survive");
        assert_eq!(err.message, "Method not found");
    }

    #[test]
    fn a_json_rpc_error_without_a_code_is_read_as_zero_rather_than_failing() {
        let json = r#"{"id":7,"error":{"message":"legacy"}}"#;
        let reply: Reply = serde_json::from_str(json).expect("parses");
        assert_eq!(reply.error.expect("an error object").code, 0);
    }

    #[test]
    fn the_stderr_tail_keeps_the_most_recent_bytes() {
        let mut tail = StderrTail::default();
        // Fill it well past the cap in small chunks, the way a server streams.
        for _ in 0..(STDERR_TAIL_BYTES / 1024 + 16) {
            tail.push(&vec![b'x'; 1024]);
        }
        tail.push(b"FINAL");
        let text = tail.text();
        assert!(text.ends_with("FINAL"), "the newest bytes are kept");
        assert!(
            !text.starts_with('x') || text.len() == STDERR_TAIL_BYTES,
            "the tail is bounded"
        );
        assert!(
            tail.len() <= STDERR_TAIL_BYTES,
            "the tail must never exceed its bound, got {}",
            tail.len()
        );
    }

    #[test]
    fn an_unreadable_reply_is_malformed_and_a_timeout_is_not() {
        // The distinction the whole module is about: these must not collapse
        // into one variant, because the caller's response differs.
        let malformed = TransportError::Malformed("expected value at line 1".into());
        let timeout = TransportError::Timeout {
            phase: Phase::ToolCall,
            limit: Duration::from_secs(1),
        };
        assert!(malformed.to_string().contains("malformed"));
        assert!(timeout.to_string().contains("did not finish within"));
        assert!(!timeout.to_string().contains("malformed"));
    }
}
