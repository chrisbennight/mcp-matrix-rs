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

    async fn start_with_files(
        engine: std::sync::Arc<Engine>,
        binaries: MediaBinaries,
        files: std::sync::Arc<matrix_server::files::FilePlane>,
    ) -> Self {
        Self::assemble(
            engine,
            binaries,
            vec!["localhost".into(), "127.0.0.1".into(), "::1".into()],
            Some(files),
        )
        .await
    }

    async fn start_with_allowed_hosts(
        engine: std::sync::Arc<Engine>,
        binaries: MediaBinaries,
        allowed_hosts: Vec<String>,
    ) -> Self {
        Self::assemble(engine, binaries, allowed_hosts, None).await
    }

    async fn assemble(
        engine: std::sync::Arc<Engine>,
        binaries: MediaBinaries,
        allowed_hosts: Vec<String>,
        files: Option<std::sync::Arc<matrix_server::files::FilePlane>>,
    ) -> Self {
        let app = matrix_server::router(engine, binaries, allowed_hosts, files);
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

/// A transfer plane staging into a scratch directory this test owns.
async fn wire_plane(name: &str) -> std::sync::Arc<matrix_server::files::FilePlane> {
    matrix_server::files::FilePlane::new(matrix_server::files::FileConfig {
        // The origin an intermediary would dial. The test dials the real listener
        // instead, because a loopback test server has no certificate for this name —
        // what is under test is the plumbing, not TLS termination, which is the
        // operator's boundary.
        public_origin: "https://panel.example".into(),
        staging_dir: std::env::temp_dir()
            .join(format!("matrix-wire-files-{}-{name}", std::process::id())),
        ttl: std::time::Duration::from_secs(60),
        max_staged: 4,
        max_source_bytes: 64 * 1024 * 1024,
    })
    .await
    .expect("plane")
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
async fn a_text_layout_with_fixed_and_scrolling_regions_plays_over_the_wire() {
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

    // A chyron: a fixed headline on top, a ticker crossing the bottom rows.
    let shown = server
        .call_tool(
            "matrix_show_text_layout",
            serde_json::json!({
                "regions": [
                    {
                        "rect": { "x": 0, "y": 0, "width": 64, "height": 16 },
                        "text": "SHIP",
                        "behavior": { "type": "fixed", "align": "center" }
                    },
                    {
                        "rect": { "x": 0, "y": 52, "width": 64, "height": 12 },
                        "text": "BUILD GREEN | TESTS PASS",
                        "style": { "scale": 1 },
                        "behavior": {
                            "type": "scroll",
                            "path": "left_to_right",
                            "direction": "reverse",
                            "speed_px_s": 25
                        }
                    }
                ]
            }),
        )
        .await;
    let payload = tool_payload(&shown);
    let frames = payload["asset"]["frames"].as_u64().expect("frame count");
    assert!(frames > 1, "a package with a scroller animates: {payload}");
    assert_eq!(payload["regions"], 2);
    let playback = payload["playback"]
        .as_str()
        .expect("played by default")
        .to_string();

    // The fixed headline is lit in every frame and each region's pixels stay inside
    // its own rectangle, so any complete frame on the wire must carry light in the
    // headline's rows and nowhere outside the two regions. Reassemble one frame by
    // declared offset, anchored on an offset-zero packet and closed by the sender's
    // push flag, so a dropped datagram costs one frame rather than splicing two.
    let frame_bytes = canvas().byte_len();
    let mut reassembled = vec![0u8; frame_bytes];
    let mut anchored = false;
    let mut covered = 0usize;
    let mut buf = vec![0u8; 2048];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "a complete frame must arrive within ten seconds"
        );
        let (n, _) = tokio::time::timeout(Duration::from_secs(5), panel.recv_from(&mut buf))
            .await
            .expect("frames are flowing")
            .expect("recv");
        let offset = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        let len = u16::from_be_bytes([buf[8], buf[9]]) as usize;
        assert_eq!(
            len,
            n - DDP_HEADER_LEN,
            "declared length matches the datagram"
        );
        assert!(
            offset + len <= frame_bytes,
            "a declared span stays inside one frame"
        );
        if offset == 0 {
            anchored = true;
            covered = 0;
        } else if !anchored {
            continue;
        }
        reassembled[offset..offset + len].copy_from_slice(&buf[DDP_HEADER_LEN..n]);
        covered += len;
        // The sender marks a frame's final packet with the DDP push flag.
        if buf[0] & 0x01 != 0 {
            if covered == frame_bytes {
                break;
            }
            // A datagram went missing mid-frame; re-anchor on the next frame.
            anchored = false;
        }
    }

    let lit_rows: Vec<usize> = reassembled
        .chunks(3)
        .enumerate()
        .filter(|(_, px)| px.iter().any(|&b| b > 0))
        .map(|(i, _)| i / 64)
        .collect();
    assert!(
        lit_rows.iter().any(|&y| y < 16),
        "the fixed headline is lit in every frame"
    );
    assert!(
        lit_rows.iter().all(|&y| y < 16 || (52..64).contains(&y)),
        "light stays inside the two regions' rows: {lit_rows:?}"
    );

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
    assert!(after_stop.is_err(), "no frame after stopping the layout");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_layout_refusal_crosses_the_wire_with_its_code_intact() {
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
            "matrix_show_text_layout",
            serde_json::json!({
                "regions": [
                    {
                        "rect": { "x": 0, "y": 0, "width": 32, "height": 16 },
                        "text": "A",
                        "behavior": { "type": "fixed" }
                    },
                    {
                        "rect": { "x": 16, "y": 8, "width": 32, "height": 16 },
                        "text": "B",
                        "behavior": { "type": "fixed" }
                    }
                ]
            }),
        )
        .await;

    let error = &response["error"];
    assert_eq!(
        error["code"], -32050,
        "implementation-defined code: {response}"
    );
    assert_eq!(error["data"]["code"], "matrix_layout_overlap");
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

#[tokio::test(flavor = "multi_thread")]
async fn media_past_the_inline_cap_reaches_the_panel_by_reference() {
    use sha2::Digest as _;

    let panel = UdpSocket::bind("127.0.0.1:0").await.expect("bind panel");
    let panel_addr = panel.local_addr().expect("panel addr");
    let base = fake_panel(PANEL_INFO);
    let scratch = std::env::temp_dir().join(format!("matrix-ref-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch");
    let binaries = stand_in_binaries(&scratch);

    let engine = Engine::new(
        canvas(),
        Rate::new(25).expect("rate"),
        matrix_device::WledClient::new(base, Duration::from_millis(500)).expect("client"),
        panel_addr,
    );
    let plane = wire_plane("roundtrip").await;
    let server = WireServer::start_with_files(engine, binaries, plane).await;

    // Four times the inline cap: this payload cannot be submitted as a tool argument at
    // all, which is the whole reason the transfer plane exists.
    let payload = vec![0xA5u8; matrix_server::tools::MAX_INLINE_BYTES * 4];
    let digest =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(&payload));

    let authorized = server
        .rpc(
            "files/authorizeUpload",
            None,
            // Deliberately declares no size. The report's byte count can then only have
            // come from counting what arrived, not from echoing a declaration back.
            serde_json::json!({
                "name": "clip.gif",
                "mimeType": "image/gif",
                "digest": { "algorithm": "sha-256", "value": digest },
            }),
        )
        .await;
    let result = &authorized["result"];
    assert_eq!(result["upload"]["transport"], "https", "{authorized}");
    assert_eq!(result["upload"]["method"], "PUT");

    let uri = result["file"]["uri"]
        .as_str()
        .expect("staged uri")
        .to_string();
    let credential = result["upload"]["headers"][matrix_server::files::TRANSFER_CREDENTIAL_HEADER]
        .as_str()
        .expect("credential")
        .to_string();
    assert!(
        !result["upload"]["url"]
            .as_str()
            .expect("url")
            .contains(&credential),
        "the credential must never appear in the descriptor URL"
    );
    assert!(
        result["upload"]["headers"]["Authorization"].is_null(),
        "Authorization stays free for whatever boundary fronts this route"
    );

    // The intermediary streams the bytes to the descriptor. It would dial the configured
    // origin; the test dials the listener that origin fronts.
    // The path the descriptor names, which is deliberately not the reference the tool
    // call redeems — that one must not be derivable from a URL a proxy logs.
    let id = result["upload"]["url"]
        .as_str()
        .expect("url")
        .rsplit('/')
        .next()
        .expect("upload identifier");
    assert!(
        !uri.contains(id),
        "the staged reference must not be derivable from the upload URL"
    );
    let upload = server
        .client
        .put(format!("{}/files/upload/{id}", server.base))
        .header(
            matrix_server::files::TRANSFER_CREDENTIAL_HEADER,
            &credential,
        )
        .body(payload.clone())
        .send()
        .await
        .expect("upload");
    assert_eq!(
        upload.status().as_u16(),
        204,
        "transfer accepted with no body"
    );

    // From here it is an ordinary submission. Nothing downstream knows the difference.
    let submitted = server
        .call_tool(
            "matrix_submit_asset",
            serde_json::json!({ "source": { "uri": uri } }),
        )
        .await;
    let by_reference = tool_payload(&submitted);
    assert_eq!(
        by_reference["source_bytes"].as_u64(),
        Some(payload.len() as u64),
        "nothing declared a size, so this count came from the bytes: {by_reference}"
    );
    assert_eq!(by_reference["media_type"], "image/gif");
    assert_eq!(by_reference["frames"].as_u64(), Some(3));

    // Same contract, same report shape as an inline submission.
    let inline = tool_payload(
        &server
            .call_tool(
                "matrix_submit_asset",
                serde_json::json!({
                    "source": { "uri": format!("data:image/gif;base64,{}", base64::engine::general_purpose::STANDARD.encode(b"tiny")) }
                }),
            )
            .await,
    );
    let shape = |report: &serde_json::Value| {
        let mut keys: Vec<String> = report
            .as_object()
            .expect("report object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    };
    assert_eq!(
        shape(&by_reference),
        shape(&inline),
        "a reference and an inline submission must report identically"
    );

    // The staged bytes are consumed: the same reference cannot be replayed.
    let replayed = server
        .call_tool(
            "matrix_submit_asset",
            serde_json::json!({ "source": { "uri": uri } }),
        )
        .await;
    assert_eq!(
        replayed["error"]["data"]["code"], "matrix_unsupported_source",
        "a spent reference is just an unrecognised one: {replayed}"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caller_named_url_is_refused_while_the_transfer_plane_is_configured() {
    let panel = UdpSocket::bind("127.0.0.1:0").await.expect("bind panel");
    let panel_addr = panel.local_addr().expect("panel addr");
    let base = fake_panel(PANEL_INFO);
    let scratch = std::env::temp_dir().join(format!("matrix-refuse-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch");
    let binaries = stand_in_binaries(&scratch);

    let engine = Engine::new(
        canvas(),
        Rate::new(25).expect("rate"),
        matrix_device::WledClient::new(base, Duration::from_millis(500)).expect("client"),
        panel_addr,
    );
    let server = WireServer::start_with_files(engine, binaries, wire_plane("refuse").await).await;

    // Having a transfer plane must not turn this server into anyone's user agent.
    for uri in [
        "https://example.invalid/clip.gif",
        "http://127.0.0.1:1/clip.gif",
        "file:///etc/passwd",
        "matrix-file://staged/fabricated-identifier",
    ] {
        let response = server
            .call_tool(
                "matrix_submit_asset",
                serde_json::json!({ "source": { "uri": uri } }),
            )
            .await;
        assert_eq!(
            response["error"]["data"]["code"], "matrix_unsupported_source",
            "{uri} must be refused: {response}"
        );
    }

    let _ = std::fs::remove_dir_all(&scratch);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_inline_only_server_advertises_and_answers_exactly_as_before() {
    let panel = UdpSocket::bind("127.0.0.1:0").await.expect("bind panel");
    let panel_addr = panel.local_addr().expect("panel addr");
    let base = fake_panel(PANEL_INFO);
    let scratch = std::env::temp_dir().join(format!("matrix-off-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch");
    let binaries = stand_in_binaries(&scratch);

    let engine = Engine::new(
        canvas(),
        Rate::new(25).expect("rate"),
        matrix_device::WledClient::new(base, Duration::from_millis(500)).expect("client"),
        panel_addr,
    );
    let server = WireServer::start(engine, binaries).await;

    // Method-not-found is the contract: a file-aware intermediary reads -32601 as "no
    // native file transfer here" and stops asking.
    let authorized = server
        .rpc("files/authorizeUpload", None, serde_json::json!({}))
        .await;
    assert_eq!(
        authorized["error"]["code"].as_i64(),
        Some(-32601),
        "{authorized}"
    );

    // And the published contract says nothing about files, so nothing offers one.
    let listed = server.rpc("tools/list", None, serde_json::json!({})).await;
    let submit = listed["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|t| t["name"] == "matrix_submit_asset")
        .expect("submit tool")
        .clone();
    assert!(
        submit["inputSchema"]["properties"]["source"]["x-mcp-file"].is_null(),
        "an inline-only server must not advertise a file input: {submit}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configured_server_advertises_the_file_input_on_its_source_property() {
    let panel = UdpSocket::bind("127.0.0.1:0").await.expect("bind panel");
    let panel_addr = panel.local_addr().expect("panel addr");
    let base = fake_panel(PANEL_INFO);
    let scratch = std::env::temp_dir().join(format!("matrix-adv-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch");
    let binaries = stand_in_binaries(&scratch);

    let engine = Engine::new(
        canvas(),
        Rate::new(25).expect("rate"),
        matrix_device::WledClient::new(base, Duration::from_millis(500)).expect("client"),
        panel_addr,
    );
    let server = WireServer::start_with_files(engine, binaries, wire_plane("advert").await).await;

    let listed = server.rpc("tools/list", None, serde_json::json!({})).await;
    let submit = listed["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|t| t["name"] == "matrix_submit_asset")
        .expect("submit tool")
        .clone();
    let annotation = &submit["inputSchema"]["properties"]["source"]["x-mcp-file"];

    // Both modes: a reference is now possible and inline stays a first-class way to
    // submit a small still.
    assert_eq!(
        annotation["transferModes"],
        serde_json::json!(["inline", "upload"]),
        "{submit}"
    );
    // The advertised ceiling is the decoder's source ceiling, which is what a transfer
    // is actually bounded by.
    assert_eq!(
        annotation["maxSize"].as_u64(),
        Some(matrix_media::Limits::default().max_source_bytes),
    );

    // The catalog itself is unchanged: the transfer plane is a method, not a tool.
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    let expected: Vec<String> = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../smoke/expected-tools.txt"),
    )
    .expect("catalog")
    .lines()
    .map(|l| l.trim().to_string())
    .filter(|l| !l.is_empty())
    .collect();
    assert_eq!(names, expected, "the advertised catalog must not change");

    let _ = std::fs::remove_dir_all(&scratch);
}
