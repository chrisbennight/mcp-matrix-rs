//! End-to-end engine and HTTP contracts.
//!
//! These tests drive the engine as a caller does and assert that real DDP packets arrive
//! at a socket standing in for the panel, with the pixels that were submitted. That
//! verifies the wiring between layers as well as each layer's isolated behavior.

use matrix_device::WledClient;
use matrix_frame::{Canvas, Frame, FrameSequence, Rate, Rgb};
use matrix_server::state::Engine;
use std::io::{Read, Write};
use std::time::Duration;
use tokio::net::UdpSocket;

const DDP_HEADER_LEN: usize = 10;

/// A loopback HTTP responder standing in for the panel's JSON API.
///
/// Reports a framerate and a power ceiling, which is what the engine needs before it
/// will start playback.
fn fake_panel(info: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind panel");
    let addr = listener.local_addr().expect("addr");

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut raw = vec![0u8; 4096];
            let _ = stream.read(&mut raw);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{info}",
                info.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    format!("http://{addr}")
}

const PANEL_INFO: &str = r#"{
    "ver": "0.16.0",
    "name": "Apollo LED Matrix",
    "leds": {"count": 4096, "fps": 25, "pwr": 900, "maxpwr": 3000, "matrix": {"w": 64, "h": 64}}
}"#;

fn canvas() -> Canvas {
    Canvas::new(64, 64).expect("valid")
}

/// A sequence whose first frame is identifiable per pixel, so what arrives on the wire
/// can be compared against what was submitted rather than merely counted.
fn recognisable_sequence(frames: usize) -> FrameSequence {
    let built: Vec<Frame> = (0..frames)
        .map(|i| {
            let mut frame = Frame::blank(canvas());
            for y in 0..64u16 {
                for x in 0..64u16 {
                    frame.set(
                        x,
                        y,
                        Rgb::new(x as u8, y as u8, (i as u8).wrapping_add(x as u8 ^ y as u8)),
                    );
                }
            }
            frame
        })
        .collect();
    FrameSequence::new(Rate::new(25).expect("valid"), built).expect("uniform")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_played_asset_reaches_the_panel_as_ddp_frames() {
    let panel = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind panel socket");
    let panel_addr = panel.local_addr().expect("panel addr");
    let base = fake_panel(PANEL_INFO);

    let engine = Engine::new(
        canvas(),
        Rate::new(25).expect("valid"),
        WledClient::new(base, Duration::from_secs(2)).expect("valid base"),
        panel_addr,
    );

    let sequence = recognisable_sequence(3);
    let expected = sequence.get(0).expect("first frame").as_rgb().to_vec();
    let asset = engine.store_asset(sequence, 4096, "image/gif".into()).await;

    let playback = engine.play(&asset.handle, true).await.expect("play starts");
    assert!(playback.starts_with("play_"), "playback handle: {playback}");

    // Reassemble one frame by declared offset. UDP does not promise ordering and the
    // offset field is what a receiver keys on.
    let mut reassembled = vec![0u8; expected.len()];
    let mut covered = 0usize;
    let mut buf = vec![0u8; 2048];
    for _ in 0..9 {
        let (n, _) = tokio::time::timeout(Duration::from_secs(10), panel.recv_from(&mut buf))
            .await
            .expect("a frame must arrive within ten seconds")
            .expect("recv");

        let offset = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        let len = u16::from_be_bytes([buf[8], buf[9]]) as usize;
        assert_eq!(
            len,
            n - DDP_HEADER_LEN,
            "declared length matches the datagram"
        );
        reassembled[offset..offset + len].copy_from_slice(&buf[DDP_HEADER_LEN..n]);
        covered += len;
    }

    assert_eq!(
        covered,
        expected.len(),
        "one whole 4096-pixel frame arrived"
    );
    assert_eq!(
        reassembled, expected,
        "the pixels on the wire are the pixels that were submitted"
    );

    let stopped = engine.stop(Some(&playback)).await.expect("stop");
    assert_eq!(stopped, playback);
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_playback_stops_the_frame_stream() {
    let panel = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind panel socket");
    let panel_addr = panel.local_addr().expect("panel addr");
    let base = fake_panel(PANEL_INFO);

    let engine = Engine::new(
        canvas(),
        Rate::new(25).expect("valid"),
        WledClient::new(base, Duration::from_secs(2)).expect("valid base"),
        panel_addr,
    );

    let asset = engine
        .store_asset(recognisable_sequence(50), 4096, "video/mp4".into())
        .await;
    let playback = engine.play(&asset.handle, true).await.expect("play");

    let mut buf = vec![0u8; 2048];
    tokio::time::timeout(Duration::from_secs(10), panel.recv_from(&mut buf))
        .await
        .expect("frames are flowing")
        .expect("recv");

    engine.stop(Some(&playback)).await.expect("stop");

    // Drain whatever was already in flight, then require silence. The panel's own
    // realtime timeout is what returns it to ambient once the stream stops.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while tokio::time::Instant::now() < deadline {
        if tokio::time::timeout(Duration::from_millis(100), panel.recv_from(&mut buf))
            .await
            .is_err()
        {
            break;
        }
    }

    let after_stop =
        tokio::time::timeout(Duration::from_millis(750), panel.recv_from(&mut buf)).await;
    assert!(
        after_stop.is_err(),
        "no frame may reach the panel after playback stops"
    );
    assert!(engine.playing().await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_frame_over_the_power_ceiling_is_scaled_before_it_reaches_the_panel() {
    let panel = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind panel socket");
    let panel_addr = panel.local_addr().expect("panel addr");

    // A ceiling well below the panel's rated full-white draw, so the clamp must engage.
    const DERATED: &str = r#"{
        "ver": "0.16.0",
        "name": "Apollo LED Matrix",
        "leds": {"count": 4096, "fps": 25, "pwr": 100, "maxpwr": 800, "matrix": {"w": 64, "h": 64}}
    }"#;
    let base = fake_panel(DERATED);

    let engine = Engine::new(
        canvas(),
        Rate::new(25).expect("valid"),
        WledClient::new(base, Duration::from_secs(2)).expect("valid base"),
        panel_addr,
    );

    let mut white = Frame::blank(canvas());
    white.fill(Rgb::new(255, 255, 255));
    let sequence = FrameSequence::new(Rate::new(25).expect("valid"), vec![white]).expect("uniform");
    let asset = engine.store_asset(sequence, 16, "image/png".into()).await;

    engine.play(&asset.handle, true).await.expect("play");

    let mut buf = vec![0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_secs(10), panel.recv_from(&mut buf))
        .await
        .expect("a frame must arrive")
        .expect("recv");

    let payload = &buf[DDP_HEADER_LEN..n];
    assert!(
        payload.iter().all(|&channel| channel < 255),
        "full white must reach the panel scaled down, not at full brightness"
    );
    assert!(
        payload.iter().any(|&channel| channel > 0),
        "the frame must not be scaled to black"
    );
}

// Exercise the same path through the HTTP router: server/discover, tools/list, and
// tools/call, with stand-in decoder binaries so the media path needs no FFmpeg install.

use base64::Engine as _;
use matrix_server::mcp::MediaBinaries;

/// Stand-in ffprobe and ffmpeg written to a temp dir.
///
/// The probe answers the two questions the real one is asked. The decoder ignores its
/// argv, drains nothing, and emits three whole black 64x64 RGB24 frames using only
/// shell builtins — the environment it runs in is deliberately stripped, so external
/// commands cannot be assumed present.
fn stand_in_binaries(dir: &std::path::Path) -> MediaBinaries {
    use std::os::unix::fs::PermissionsExt;

    let probe = dir.join("probe.sh");
    std::fs::write(&probe, "#!/bin/sh\nprintf '64x64\\n0.12\\n'\n").expect("write probe");
    let decoder = dir.join("decode.sh");
    std::fs::write(
        &decoder,
        "#!/bin/sh\ni=0\nwhile [ $i -lt 36864 ]; do printf '\\000'; i=$((i+1)); done\n",
    )
    .expect("write decoder");
    for path in [&probe, &decoder] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    MediaBinaries {
        ffmpeg: decoder.display().to_string(),
        ffprobe: probe.display().to_string(),
    }
}

struct WireServer {
    base: String,
    client: reqwest::Client,
}

impl WireServer {
    async fn start(engine: std::sync::Arc<Engine>, binaries: MediaBinaries) -> Self {
        Self::start_with_allowed_hosts(
            engine,
            binaries,
            vec!["localhost".into(), "127.0.0.1".into(), "::1".into()],
        )
        .await
    }

    async fn start_with_allowed_hosts(
        engine: std::sync::Arc<Engine>,
        binaries: MediaBinaries,
        allowed_hosts: Vec<String>,
    ) -> Self {
        let app = matrix_server::router(engine, binaries, allowed_hosts);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base: format!("http://{addr}"),
            client: reqwest::Client::new(),
        }
    }

    /// One request under the protocol's rules: `_meta` carries the protocol version
    /// and client capabilities on every call, and `Mcp-Name` names the tool for
    /// `tools/call` so an intermediary can route without parsing the body.
    async fn rpc(
        &self,
        method: &str,
        name: Option<&str>,
        mut params: serde_json::Value,
    ) -> serde_json::Value {
        params["_meta"] = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let response = self
            .client
            .post(format!("{}/mcp", self.base))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", method)
            .header("Mcp-Name", name.unwrap_or(method))
            .json(&body)
            .send()
            .await
            .expect("request");
        response.json().await.expect("json body")
    }

    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        self.rpc(
            "tools/call",
            Some(name),
            serde_json::json!({
                "name": name,
                "arguments": arguments,
            }),
        )
        .await
    }
}

/// Text payload of a tool result, parsed as the JSON the tools emit.
fn tool_payload(response: &serde_json::Value) -> serde_json::Value {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool result had no text content: {response}"));
    serde_json::from_str(text).expect("tool payload is JSON")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_wire_carries_a_submission_to_playback_and_back_to_silence() {
    let panel = UdpSocket::bind("127.0.0.1:0").await.expect("bind panel");
    let panel_addr = panel.local_addr().expect("panel addr");
    let base = fake_panel(PANEL_INFO);
    let scratch = std::env::temp_dir().join(format!("matrix-wire-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch dir");

    let engine = Engine::new(
        canvas(),
        Rate::new(25).expect("valid"),
        WledClient::new(base, Duration::from_secs(2)).expect("valid base"),
        panel_addr,
    );
    let server = WireServer::start(engine.clone(), stand_in_binaries(&scratch)).await;

    // Discovery reports the protocol version.
    let discover = server
        .rpc("server/discover", None, serde_json::json!({}))
        .await;
    assert!(
        discover["result"].is_object(),
        "server/discover must answer: {discover}"
    );

    // The tool catalog is sorted and carries its freshness hints.
    let listed = server.rpc("tools/list", None, serde_json::json!({})).await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "catalog must be sorted");
    assert!(names.contains(&"matrix_submit_asset"));
    assert!(listed["result"]["ttlMs"].as_u64().unwrap_or(0) > 0);
    assert_eq!(listed["result"]["cacheScope"], "private");

    // A small payload passes through resolution, decode, and assembly.
    let payload = base64::engine::general_purpose::STANDARD.encode(b"stand-in source");
    let submitted = server
        .call_tool(
            "matrix_submit_asset",
            serde_json::json!({
                "source": { "uri": format!("data:image/gif;base64,{payload}") }
            }),
        )
        .await;
    let asset = tool_payload(&submitted);
    assert_eq!(asset["frames"], 3, "three stand-in frames: {asset}");
    let handle = asset["handle"].as_str().expect("asset handle");

    // Playback sends frames to the panel socket.
    let played = server
        .call_tool(
            "matrix_play",
            serde_json::json!({ "asset": handle, "looping": true }),
        )
        .await;
    let playback = tool_payload(&played)["playback"]
        .as_str()
        .expect("playback handle")
        .to_string();

    let mut buf = vec![0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_secs(10), panel.recv_from(&mut buf))
        .await
        .expect("frames are flowing")
        .expect("recv");
    assert!(
        buf[DDP_HEADER_LEN..n].iter().all(|&b| b == 0),
        "the wire must carry the stand-in decoder's pixels, not something else"
    );

    // Stopping by handle ends the stream.
    let stopped = server
        .call_tool("matrix_stop", serde_json::json!({ "playback": playback }))
        .await;
    assert_eq!(
        tool_payload(&stopped)["stopped"].as_str(),
        Some(playback.as_str())
    );

    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while tokio::time::Instant::now() < deadline {
        if tokio::time::timeout(Duration::from_millis(100), panel.recv_from(&mut buf))
            .await
            .is_err()
        {
            break;
        }
    }
    let after_stop =
        tokio::time::timeout(Duration::from_millis(750), panel.recv_from(&mut buf)).await;
    assert!(after_stop.is_err(), "no frame after a wire-level stop");

    let _ = std::fs::remove_dir_all(&scratch);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_domain_refusal_crosses_the_wire_with_its_code_intact() {
    let base = fake_panel(PANEL_INFO);
    let engine = Engine::new(
        canvas(),
        Rate::new(25).expect("valid"),
        WledClient::new(base, Duration::from_secs(2)).expect("valid base"),
        "127.0.0.1:4048".parse().expect("addr"),
    );
    let server = WireServer::start(
        engine,
        MediaBinaries {
            ffmpeg: "unused".into(),
            ffprobe: "unused".into(),
        },
    )
    .await;

    let response = server
        .call_tool(
            "matrix_play",
            serde_json::json!({ "asset": "asset_deadbeef_1" }),
        )
        .await;

    // The stable machine contract is the string code in data; the numeric code is
    // implementation-defined rather than the reserved invalid-params code.
    let error = &response["error"];
    assert_eq!(
        error["code"], -32050,
        "implementation-defined code: {response}"
    );
    assert_eq!(error["data"]["code"], "matrix_unknown_asset");
}

#[tokio::test(flavor = "multi_thread")]
async fn text_shown_over_the_wire_scrolls_onto_the_panel() {
    let panel = UdpSocket::bind("127.0.0.1:0").await.expect("bind panel");
    let panel_addr = panel.local_addr().expect("panel addr");
    let base = fake_panel(PANEL_INFO);

    let engine = Engine::new(
        canvas(),
        Rate::new(25).expect("valid"),
        WledClient::new(base, Duration::from_secs(2)).expect("valid base"),
        panel_addr,
    );
    let server = WireServer::start(
        engine,
        MediaBinaries {
            ffmpeg: "unused".into(),
            ffprobe: "unused".into(),
        },
    )
    .await;

    // Long enough to scroll, so playback loops until stopped.
    let shown = server
        .call_tool(
            "matrix_show_text",
            serde_json::json!({ "text": "HELLO FROM THE WIRE" }),
        )
        .await;
    let payload = tool_payload(&shown);
    let frames = payload["asset"]["frames"].as_u64().expect("frame count");
    assert!(frames > 1, "a marquee is many frames: {payload}");
    let playback = payload["playback"]
        .as_str()
        .expect("played by default")
        .to_string();

    // The marquee's pixels must reach the panel. Not every frame lights pixels — the
    // first few steps are still off-edge — so drain until a lit one arrives.
    let mut buf = vec![0u8; 2048];
    let mut lit = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let Ok(received) =
            tokio::time::timeout(Duration::from_secs(5), panel.recv_from(&mut buf)).await
        else {
            break;
        };
        let (n, _) = received.expect("recv");
        if buf[DDP_HEADER_LEN..n].iter().any(|&b| b > 0) {
            lit = true;
            break;
        }
    }
    assert!(lit, "the scrolling text must light pixels on the wire");

    let stopped = server
        .call_tool("matrix_stop", serde_json::json!({ "playback": playback }))
        .await;
    assert_eq!(
        tool_payload(&stopped)["stopped"].as_str(),
        Some(playback.as_str())
    );

    // Stopping must end the stream, not merely answer the call.
    let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while tokio::time::Instant::now() < drain_deadline {
        if tokio::time::timeout(Duration::from_millis(100), panel.recv_from(&mut buf))
            .await
            .is_err()
        {
            break;
        }
    }
    let after_stop =
        tokio::time::timeout(Duration::from_millis(750), panel.recv_from(&mut buf)).await;
    assert!(after_stop.is_err(), "no frame after stopping the marquee");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_host_allowlist_admits_the_dialed_authority_and_refuses_others() {
    let base = fake_panel(PANEL_INFO);
    let engine = Engine::new(
        canvas(),
        Rate::new(25).expect("valid"),
        WledClient::new(base, Duration::from_secs(2)).expect("valid base"),
        "127.0.0.1:4048".parse().expect("addr"),
    );
    let server = WireServer::start_with_allowed_hosts(
        engine,
        MediaBinaries {
            ffmpeg: "unused".into(),
            ffprobe: "unused".into(),
        },
        vec!["matrix-mcp:8080".into()],
    )
    .await;

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {"_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
        }},
    });
    let request = |host: &'static str| {
        let client = server.client.clone();
        let url = format!("{}/mcp", server.base);
        let body = body.clone();
        async move {
            client
                .post(url)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream")
                .header("MCP-Protocol-Version", "2026-07-28")
                .header("Mcp-Method", "tools/list")
                .header("Mcp-Name", "tools/list")
                .header("Host", host)
                .json(&body)
                .send()
                .await
                .expect("request")
        }
    };

    let allowed = request("matrix-mcp:8080").await;
    assert!(
        allowed.status().is_success(),
        "allowlisted authority must be served: {}",
        allowed.status()
    );

    let refused = request("evil.example:8080").await;
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::FORBIDDEN,
        "an authority outside the allowlist is refused"
    );
}
