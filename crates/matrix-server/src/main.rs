//! Binary entry point.
//!
//! Serves MCP `2026-07-28` over Streamable HTTP at `/mcp`, with `/healthz` for liveness,
//! or over stdio for a single spawning client when `--stdio` is set. Stateless: the HTTP
//! transport builds a handler per request, so the engine lives behind an `Arc` the
//! factory clones; the stdio session holds one handler for its lifetime.

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

    /// Public origin this server is reached at, which enables the transfer plane.
    ///
    /// Scheme and authority only, e.g. `https://panel.example.org`. Unset leaves the
    /// server inline-only. A trusted intermediary dials this to deliver media too large
    /// for a tool argument, and reuses the addresses it pinned for `/mcp` only when this
    /// names that same host — so it should be the origin `/mcp` is already served on.
    ///
    /// `http` is accepted for a deployment whose intermediary reaches this server over a
    /// private segment it already trusts in cleartext. Whether that holds is a property
    /// of the topology this process cannot see, so it is the operator's assertion to
    /// make; an intermediary refuses a plaintext descriptor from anywhere else.
    #[arg(long, env = "MATRIX_FILE_PUBLIC_URL")]
    file_public_url: Option<String>,

    /// Directory transfers are staged in. Created if absent; must be writable.
    #[arg(
        long,
        env = "MATRIX_FILE_STAGING_DIR",
        default_value = "/tmp/matrix-staging"
    )]
    file_staging_dir: std::path::PathBuf,

    /// How long an unused authorization or an unconsumed staged transfer survives.
    #[arg(long, env = "MATRIX_FILE_TTL_SECS", default_value_t = 300)]
    file_ttl_secs: u64,

    /// Ceiling on authorizations and staged transfers outstanding at once.
    #[arg(long, env = "MATRIX_FILE_MAX_STAGED", default_value_t = 4)]
    file_max_staged: usize,

    /// Probe a running instance on loopback and exit 0 or 1.
    #[arg(long)]
    healthcheck: bool,

    /// Serve MCP over stdio for the single client that spawned this process instead of
    /// listening on HTTP. `--listen` and `--allowed-hosts` are ignored in this mode.
    /// Combining it with `--healthcheck` is rejected: there is no HTTP endpoint to
    /// probe, and silently probing one anyway would report the wrong process's health.
    #[arg(long, env = "MATRIX_STDIO", conflicts_with = "healthcheck")]
    stdio: bool,
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

    // The plane needs a listener to receive on and a TLS boundary in front of it, and
    // stdio has neither. Refusing the combination beats starting a server that mints
    // descriptors nothing can reach.
    if args.stdio && args.file_public_url.is_some() {
        anyhow::bail!(
            "--file-public-url needs the HTTP transport: under --stdio there is no \
             listener to receive a transfer on"
        );
    }

    let files = match &args.file_public_url {
        None => None,
        Some(origin) => {
            let origin = matrix_server::files::validate_public_origin(origin)
                .map_err(|e| anyhow::anyhow!("--file-public-url {e}"))?;
            let plane = matrix_server::files::FilePlane::new(matrix_server::files::FileConfig {
                public_origin: origin.clone(),
                staging_dir: args.file_staging_dir.clone(),
                ttl: Duration::from_secs(args.file_ttl_secs),
                max_staged: args.file_max_staged,
                // The decoder's own ceiling, so an over-size transfer is refused when it
                // is authorized rather than after it has been moved.
                max_source_bytes: matrix_media::Limits::default().max_source_bytes,
            })
            .await
            .context("preparing the transfer staging directory")?;

            // Without this, an authorization nobody redeems and a transfer nobody names
            // hold their files until the process exits.
            tokio::spawn(matrix_server::files::run_sweeper(plane.clone()));
            tracing::info!(
                origin = %origin,
                staging = %args.file_staging_dir.display(),
                "transfer plane enabled"
            );
            Some(plane)
        }
    };

    if args.stdio {
        use rmcp::ServiceExt;

        tracing::info!(
            wled = %args.wled_url,
            ddp = %args.ddp_addr,
            canvas = format!("{}x{}", args.width, args.height),
            target_fps = args.target_fps,
            "matrix-server ready (stdio)"
        );

        // The spawning client owns the process lifetime: when it closes stdin the
        // session ends, this returns, and process exit stops the device poller. WLED's
        // realtime timeout then returns the panel to its ambient state.
        let service = matrix_server::mcp::MatrixHandler::new(engine, binaries)
            .serve(rmcp::transport::stdio())
            .await
            .context("starting stdio transport")?;
        service.waiting().await.context("stdio session")?;
        return Ok(());
    }

    let app = matrix_server::router(engine.clone(), binaries, args.allowed_hosts.clone(), files);

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

    #[test]
    fn stdio_flag_defaults_off_and_binds_env() {
        let command = Args::command();
        let stdio = command
            .get_arguments()
            .find(|argument| argument.get_id() == "stdio")
            .expect("stdio argument");

        assert_eq!(
            stdio.get_env().and_then(|env| env.to_str()),
            Some("MATRIX_STDIO")
        );

        let args = Args::try_parse_from([
            "matrix-server",
            "--wled-url",
            "http://192.0.2.10",
            "--ddp-addr",
            "192.0.2.10:4048",
        ])
        .expect("parse without --stdio");
        assert!(!args.stdio);
    }

    #[test]
    fn stdio_rejects_healthcheck() {
        let result = Args::try_parse_from([
            "matrix-server",
            "--stdio",
            "--healthcheck",
            "--wled-url",
            "http://192.0.2.10",
            "--ddp-addr",
            "192.0.2.10:4048",
        ]);

        let error = result.expect_err("--stdio with --healthcheck must be refused");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
