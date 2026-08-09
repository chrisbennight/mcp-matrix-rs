//! Wire-level contract of the stdio transport.
//!
//! Spawns the real binary with `--stdio`, performs the MCP handshake over its
//! stdin/stdout, and asserts the advertised tool catalog matches the release catalog in
//! `smoke/expected-tools.txt` — the same surface the HTTP transport advertises. The
//! panel is a loopback fake; nothing outside the process boundary is contacted.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A loopback HTTP responder standing in for the panel's JSON API, for the device
/// poller the binary always runs.
fn fake_panel() -> String {
    const PANEL_INFO: &str = r#"{
        "ver": "0.16.0",
        "name": "Apollo LED Matrix",
        "leds": {"count": 4096, "fps": 25, "pwr": 900, "maxpwr": 3000, "matrix": {"w": 64, "h": 64}}
    }"#;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind panel");
    let addr = listener.local_addr().expect("addr");

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut raw = vec![0u8; 4096];
            let _ = stream.read(&mut raw);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PANEL_INFO}",
                PANEL_INFO.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    format!("http://{addr}")
}

/// Receive newline-delimited JSON-RPC messages until one carries the wanted `id`.
fn response_with_id(
    lines: &mpsc::Receiver<String>,
    id: u64,
    deadline: Duration,
) -> serde_json::Value {
    let start = Instant::now();
    while start.elapsed() < deadline {
        let remaining = deadline.saturating_sub(start.elapsed());
        let line = lines
            .recv_timeout(remaining)
            .expect("server stdout closed or timed out before responding");
        let message: serde_json::Value = serde_json::from_str(&line).expect("valid JSON per line");
        if message.get("id").and_then(|v| v.as_u64()) == Some(id) {
            return message;
        }
    }
    panic!("no response with id {id} within {deadline:?}");
}

/// Wait for exit, killing the child if it outlives the deadline.
fn reap(mut child: Child, deadline: Duration) {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if child.try_wait().expect("wait").is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    panic!("server did not exit after stdin closed");
}

#[test]
fn stdio_serves_the_release_tool_catalog() {
    let wled_url = fake_panel();

    let mut child = Command::new(env!("CARGO_BIN_EXE_matrix-server"))
        .args([
            "--stdio",
            "--wled-url",
            &wled_url,
            "--ddp-addr",
            "127.0.0.1:4048",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn matrix-server --stdio");

    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });

    let mut stdin = child.stdin.take().expect("piped stdin");
    let handshake = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"stdio-test","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    ];
    for message in handshake {
        writeln!(stdin, "{message}").expect("write request");
    }
    stdin.flush().expect("flush requests");

    let timeout = Duration::from_secs(30);
    let initialize = response_with_id(&lines, 1, timeout);
    assert_eq!(
        initialize["result"]["protocolVersion"], "2026-07-28",
        "unexpected initialize response: {initialize}"
    );

    let tool_list = response_with_id(&lines, 2, timeout);
    let mut advertised: Vec<String> = tool_list["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("unexpected tools/list response: {tool_list}"))
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name").to_owned())
        .collect();
    advertised.sort();

    let expected: Vec<String> = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../smoke/expected-tools.txt"
    ))
    .expect("read release catalog")
    .lines()
    .map(str::to_owned)
    .collect();

    assert_eq!(advertised, expected);

    // Closing stdin ends the session; the client-owned process lifetime is part of the
    // stdio contract.
    drop(stdin);
    reap(child, Duration::from_secs(10));
}
