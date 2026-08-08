//! The `2026-07-28` handler.
//!
//! Stateless by construction: the transport builds one of these per request, so it
//! holds nothing but a clone of the shared [`Engine`] handle. Every identifier a caller
//! gets back — an asset handle, a playback handle — is a key into that shared state and
//! is passed as an ordinary tool argument, which is the protocol's cross-call
//! mechanism.

use crate::state::Engine;
use crate::tools::{self, FileValue};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CacheScope, ListToolsResult, PaginatedRequestParams};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use std::sync::Arc;

/// Binaries the media path shells out to, resolved once at startup.
#[derive(Debug, Clone)]
pub struct MediaBinaries {
    pub ffmpeg: String,
    pub ffprobe: String,
}

#[derive(Clone)]
pub struct MatrixHandler {
    engine: Arc<Engine>,
    binaries: MediaBinaries,
}

impl MatrixHandler {
    pub fn new(engine: Arc<Engine>, binaries: MediaBinaries) -> Self {
        Self { engine, binaries }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SubmitParams {
    /// The media to normalize and hold.
    pub source: FileValue,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PlayParams {
    /// An asset handle returned by `matrix_submit_asset`.
    pub asset: String,
    /// Repeat the sequence until stopped. Defaults to false.
    #[serde(default)]
    pub looping: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StopParams {
    /// A playback handle. Omitted stops whatever is playing; supplied and stale is
    /// refused rather than cancelling something the caller did not start.
    #[serde(default)]
    pub playback: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ShowTextParams {
    /// The text to display. Input is capped at 100 characters; scrolling text must
    /// also fit the configured canvas and frame budget. ASCII renders exactly;
    /// anything the font cannot draw becomes a visible `?`.
    pub text: String,
    /// Start playing immediately. Defaults to true; false holds the asset for a
    /// later `matrix_play`.
    #[serde(default = "default_true")]
    pub play: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct BrightnessParams {
    /// 0 to 255.
    pub level: u8,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PowerParams {
    pub on: bool,
}

/// Implementation-defined JSON-RPC code for a domain refusal.
///
/// These are not invalid params — the arguments parsed fine and the server refused for
/// a reason the caller can act on. `-32050` sits in the implementation-defined range;
/// the stable machine contract is the string code carried in `data.code`.
const DOMAIN_REFUSAL: rmcp::model::ErrorCode = rmcp::model::ErrorCode(-32050);

fn fail(error: impl std::fmt::Display, code: &str) -> McpError {
    McpError::new(
        DOMAIN_REFUSAL,
        format!("{code}: {error}"),
        Some(serde_json::json!({ "code": code })),
    )
}

fn json<T: serde::Serialize>(value: &T) -> Result<String, McpError> {
    serde_json::to_string(value).map_err(|e| fail(e, "matrix_serialization_failed"))
}

#[tool_router]
impl MatrixHandler {
    #[tool(
        description = "Read the panel: identity, firmware, dimensions, the framerate it \
                       is achieving, and its power draw against any configured ceiling. \
                       Polling also refreshes the framerate that paces playback.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn matrix_describe_device(&self) -> Result<String, McpError> {
        let report = tools::describe_device(&self.engine)
            .await
            .map_err(|e| fail(&e, e.code()))?;
        json(&report)
    }

    #[tool(
        description = "Report what is playing, how many assets are held, and the \
                       framerate the panel last reported.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn matrix_status(&self) -> Result<String, McpError> {
        json(&tools::status(&self.engine).await)
    }

    #[tool(
        description = "Normalize media into frames the panel can display and hold it, \
                       returning an asset handle. Accepts a base64 data: URI up to 16 KiB \
                       — content at or near the panel's resolution. Larger media needs an \
                       artifact reference, which this server does not accept yet. Returns \
                       the frame count and duration so a caller knows what it got without \
                       playing it.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn matrix_submit_asset(
        &self,
        Parameters(SubmitParams { source }): Parameters<SubmitParams>,
    ) -> Result<String, McpError> {
        let report = tools::submit_asset(
            &self.engine,
            &source,
            &self.binaries.ffmpeg,
            &self.binaries.ffprobe,
        )
        .await
        .map_err(|e| fail(&e, e.code()))?;
        json(&report)
    }

    #[tool(
        description = "List the assets currently held, newest handles last.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn matrix_list_assets(&self) -> Result<String, McpError> {
        json(&tools::list_assets(&self.engine).await)
    }

    #[tool(
        description = "Play a held asset on the panel, replacing whatever was playing. \
                       Returns a playback handle. Playback is paced by the framerate the \
                       panel reports and each frame is clamped to its power ceiling.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn matrix_play(
        &self,
        Parameters(PlayParams { asset, looping }): Parameters<PlayParams>,
    ) -> Result<String, McpError> {
        let handle = tools::play(&self.engine, &asset, looping)
            .await
            .map_err(|e| fail(&e, e.code()))?;
        json(&serde_json::json!({ "playback": handle }))
    }

    #[tool(
        description = "Display a line of text on the panel. Text that fits shows as a \
                       centered still; longer text scrolls as a marquee until stopped. \
                       Returns the asset handle and, when played, the playback handle.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn matrix_show_text(
        &self,
        Parameters(ShowTextParams { text, play }): Parameters<ShowTextParams>,
    ) -> Result<String, McpError> {
        let (asset, playback) = tools::show_text(&self.engine, &text, play)
            .await
            .map_err(|e| fail(&e, e.code()))?;
        let visible = tools::text_visible_chars(&self.engine);
        json(&serde_json::json!({
            "asset": asset,
            "playback": playback,
            "visible_chars": visible,
            "scrolls": asset.frames > 1,
        }))
    }

    #[tool(
        description = "Stop playback. The panel returns to its configured ambient \
                       behaviour once the frame stream stops.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn matrix_stop(
        &self,
        Parameters(StopParams { playback }): Parameters<StopParams>,
    ) -> Result<String, McpError> {
        let stopped = tools::stop(&self.engine, playback.as_deref())
            .await
            .map_err(|e| fail(&e, e.code()))?;
        json(&serde_json::json!({ "stopped": stopped }))
    }

    #[tool(
        description = "Set panel brightness, 0 to 255.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn matrix_set_brightness(
        &self,
        Parameters(BrightnessParams { level }): Parameters<BrightnessParams>,
    ) -> Result<String, McpError> {
        tools::set_brightness(&self.engine, level)
            .await
            .map_err(|e| fail(&e, e.code()))?;
        json(&serde_json::json!({ "brightness": level }))
    }

    #[tool(
        description = "Turn the panel on or off.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn matrix_power(
        &self,
        Parameters(PowerParams { on }): Parameters<PowerParams>,
    ) -> Result<String, McpError> {
        tools::set_power(&self.engine, on)
            .await
            .map_err(|e| fail(&e, e.code()))?;
        json(&serde_json::json!({ "on": on }))
    }
}

/// Device capability data changes only on a reflash or a settings change, and the tool
/// list is fixed for a build, so a short freshness hint would spend requests re-reading
/// constants.
const TOOL_LIST_TTL_MS: u64 = 300_000;

#[tool_handler(
    name = "matrix-server",
    version = "0.1.0",
    instructions = "Renders media to a WLED LED matrix. Submit media to get an asset \
                    handle, then play that handle. Playback is paced by the framerate \
                    the panel reports and each frame is clamped to the panel's power \
                    ceiling. Stopping returns the panel to its configured ambient \
                    behaviour."
)]
impl ServerHandler for MatrixHandler {
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // Deterministic order so a client cache and an LLM prompt cache both hit; the
        // router yields registration order, which is stable per build but not sorted.
        let mut tools = Self::tool_router().list_all();
        tools.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(ListToolsResult {
            tools,
            ..ListToolsResult::default()
        }
        .with_ttl_ms(TOOL_LIST_TTL_MS)
        // Private: the catalog belongs to one server instance. A shared intermediary
        // must not serve one instance's tools as another's.
        .with_cache_scope(CacheScope::Private))
    }
}
