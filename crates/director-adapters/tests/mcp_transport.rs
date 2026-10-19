//! The shared MCP stdio transport, against a real child process.
//!
//! Every case here uses a locally compiled server fixture rather than ai-memory
//! or handoff-mcp, because the point is to reproduce *specific* misbehaviour —
//! a server that never answers, one that exits mid-call, one that replies with
//! something unreadable — and a real substrate cannot be made to do those on
//! demand. The live suites still cover the real servers; these cover the
//! failure modes the live suites cannot reach.
//!
//! Timeouts are [`McpTimeouts::fast`] throughout: sub-second bounds, so the
//! suite finishes in seconds and nothing here sleeps for minutes.
//!
//! What is real: a spawned child process, a real pipe, real line framing, real
//! kill signals. What is scripted: what the server chooses to do.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use director_adapters::stdio_mcp::{McpTimeouts, Phase, StdioMcpTransport, TransportError};

/// Compile the server fixture once per test run.
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
            .expect("rustc must be on PATH; it is present whenever cargo is");
        assert!(status.success(), "the MCP server fixture must compile");
        binary
    })
    .clone()
}

/// Hand-rolled JSON, because the fixture is compiled by bare `rustc` with no
/// crates available. It only needs to read two scalar fields and write fixed
/// shapes, which is well inside what string handling can do honestly.
const SERVER_SOURCE: &str = r#"
use std::io::{BufRead, Write};
use std::time::Duration;

/// Read one scalar field out of a flat JSON line.
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
    // --transport stdio`) and cannot be told to pass a mode. argv[1] remains as
    // the fallback for callers that control it.
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

    // Exits before reading anything at all.
    if mode == "exit_early" {
        std::process::exit(0);
    }

    // Writes a lot of diagnostics before doing anything useful, so the test can
    // prove stderr never reaches the protocol stream.
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
                    // Never reads again and never exits on its own: shutdown has
                    // to bound itself rather than wait for this.
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
                    // Answers a named tool immediately and a different one only
                    // after an externally-controlled release. That is what lets
                    // a test hold a request in flight, prove a second caller is
                    // refused, and then let the first succeed on demand instead
                    // of sleeping and hoping.
                    "gated" => {
                        let name = field(text, "name").unwrap_or("").to_string();
                        if name == "slow" {
                            // Files in the working directory, not stderr: a test
                            // watching this cannot read the adapter's
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

fn scratch() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("orqyn-mcp-cwd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Spawn the fixture in `mode`, ready for calls.
async fn connect(mode: &str, timeouts: McpTimeouts) -> Result<StdioMcpTransport, TransportError> {
    let binary = server_binary();
    StdioMcpTransport::spawn_with_timeouts(
        binary.to_str().expect("utf-8 path"),
        &[mode],
        &[],
        &[],
        &scratch(),
        "orqyn-transport-test",
        timeouts,
    )
    .await
}

/// Spawn the fixture expecting success.
async fn connected(mode: &str) -> StdioMcpTransport {
    connect(mode, McpTimeouts::fast())
        .await
        .unwrap_or_else(|err| panic!("the fixture must handshake in mode {mode}: {err}"))
}

// ---------------------------------------------------------------------------
// 1. A server that never answers `initialize`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_server_that_never_answers_initialize_times_out() {
    let started = Instant::now();
    let err = connect("hang_init", McpTimeouts::fast())
        .await
        .expect_err("a server that never answers must not succeed");

    match err {
        TransportError::Timeout { phase, .. } => assert_eq!(
            phase,
            Phase::Handshake,
            "a silent server is a handshake timeout, not a tool-call one"
        ),
        other => panic!("expected a handshake timeout, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the bound must be what ended the wait, not the server"
    );
}

// ---------------------------------------------------------------------------
// 2. A server that handshakes then hangs on a tool call
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_server_that_hangs_on_a_tool_call_times_out_rather_than_blocking() {
    let mut transport = connected("hang_call").await;
    let started = Instant::now();

    let err = transport
        .call_tool("memory_query", serde_json::json!({ "query": "x" }))
        .await
        .expect_err("a hanging call must not succeed");

    match err {
        TransportError::Timeout { phase, .. } => assert_eq!(phase, Phase::ToolCall),
        other => panic!("expected a tool-call timeout, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the call must be bounded by its own timeout, took {:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// 3. A server that exits before responding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_server_that_exits_before_responding_is_reported_as_disconnected() {
    let err = connect("exit_early", McpTimeouts::fast())
        .await
        .expect_err("a server that exits immediately must not hand us a transport");
    assert!(
        matches!(err, TransportError::Disconnected(_)),
        "a closed pipe is a disconnection, not a parse failure: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. A malformed response
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_malformed_response_is_malformed_and_not_reported_as_a_timeout() {
    let mut transport = connected("garbage").await;
    let err = transport
        .call_tool("memory_query", serde_json::json!({}))
        .await
        .expect_err("garbage is not an answer");

    assert!(
        matches!(err, TransportError::Malformed(_)),
        "unreadable output must be Malformed: {err:?}"
    );
    assert!(
        !err.to_string().contains("did not finish within"),
        "a parse failure must never be dressed as a timeout: {err}"
    );
}

// ---------------------------------------------------------------------------
// 5. A JSON-RPC error object
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_json_rpc_error_preserves_its_code_and_message() {
    let mut transport = connected("rpc_error").await;
    let err = transport
        .call_tool("memory_write_page", serde_json::json!({}))
        .await
        .expect_err("the server refused");

    match err {
        TransportError::Protocol { code, message } => {
            assert_eq!(code, -32001, "the numeric code must survive");
            assert_eq!(message, "the tool refused");
        }
        other => panic!("expected a protocol error carrying the code, got {other:?}"),
    }
    // A protocol error is the server answering correctly, so the transport is
    // still usable — which is why it must not poison itself. The fixture refuses
    // every call, so the second one fails the same way rather than differently.
    assert!(
        !transport.is_failed(),
        "an error object is a reply, not a desynchronization"
    );
    match transport
        .call_tool("memory_query", serde_json::json!({}))
        .await
    {
        Err(TransportError::Protocol { code, .. }) => {
            assert_eq!(
                code, -32001,
                "the transport keeps answering protocol errors"
            )
        }
        other => panic!(
            "expected the transport to stay usable and answer again, got {other:?} \
             — an error object must not poison it"
        ),
    }
}

// ---------------------------------------------------------------------------
// 6 & 7. Late responses and reuse after a failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_late_reply_after_a_timeout_cannot_be_taken_for_a_later_calls_answer() {
    let mut transport = connected("late").await;

    // The fixture replies 2.5s from now; the bound is 600ms.
    let err = transport
        .call_tool("memory_query", serde_json::json!({}))
        .await
        .expect_err("the answer arrives after the bound");
    assert!(matches!(err, TransportError::Timeout { .. }), "{err:?}");
    assert!(
        transport.is_failed(),
        "a timed-out transport must refuse further work rather than guess where \
         the stream is"
    );

    // The real question: does the *next* call pick up the undelivered reply? It
    // must not even reach the wire, let alone read it.
    let started = Instant::now();
    let second = transport
        .call_tool("memory_recent", serde_json::json!({}))
        .await
        .expect_err("a failed transport must refuse");
    match second {
        TransportError::Desynchronized { .. } => {}
        other => panic!(
            "expected the transport to refuse as desynchronized, got {other:?} \
             — a late reply may have been accepted"
        ),
    }
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the refusal must be immediate, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_write_failure_also_refuses_further_work() {
    let mut transport = connected("garbage").await;
    let _ = transport
        .call_tool("memory_query", serde_json::json!({}))
        .await;
    assert!(
        transport.is_failed(),
        "unreadable output means the stream is no longer trustworthy"
    );
    assert!(matches!(
        transport
            .call_tool("memory_recent", serde_json::json!({}))
            .await,
        Err(TransportError::Desynchronized { .. })
    ));
}

// ---------------------------------------------------------------------------
// 8. Shutdown of a child that will not exit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_of_a_child_that_will_not_exit_still_returns_promptly() {
    let mut transport = connected("refuse_exit").await;
    let started = Instant::now();
    // The fixture never reads again and never exits on its own. `shutdown`
    // must bound itself rather than wait for it.
    transport.shutdown().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "shutdown must be bounded, took {elapsed:?}"
    );
    assert!(transport.is_failed(), "a shut-down transport is not usable");
}

// ---------------------------------------------------------------------------
// 9. Successful initialization and sequential calls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sequential_calls_each_receive_their_own_reply() {
    let mut transport = connected("ok").await;
    for expected in 1..=4 {
        let reply = transport
            .call_tool("memory_recent", serde_json::json!({ "limit": 1 }))
            .await
            .unwrap_or_else(|err| panic!("call {expected} must succeed: {err}"));
        assert_eq!(
            reply,
            format!("reply-{expected}"),
            "each reply must be matched to its own request, in order"
        );
    }
    assert!(!transport.is_failed());
}

#[tokio::test]
async fn a_zero_timeout_is_refused_rather_than_refusing_everything_later() {
    let err = connect(
        "ok",
        McpTimeouts {
            tool_call: Duration::ZERO,
            ..McpTimeouts::fast()
        },
    )
    .await
    .expect_err("a zero bound is unusable");
    assert!(
        matches!(err, TransportError::InvalidTimeouts(_)),
        "expected the configuration to be refused up front: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 10. stdout is protocol-only; stderr is captured separately
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_server_that_writes_to_stderr_still_answers_cleanly() {
    let mut transport = connected("stderr_noise").await;

    let reply = transport
        .call_tool("memory_recent", serde_json::json!({}))
        .await
        .expect("stderr must not disturb the protocol stream");
    assert_eq!(reply, "reply-1");

    // The diagnostics are retained, and retrievable, and were never mixed in.
    let tail = transport.stderr_tail();
    assert!(
        tail.contains("diagnostic line"),
        "the child's stderr must be captured: {tail:?}"
    );
    assert!(
        !reply.contains("diagnostic"),
        "stderr must never appear in a protocol result"
    );
    assert!(
        transport.stderr_tail_len() > 0,
        "the tail reports what it retained"
    );
}

#[tokio::test]
async fn the_stderr_tail_is_bounded_even_when_a_server_floods_it() {
    // The fixture writes ~2000 lines before answering; the cap is 64 KiB, and
    // the tail must not grow past it however loud the child is.
    let transport = connected("stderr_noise").await;
    assert!(
        transport.stderr_tail_len() <= 64 * 1024,
        "the tail is bounded, got {}",
        transport.stderr_tail_len()
    );
}
