//! The `2026-07-28` handler.
//!
//! Stateless by construction: the transport builds one of these per request, so it
//! holds nothing but a clone of the shared [`Engine`] handle. Every identifier a caller
//! gets back — an asset handle, a playback handle — is a key into that shared state and
//! is passed as an ordinary tool argument, which is the protocol's cross-call
//! mechanism.

use crate::state::Engine;
use crate::tools::{self, FileValue, RegionParam};
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
    files: Option<Arc<crate::files::FilePlane>>,
}

impl MatrixHandler {
    pub fn new(engine: Arc<Engine>, binaries: MediaBinaries) -> Self {
        Self {
            engine,
            binaries,
            files: None,
        }
    }

    /// Serve the transfer plane too. Without it this server is inline-only and answers
    /// the authorization method with method-not-found, which is what a file-aware
    /// intermediary reads as "no native file transfer".
    pub fn with_files(mut self, files: Option<Arc<crate::files::FilePlane>>) -> Self {
        self.files = files;
        self
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitParams {
    /// The media to normalize and hold.
    pub source: FileValue,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlayParams {
    /// An asset handle returned by `matrix_submit_asset`.
    pub asset: String,
    /// Repeat the sequence until stopped. Defaults to false.
    #[serde(default)]
    pub looping: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StopParams {
    /// A playback handle. Omitted stops whatever is playing; supplied and stale is
    /// refused rather than cancelling something the caller did not start.
    #[serde(default)]
    pub playback: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ShowTextLayoutParams {
    /// The text regions to compose, at most 16. Rectangles must lie on the canvas
    /// and must not overlap; fixed text must fit its rectangle, and scrolling text
    /// must fit it across the axis it does not travel.
    #[schemars(length(min = 1, max = 16))]
    pub regions: Vec<RegionParam>,
    /// Start playing immediately. Defaults to true; false holds the asset for a
    /// later `matrix_play`.
    #[serde(default = "default_true")]
    pub play: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrightnessParams {
    /// 0 to 255.
    pub level: u8,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
                       — content at or near the panel's resolution. Larger media needs a \
                       reference obtained through this server's file transfer, which is \
                       available only where the operator has configured it. Returns the \
                       frame count and duration so a caller knows what it got without \
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
            self.files.as_ref(),
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
        description = "Compose fixed and scrolling text regions into one animated \
                       package. Each region has a rectangle, text, style, and either \
                       fixed alignment or a scroll path (four canonical paths, each \
                       reversible) with a speed in pixels per second. The longest \
                       single crossing sets the package length and looping repeats \
                       the whole package. By default a scroller crosses once and \
                       parks; `repeat: true` re-enters for as many evenly spaced \
                       crossings as fit the package, and `phase` (0 to below 1, \
                       requires repeat) offsets where in its cycle a region starts, \
                       so regions animate continuously and independently. Rectangles \
                       must not overlap. Returns the asset handle and, when played, \
                       the playback handle.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn matrix_show_text_layout(
        &self,
        Parameters(ShowTextLayoutParams { regions, play }): Parameters<ShowTextLayoutParams>,
    ) -> Result<String, McpError> {
        let (asset, playback) = tools::show_text_layout(&self.engine, &regions, play)
            .await
            .map_err(|e| fail(&e, e.code()))?;
        json(&serde_json::json!({
            "asset": asset,
            "playback": playback,
            "regions": regions.len(),
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

/// Mark `source` as a file-valued input, so an intermediary knows it may deliver one.
///
/// The annotation goes on the `source` subschema, which is the object shape this server
/// has always accepted. `transferModes` names both paths deliberately: inline stays a
/// first-class way to submit a small still, and the ceiling advertised here is the
/// decoder's source ceiling rather than the inline cap, because the inline cap is a
/// separate and much lower bound this server enforces itself.
fn annotate_file_input(tool: &mut rmcp::model::Tool) {
    let mut schema = (*tool.input_schema).clone();
    let Some(source) = schema
        .get_mut("properties")
        .and_then(|p| p.get_mut("source"))
        .and_then(|s| s.as_object_mut())
    else {
        // The schema is derived, so this cannot happen for a build that compiles; saying
        // nothing is still better than publishing an annotation on the wrong property.
        tracing::warn!("matrix_submit_asset has no source property to annotate");
        return;
    };

    source.insert(
        "x-mcp-file".into(),
        serde_json::json!({
            "transferModes": ["inline", "upload"],
            "maxSize": matrix_media::Limits::default().max_source_bytes,
        }),
    );
    tool.input_schema = Arc::new(schema);

    // The prose has to agree with the annotation. A tool that declares an upload mode
    // while telling callers references are unavailable is publishing two contracts.
    tool.description = Some(
        "Normalize media into frames the panel can display and hold it, returning an \
         asset handle. Accepts a base64 data: URI up to 16 KiB — content at or near the \
         panel's resolution — or a reference to media already delivered through this \
         server's file transfer, which is how anything larger arrives. Returns the frame \
         count and duration so a caller knows what it got without playing it."
            .into(),
    );
}

#[tool_handler(
    name = "matrix-server",
    // The macro takes a literal, so this cannot read the crate metadata directly. It
    // said 0.2.0 through the whole of 0.3.0 for exactly the reason a version in two
    // places always drifts — only one of them got updated. `the_advertised_version_is_
    // the_crate_version` fails the build when they disagree, which is the part that
    // stops it happening again.
    version = "0.4.1",
    instructions = "Renders media to a WLED LED matrix. Submit media to get an asset \
                    handle, then play that handle. Playback is paced by the framerate \
                    the panel reports and each frame is clamped to the panel's power \
                    ceiling. Stopping returns the panel to its configured ambient \
                    behaviour."
)]
impl ServerHandler for MatrixHandler {
    /// Serve the draft's upload authorization when the transfer plane is configured.
    ///
    /// Everything else — including this method on an inline-only server — falls through
    /// to the library's method-not-found. That answer is the contract: a file-aware
    /// intermediary reads `-32601` as "this upstream has no native file transfer" and
    /// stops asking, so an unconfigured deployment needs no advertisement to say so.
    ///
    /// The draft is unmerged, so its spellings live in one module rather than being
    /// spread across the handler.
    async fn on_custom_request(
        &self,
        request: rmcp::model::CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CustomResult, McpError> {
        use crate::files::AuthorizeUploadParams;

        if request.method != crate::files::AUTHORIZE_UPLOAD {
            return Err(McpError::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                request.method,
                None,
            ));
        }
        let Some(files) = self.files.as_ref() else {
            return Err(McpError::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                request.method,
                None,
            ));
        };

        // Absent params are an authorization with nothing declared, which is legitimate:
        // every field of the draft's request is optional.
        let params: AuthorizeUploadParams = match request.params {
            None => AuthorizeUploadParams::default(),
            Some(value) => serde_json::from_value(value).map_err(|e| {
                McpError::new(
                    rmcp::model::ErrorCode::INVALID_PARAMS,
                    format!("matrix_file_bad_params: {e}"),
                    Some(serde_json::json!({ "code": "matrix_file_bad_params" })),
                )
            })?,
        };

        let authorized = files
            .authorize_upload(params)
            .await
            .map_err(|e| fail(&e, e.code()))?;
        let value = serde_json::to_value(&authorized)
            .map_err(|e| fail(e, "matrix_serialization_failed"))?;
        Ok(rmcp::model::CustomResult::new(value))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // Deterministic order so a client cache and an LLM prompt cache both hit; the
        // router yields registration order, which is stable per build but not sorted.
        let mut tools = Self::tool_router().list_all();
        tools.sort_by(|a, b| a.name.cmp(&b.name));

        // The annotation is what makes an intermediary offer this tool a file at all, so
        // it appears only where the transfer plane can actually receive one. An
        // inline-only deployment therefore publishes exactly the schema it publishes
        // today, rather than advertising a capability it would then refuse.
        if self.files.is_some() {
            for tool in &mut tools {
                if tool.name == "matrix_submit_asset" {
                    annotate_file_input(tool);
                }
            }
        }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The version a client is told must be the version that was built.
    ///
    /// `#[tool_handler]` takes a literal, so the crate version cannot be interpolated
    /// into it and the two are maintained separately. They drifted once already — the
    /// handler advertised 0.2.0 for the whole of 0.3.0 — and a caller has no way to
    /// notice, because the wrong answer is a perfectly well-formed one. Failing here
    /// makes a release that forgets this impossible to ship.
    #[test]
    fn the_advertised_version_is_the_crate_version() {
        let engine = crate::state::Engine::new(
            matrix_frame::Canvas::new(64, 64).expect("valid"),
            matrix_frame::Rate::new(25).expect("valid"),
            matrix_device::WledClient::new(
                "http://127.0.0.1:1".to_string(),
                std::time::Duration::from_millis(1),
            )
            .expect("valid base"),
            "127.0.0.1:4048".parse().expect("addr"),
        );
        let handler = MatrixHandler::new(
            engine,
            MediaBinaries {
                ffmpeg: "ffmpeg".into(),
                ffprobe: "ffprobe".into(),
            },
        );

        assert_eq!(
            handler.get_info().server_info.version,
            env!("CARGO_PKG_VERSION"),
        );
    }
}
