//! Binary entry point.
//!
//! Serves MCP `2026-07-28` over Streamable HTTP at `/mcp`, with `/healthz` for liveness.
//! Stateless: the transport builds a handler per request, so the engine lives behind an
//! `Arc` the factory clones.

use anyhow::{Context, Result};
use clap::Parser;
use matrix_frame::{Canvas, Rate};
use matrix_server::mcp::MediaBinaries;
use matrix_server::state::{Engine, run_device_poller};
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "matrix-server",
    about = "MCP render server for a WLED LED matrix"
)]
struct Args {
    /// Address to listen on.
    #[arg(long, env = "MATRIX_HTTP_ADDR", default_value = "127.0.0.1:8080")]
    listen: SocketAddr,

    /// Origin of the WLED device, e.g. http://192.0.2.10
    #[arg(long, env = "MATRIX_WLED_URL")]
    wled_url: String,

    /// Address the panel receives DDP frames on.
    #[arg(long, env = "MATRIX_DDP_ADDR")]
    ddp_addr: SocketAddr,

    /// Panel width in pixels.
    #[arg(long, env = "MATRIX_WIDTH", default_value_t = 64)]
    width: u16,

    /// Panel height in pixels.
    #[arg(long, env = "MATRIX_HEIGHT", default_value_t = 64)]
    height: u16,

    /// Rate to aim for. The panel's reported framerate reduces this when it cannot keep
    /// up; nothing here raises it above what is asked for.
    #[arg(long, env = "MATRIX_TARGET_FPS", default_value_t = 25)]
    target_fps: u16,

    /// HTTP timeout for WLED JSON calls.
    #[arg(long, env = "MATRIX_DEVICE_TIMEOUT_MS", default_value_t = 3000)]
    device_timeout_ms: u64,

    #[arg(long, env = "MATRIX_FFMPEG_BIN", default_value = "ffmpeg")]
    ffmpeg_bin: String,

    #[arg(long, env = "MATRIX_FFPROBE_BIN", default_value = "ffprobe")]
    ffprobe_bin: String,

    /// Complete `Host`-header allowlist for the MCP transport's DNS-rebinding
    /// guard, comma-separated. Must include every authority clients or proxies dial.
    #[arg(
        long,
        env = "MATRIX_ALLOWED_HOSTS",
        value_delimiter = ',',
        default_value = "localhost,127.0.0.1,::1"
    )]
    allowed_hosts: Vec<String>,

    /// Probe a running instance on loopback and exit 0 or 1.
    #[arg(long)]
    healthcheck: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "matrix_server=info,rmcp=warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    if args.healthcheck {
        let port = args.listen.port();
        let url = format!("http://127.0.0.1:{port}/healthz");
        let ok = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .get(&url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if ok {
            return Ok(());
        }
        std::process::exit(1);
    }

    let canvas = Canvas::new(args.width, args.height)
        .with_context(|| format!("panel dimensions {}x{}", args.width, args.height))?;
    let rate = Rate::new(args.target_fps)
        .with_context(|| format!("target framerate {}", args.target_fps))?;

    let wled = matrix_device::WledClient::new(
        args.wled_url.clone(),
        Duration::from_millis(args.device_timeout_ms),
    )
    .context("WLED base URL")?;

    let engine = Engine::new(canvas, rate, wled, args.ddp_addr);

    // Without this the reported framerate never changes and rate adaptation has nothing
    // to adapt to. It is part of the running server, not a diagnostic.
    tokio::spawn(run_device_poller(engine.clone()));

    let binaries = MediaBinaries {
        ffmpeg: args.ffmpeg_bin.clone(),
        ffprobe: args.ffprobe_bin.clone(),
    };

    let app = matrix_server::router(engine.clone(), binaries, args.allowed_hosts.clone());

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;

    tracing::info!(
        listen = %args.listen,
        wled = %args.wled_url,
        ddp = %args.ddp_addr,
        canvas = format!("{}x{}", args.width, args.height),
        target_fps = args.target_fps,
        "matrix-server ready"
    );

    axum::serve(listener, app).await.context("serving")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn standalone_listener_defaults_to_loopback() {
        let command = Args::command();
        let listen = command
            .get_arguments()
            .find(|argument| argument.get_id() == "listen")
            .expect("listen argument");
        let defaults: Vec<_> = listen
            .get_default_values()
            .iter()
            .map(|value| value.to_str())
            .collect();

        assert_eq!(defaults, [Some("127.0.0.1:8080")]);
    }
}
