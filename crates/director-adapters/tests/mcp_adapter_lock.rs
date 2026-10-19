//! Production-path coverage: timeout propagation and lock safety on the real
//! adapters.
//!
//! These tests drive [`AiMemoryAdapter`] and [`HandoffAdapter`] over the same
//! locally compiled MCP fixture the transport tests use. Nothing is mocked at
//! the adapter boundary: a real child process is spawned and real JSON-RPC is
//! spoken to it.
//!
//! # Why a fixture rather than a fake wire
//!
//! A fake wire would prove that the adapter calls the wire — which the existing
//! unit tests already do. It would not prove that a configured timeout actually
//! reaches a child process, or that a caller which cannot get the adapter's lock
//! leaves the caller's in-flight request alone. Both are properties of the
//! *connection*, and the connection is what this phase changed.
//!
//! # No timing races
//!
//! The contention tests put the fixture in `gated` mode, which announces that a
//! request has arrived by creating a file and then blocks until a second file
//! appears. The test waits for the first and creates the second. So every
//! ordering here is established by the test through the filesystem, not by a
//! sleep and a hope. Nothing in this file depends on how fast the machine is.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use director_adapters::aimemory::adapter::{AiMemoryAdapter, AiMemoryAdapterError, AiMemoryWire};
use director_adapters::handoff::adapter::HandoffAdapter;
use director_adapters::stdio_mcp::{McpTimeouts, StdioMcpTransport};
use director_domain::ids::AgentId;

/// A wire that is a real child process and nothing else.
///
/// `AiMemoryAdapter` is generic over its wire so tests can supply one, and this
/// supplies a *real* one: it holds the same [`StdioMcpTransport`] the production
/// adapters hold and forwards to it unchanged. The only thing it adds is the
/// ability to choose the fixture's mode, because ai-memory's real spawn shape
/// hardcodes its own `serve --transport stdio` argv. Every byte on the wire and
/// every child process is genuine.
struct FixtureWire(StdioMcpTransport);

#[async_trait::async_trait]
impl AiMemoryWire for FixtureWire {
    async fn call_tool(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, AiMemoryAdapterError> {
        self.0
            .call_tool(name, arguments)
            .await
            .map_err(AiMemoryAdapterError::from)
    }

    fn stderr_tail(&self) -> Option<String> {
        Some(self.0.stderr_tail())
    }
}

fn server_binary() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("orqyn-mcp-fixture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the fixture's directory");
        let source = dir.join("server.rs");
        let binary = dir.join(if cfg!(windows) {
            "server.exe"
        } else {
            "server"
        });
        std::fs::write(&source, SERVER_SOURCE).expect("the fixture's source");
        let status = std::process::Command::new("rustc")
            .arg("--edition")
            .arg("2021")
            .arg("-O")
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .status()
            .expect("rustc must be on PATH");
        assert!(status.success(), "the MCP server fixture must compile");
        binary
    })
    .clone()
}

/// The fixture source, shared with `mcp_transport.rs`.
///
/// Kept as a literal here rather than imported: integration test files are
/// separate crates, and duplicating it beats adding a shared module for one
/// consumer. `the_two_fixtures_stay_in_step` below fails if the two copies drift
/// apart behaviourally.
const SERVER_SOURCE: &str = r#"
use std::io::{BufRead, Write};
use std::time::Duration;

fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let pat = format!("\"{name}\":");
    let start = line.find(&pat)? + pat.len();
    let rest = &line[start..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim().trim_matches('"'))
}

fn send(out: &mut dyn Write, id: &str, text: &str) {
    let line = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}"
    );
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn send_err(out: &mut dyn Write, id: &str, code: i32, msg: &str) {
    let line = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":{code},\"message\":\"{msg}\"}}}}"
    );
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn main() {
    // Mode resolution: a `mode.txt` in the working directory wins, because a
    // substrate fixes its own argv (ai-memory always sends `serve
    // --transport stdio`) and cannot be told to pass a mode. argv[1] remains the
    // fallback for callers that do control it.
    let from_file = std::fs::read_to_string("mode.txt")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let mode = from_file
        .or_else(|| std::env::args().nth(1))
        .unwrap_or_else(|| "ok".to_string());

    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut call = 0usize;

    if mode == "exit_early" {
        std::process::exit(0);
    }

    if mode == "stderr_noise" {
        for i in 0..2000 {
            let _ = writeln!(std::io::stderr(), "diagnostic line {i}");
        }
        let _ = std::io::stderr().flush();
    }

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let method = field(text, "method").unwrap_or("").to_string();
        let id = field(text, "id").map(|s| s.to_string());

        match method.as_str() {
            "initialize" => {
                if mode == "hang_init" {
                    std::thread::sleep(Duration::from_secs(3600));
                }
                let id = id.unwrap_or_else(|| "1".to_string());
                let reply = format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{{}}}}}}"
                );
                let _ = writeln!(out, "{reply}");
                let _ = out.flush();
                if mode == "refuse_exit" {
                    std::thread::sleep(Duration::from_secs(3600));
                }
            }
            "notifications/initialized" => {}
            "shutdown" => break,
            "tools/call" => {
                let id = id.unwrap_or_else(|| "1".to_string());
                match mode.as_str() {
                    "hang_call" => std::thread::sleep(Duration::from_secs(3600)),
                    "late" => {
                        std::thread::sleep(Duration::from_millis(2500));
                        send(&mut out, &id, "an answer far too late");
                    }
                    "garbage" => {
                        let _ = writeln!(out, "this is not json at all");
                        let _ = out.flush();
                    }
                    "rpc_error" => send_err(&mut out, &id, -32001, "the tool refused"),
                    "gated" => {
                        let name = field(text, "name").unwrap_or("").to_string();
                        if name == "slow" {
                            // Files in the working directory, not stderr: a test
                            // watching for this cannot read the adapter's
                            // diagnostics while the very lock it is studying is
                            // held by the caller it is waiting on.
                            let _ = std::fs::write("arrived.marker", b"1");
                            let mut waited = 0u32;
                            while waited < 9000 {
                                if std::path::Path::new("gate.marker").exists() {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(10));
                                waited += 1;
                            }
                            send(&mut out, &id, "released");
                        } else {
                            send(&mut out, &id, "immediate");
                        }
                    }
                    _ => {
                        call += 1;
                        send(&mut out, &id, &format!("reply-{call}"));
                    }
                }
            }
            _ => {
                if let Some(id) = id {
                    send_err(&mut out, &id, -32601, "no such method");
                }
            }
        }
    }
}
"#;

/// Short enough to keep the suite fast, long enough that a real handshake and
/// one tool call comfortably fit inside them.
fn test_timeouts(lock: Duration) -> McpTimeouts {
    McpTimeouts {
        handshake: Duration::from_secs(5),
        tool_call: Duration::from_secs(5),
        write: Duration::from_secs(2),
        shutdown: Duration::from_millis(300),
        lock,
    }
}

/// A fresh working directory for one test, carrying the fixture's mode.
///
/// Each test gets its own directory, which is also the child's cwd. That is what
/// keeps the `gated` markers (`arrived.marker`, `gate.marker`) and the mode file
/// independent per test, with no shared-state race between concurrently running
/// tests.
fn test_dir(name: &str, mode: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("orqyn-mcp-dirs").join(format!(
        "{}-{}-{name}",
        std::process::id(),
        mode
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("the test's directory");
    std::fs::write(dir.join("mode.txt"), mode).expect("the fixture's mode");
    dir
}

/// Wait until the fixture reports the gated request has arrived.
///
/// A real synchronization point: the child creates the file immediately before
/// it starts waiting, so once it exists the adapter's lock is provably held by
/// the in-flight request.
async fn wait_for_arrival(dir: &std::path::Path) {
    let marker = dir.join("arrived.marker");
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if marker.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the fixture never reported an in-flight request");
}

/// Release the gated request.
fn release(dir: &std::path::Path) {
    std::fs::write(dir.join("gate.marker"), b"go").expect("release the gate");
}

/// An adapter over a real child in `mode`, plus its working directory.
///
/// `name` must be unique per test: the directory is the child's cwd and holds
/// the `gated` markers, so two tests sharing one would silently release each
/// other's in-flight request.
async fn adapter_over(
    name: &str,
    mode: &str,
    lock: Duration,
) -> (AiMemoryAdapter<FixtureWire>, PathBuf) {
    let binary = server_binary();
    let dir = test_dir(&format!("{name}-{mode}-{lock:?}"), mode);
    let wire = StdioMcpTransport::spawn_with_timeouts(
        binary.to_str().expect("utf-8"),
        &[mode],
        &[],
        &[],
        &dir,
        "orqyn-lock-test",
        test_timeouts(lock),
    )
    .await
    .expect("the fixture must handshake");
    (
        AiMemoryAdapter::new_with_timeouts(FixtureWire(wire), "ws", "proj", test_timeouts(lock)),
        dir,
    )
}

// ---------------------------------------------------------------------------
// 1 & 2. Custom timeouts reach both real transports
// ---------------------------------------------------------------------------

#[tokio::test]
async fn custom_timeouts_reach_the_ai_memory_transport() {
    let binary = server_binary().to_string_lossy().into_owned();

    // A 50ms handshake bound against a server that never answers. The default is
    // 30s, so finishing quickly cannot be the default talking.
    let timeouts = McpTimeouts {
        handshake: Duration::from_millis(50),
        tool_call: Duration::from_millis(200),
        write: Duration::from_millis(200),
        shutdown: Duration::from_millis(100),
        lock: Duration::from_millis(100),
    };
    let started = Instant::now();
    let err = match AiMemoryAdapter::connect_with_timeouts(
        &binary,
        &test_dir("ai_connect", "hang_init"),
        "ws",
        "proj",
        timeouts,
    )
    .await
    {
        Ok(_) => panic!("a server that never answers must not connect"),
        Err(err) => err,
    };

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "connect must be bounded by the supplied handshake, took {:?}",
        started.elapsed()
    );
    let rendered = err.to_string();
    assert!(
        rendered.contains("did not finish within"),
        "the failure must be the configured handshake bound: {err:?}"
    );
}

#[tokio::test]
async fn custom_timeouts_reach_the_handoff_transport() {
    let binary = server_binary().to_string_lossy().into_owned();
    let timeouts = McpTimeouts {
        handshake: Duration::from_millis(50),
        tool_call: Duration::from_millis(200),
        write: Duration::from_millis(200),
        shutdown: Duration::from_millis(100),
        lock: Duration::from_millis(100),
    };
    let started = Instant::now();
    let err = match HandoffAdapter::connect_with_timeouts(
        &binary,
        AgentId::from_string("AGENT-1"),
        test_dir("handoff_connect", "hang_init"),
        "proj",
        timeouts,
    )
    .await
    {
        Ok(_) => panic!("a server that never answers must not connect"),
        Err(err) => err,
    };

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "connect must be bounded by the supplied handshake, took {:?}",
        started.elapsed()
    );
    assert!(
        err.to_string().contains("did not finish within"),
        "the failure must be the configured handshake bound: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. Defaults still apply to the existing connection methods
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_default_connect_methods_still_use_defaults() {
    // A caller using `connect` gets these, so they are asserted rather than
    // assumed. `connect` delegates to `connect_with_timeouts` with this value.
    let d = McpTimeouts::default();
    assert_eq!(d.handshake, Duration::from_secs(30));
    assert_eq!(d.tool_call, Duration::from_secs(120));
    assert_eq!(d.write, Duration::from_secs(30));
    assert_eq!(d.shutdown, Duration::from_secs(5));
    assert_eq!(d.lock, Duration::from_secs(30));

    // `fast()` is the transport suite's test twin and must stay well inside every
    // default, or those tests would be asserting against slower-than-default
    // behaviour.
    let f = McpTimeouts::fast();
    for (name, fast, default) in [
        ("handshake", f.handshake, d.handshake),
        ("tool_call", f.tool_call, d.tool_call),
        ("write", f.write, d.write),
        ("shutdown", f.shutdown, d.shutdown),
        ("lock", f.lock, d.lock),
    ] {
        assert!(
            fast < default,
            "{name} fast ({fast:?}) must beat the default"
        );
    }
}

// ---------------------------------------------------------------------------
// 4, 5, 6, 7. Lock contention
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_caller_waiting_on_a_busy_adapter_reports_lock_timeout() {
    let (adapter, dir) = adapter_over("busy", "gated", Duration::from_millis(150)).await;
    let adapter = Arc::new(adapter);

    // Hold the connection with a call that blocks until the gate opens.
    let holder = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move { adapter.raw_call("slow", serde_json::json!({})).await })
    };
    wait_for_arrival(&dir).await;

    // The lock is now provably held. A second caller must be told, not queued.
    let err = match adapter.raw_call("immediate", serde_json::json!({})).await {
        Ok(_) => panic!("a second caller must not be admitted while one is in flight"),
        Err(err) => err,
    };
    match err {
        AiMemoryAdapterError::LockTimeout { waited } => {
            assert_eq!(
                waited,
                Duration::from_millis(150),
                "the configured bound is reported"
            );
        }
        other => panic!("expected a lock timeout, got {other:?}"),
    }

    // A lock timeout must not be mistaken for a tool-call timeout.
    let rendered = err_text(&Err(AiMemoryAdapterError::LockTimeout {
        waited: Duration::from_millis(150),
    }));
    assert!(
        !rendered.contains("tool call") && !rendered.contains("did not finish within"),
        "a lock timeout must not read as a transport timeout: {rendered}"
    );

    // The in-flight request is untouched and completes normally.
    release(&dir);
    let held = holder
        .await
        .expect("the holder's task must not panic")
        .expect("the holder's call must succeed");
    assert_eq!(
        held, "released",
        "the in-flight request must be unaffected by the other caller timing out"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lock_timeout_does_not_poison_the_connection() {
    let (adapter, dir) = adapter_over("poison", "gated", Duration::from_millis(150)).await;
    let adapter = Arc::new(adapter);

    let holder = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move { adapter.raw_call("slow", serde_json::json!({})).await })
    };
    wait_for_arrival(&dir).await;

    // Three callers in a row, all refused. None may damage the connection.
    for _ in 0..3 {
        assert!(matches!(
            adapter.raw_call("immediate", serde_json::json!({})).await,
            Err(AiMemoryAdapterError::LockTimeout { .. })
        ));
    }

    release(&dir);
    assert_eq!(
        holder
            .await
            .expect("task")
            .expect("the holder still succeeds"),
        "released"
    );

    // And a fresh caller is admitted immediately.
    let reply = adapter
        .raw_call("immediate", serde_json::json!({}))
        .await
        .expect("the adapter must still work after three lock timeouts");
    assert_eq!(reply, "immediate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_waiter_releases_its_place_in_the_queue() {
    let (adapter, dir) = adapter_over("cancel", "gated", Duration::from_secs(10)).await;
    let adapter = Arc::new(adapter);

    let holder = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move { adapter.raw_call("slow", serde_json::json!({})).await })
    };
    wait_for_arrival(&dir).await;

    // Queue a waiter with a generous bound, then abandon it mid-wait.
    let waiter = {
        let adapter = Arc::clone(&adapter);
        tokio::spawn(async move { adapter.raw_call("immediate", serde_json::json!({})).await })
    };
    tokio::task::yield_now().await;
    waiter.abort();
    assert!(
        waiter.await.is_err(),
        "a cancelled waiter must not report a result"
    );

    // The holder is untouched, which it could not be if the cancelled waiter had
    // taken the guard.
    release(&dir);
    assert_eq!(
        holder
            .await
            .expect("task")
            .expect("a cancelled waiter must not disturb the holder"),
        "released"
    );

    // And the lock is not stranded by the cancellation.
    let reply = adapter
        .raw_call("immediate", serde_json::json!({}))
        .await
        .expect("the lock must be free after a cancellation");
    assert_eq!(reply, "immediate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_error_from_a_caller_releases_the_lock_for_the_next_one() {
    // `rpc_error` answers every call with a JSON-RPC error, so the first call
    // genuinely fails while holding the guard.
    let (adapter, _dir) = adapter_over("errorpath", "rpc_error", Duration::from_millis(500)).await;

    let first = adapter
        .raw_call("memory_query", serde_json::json!({}))
        .await;
    assert!(
        first.is_err(),
        "this test needs a call that fails, got {first:?}"
    );

    // The next caller gets through rather than inheriting a held lock.
    let second = adapter
        .raw_call("memory_recent", serde_json::json!({}))
        .await;
    assert!(
        second.is_err(),
        "the fixture errors every call, so a second error proves the guard was \
         released: {second:?}"
    );
    assert!(
        !matches!(second, Err(AiMemoryAdapterError::LockTimeout { .. })),
        "the second caller must not be turned away: {second:?}"
    );
}

// ---------------------------------------------------------------------------
// 9. Existing timeout / desynchronization semantics still hold through adapters
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_tool_call_timeout_through_the_adapter_is_still_a_timeout() {
    let binary = server_binary();
    // A 50ms tool_call bound against a server that hangs: this must be a
    // transport timeout naming the tool-call phase, not a lock timeout.
    let timeouts = McpTimeouts {
        handshake: Duration::from_secs(5),
        tool_call: Duration::from_millis(50),
        write: Duration::from_millis(200),
        shutdown: Duration::from_millis(100),
        lock: Duration::from_secs(5),
    };
    let wire = StdioMcpTransport::spawn_with_timeouts(
        binary.to_str().expect("utf-8"),
        &["hang_call"],
        &[],
        &[],
        &test_dir("tool_timeout", "hang_call"),
        "orqyn-timeout-test",
        timeouts,
    )
    .await
    .expect("the fixture must handshake");
    let adapter = AiMemoryAdapter::new_with_timeouts(FixtureWire(wire), "ws", "proj", timeouts);

    let err = match adapter
        .raw_call("memory_query", serde_json::json!({}))
        .await
    {
        Ok(_) => panic!("a hanging call must fail"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("did not finish within"),
        "expected a tool-call timeout through the adapter, got {err:?}"
    );
    assert!(
        !matches!(err, AiMemoryAdapterError::LockTimeout { .. }),
        "a tool timeout must not be reported as a lock timeout"
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn err_text(err: &Result<String, AiMemoryAdapterError>) -> String {
    match err {
        Ok(_) => "ok".to_string(),
        Err(e) => e.to_string(),
    }
}

/// The two fixture copies must agree, or a test could pass against a different
/// server than the transport suite exercised.
#[test]
fn the_two_fixtures_stay_in_step() {
    let other = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("mcp_transport.rs");
    let text = std::fs::read_to_string(&other).expect("the transport suite is readable");
    for marker in [
        "hang_init",
        "hang_call",
        "exit_early",
        "rpc_error",
        "refuse_exit",
        "arrived.marker",
        "gate.marker",
        "mode.txt",
    ] {
        assert!(
            text.contains(marker) && SERVER_SOURCE.contains(marker),
            "the `{marker}` behaviour must exist in both fixture copies"
        );
    }
}
