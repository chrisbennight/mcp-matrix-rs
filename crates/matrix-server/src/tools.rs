//! The MCP tool surface.
//!
//! Every tool drives the engine rather than describing it. A tool that only reported a
//! value the engine computed, without the engine acting, would be indistinguishable
//! from a working one in a unit test and useless on the panel.

use crate::state::{Engine, EngineError};
use base64::Engine as _;
use matrix_media::{Limits, NormalizeParams};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// Largest inline payload accepted in a tool argument.
///
/// A 64x64 PNG at native resolution is a few kilobytes and reasonable to inline. Media
/// needing a downscale is by definition larger than this and belongs on a transfer path
/// rather than in a JSON argument, so the bound is deliberately low.
pub const MAX_INLINE_BYTES: usize = 16 * 1024;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error(transparent)]
    Engine(#[from] EngineError),

    #[error("media rejected: {0}")]
    Media(#[from] matrix_media::MediaError),

    #[error("unsupported source: {0}")]
    UnsupportedSource(String),

    #[error("inline payload is {actual} bytes, limit is {limit}")]
    InlineTooLarge { actual: usize, limit: usize },

    #[error("could not decode the inline payload: {0}")]
    InlineMalformed(String),

    #[error("text rejected: {0}")]
    Text(#[from] matrix_text::TextError),

    #[error("layout rejected: {0}")]
    Layout(#[from] matrix_text::layout::LayoutError),
}

impl ToolError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Engine(inner) => inner.code(),
            Self::Media(inner) => inner.code(),
            Self::UnsupportedSource(_) => "matrix_unsupported_source",
            Self::InlineTooLarge { .. } => "matrix_inline_too_large",
            Self::InlineMalformed(_) => "matrix_inline_malformed",
            Self::Text(inner) => inner.code(),
            Self::Layout(inner) => inner.code(),
        }
    }
}

/// A reference to media, shaped so an intermediary-minted reference and an inline
/// payload use one contract.
///
/// The shape matches SEP-2631's file object, so adopting that draft later changes how a
/// `uri` is produced and not what this tool accepts. Unlike the tool parameter structs,
/// unknown keys are tolerated here: the shape is an external draft's, and a future
/// revision may add fields an intermediary forwards before this server learns them.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct FileValue {
    /// A `data:` URI for content already at native resolution, or a URI a trusted
    /// intermediary resolved. A destination named by the caller is never dereferenced
    /// here.
    pub uri: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

/// Bytes and a media type extracted from a reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSource {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

/// Turn a reference into bytes.
///
/// Only `data:` URIs resolve here. Any other scheme is refused rather than fetched: the
/// caller chose that destination, and dereferencing it would make this server their user
/// agent. Resolving a non-inline reference belongs to a trusted transfer boundary,
/// which is why the tool contract accepts one shape regardless of where the bytes came
/// from.
pub fn resolve_inline(value: &FileValue) -> Result<ResolvedSource, ToolError> {
    let rest = value.uri.strip_prefix("data:").ok_or_else(|| {
        ToolError::UnsupportedSource(format!(
            "only data: URIs are resolved here, got {:?}",
            value.uri.chars().take(24).collect::<String>()
        ))
    })?;

    let (meta, payload) = rest.split_once(',').ok_or_else(|| {
        ToolError::InlineMalformed("a data: URI needs a comma before its payload".into())
    })?;

    if !meta.ends_with(";base64") {
        return Err(ToolError::InlineMalformed(
            "only base64-encoded data: URIs are accepted".into(),
        ));
    }

    let declared = meta.trim_end_matches(";base64");
    let media_type = if declared.is_empty() {
        value
            .mime_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".into())
    } else {
        declared.to_string()
    };

    // Checked against the encoded length first, so an oversized payload never gets a
    // decode buffer allocated for it.
    if payload.len() > MAX_INLINE_BYTES * 4 / 3 + 4 {
        return Err(ToolError::InlineTooLarge {
            actual: payload.len() * 3 / 4,
            limit: MAX_INLINE_BYTES,
        });
    }

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| ToolError::InlineMalformed(e.to_string()))?;

    if bytes.len() > MAX_INLINE_BYTES {
        return Err(ToolError::InlineTooLarge {
            actual: bytes.len(),
            limit: MAX_INLINE_BYTES,
        });
    }

    Ok(ResolvedSource { bytes, media_type })
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DeviceReport {
    pub name: String,
    pub version: String,
    pub width: u16,
    pub height: u16,
    pub pixels: usize,
    pub reported_fps: u16,
    /// The fixed rate sequences render and play at, set by the operator. Also the
    /// ceiling for a layout scroller's `speed_px_s` — the pre-render pacing
    /// contract for `matrix_show_text_layout`, the way `text_visible_chars` is the
    /// sizing contract for `matrix_show_text`.
    pub target_fps: u16,
    pub power_ma: u32,
    pub power_ceiling_ma: u32,
    pub enforces_power_ceiling: bool,
    /// Characters of text visible at once at the default text scale — the pre-render
    /// sizing contract for `matrix_show_text`.
    pub text_visible_chars: usize,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AssetReport {
    pub handle: String,
    pub frames: usize,
    pub fps: u16,
    pub duration_ms: u64,
    pub media_type: String,
    pub source_bytes: u64,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct StatusReport {
    pub playing: Option<String>,
    pub playing_asset: Option<String>,
    pub assets: usize,
    pub reported_fps: u16,
}

/// Read the device and report it. Polling also republishes the framerate the pump uses.
pub async fn describe_device(engine: &Arc<Engine>) -> Result<DeviceReport, ToolError> {
    let info = engine.poll_device().await?;
    let (width, height) = info
        .leds
        .matrix
        .map(|m| (m.width, m.height))
        .unwrap_or((engine.canvas.width(), engine.canvas.height()));

    Ok(DeviceReport {
        name: info.name,
        version: info.version,
        width,
        height,
        pixels: engine.canvas.pixels(),
        reported_fps: info.leds.fps,
        target_fps: engine.target_rate.fps(),
        power_ma: info.leds.power_ma,
        power_ceiling_ma: info.leds.max_power_ma,
        enforces_power_ceiling: info.leds.has_power_ceiling(),
        text_visible_chars: text_visible_chars(engine),
    })
}

/// Decode a reference into a normalized asset and hold it.
pub async fn submit_asset(
    engine: &Arc<Engine>,
    value: &FileValue,
    ffmpeg_bin: &str,
    ffprobe_bin: &str,
) -> Result<AssetReport, ToolError> {
    let source = resolve_inline(value)?;

    let params = NormalizeParams {
        canvas: engine.canvas,
        rate: engine.target_rate,
        limits: Limits::default(),
    };

    // The permit spans only the decode, not the asset store: it exists to bound
    // subprocess concurrency, and holding it across the store would serialize work
    // that touches no subprocess. A saturated queue refuses with a busy error rather
    // than parking the request indefinitely.
    let sequence = {
        let _slot = engine.acquire_decode_slot().await?;
        matrix_media::decode(
            &matrix_media::Source::bytes(&source.bytes),
            None,
            None,
            &params,
            ffmpeg_bin,
            ffprobe_bin,
        )
        .await?
    };

    let asset = engine
        .store_asset(
            sequence,
            source.bytes.len() as u64,
            source.media_type.clone(),
        )
        .await;

    Ok(report_for(&asset))
}

fn report_for(asset: &crate::state::Asset) -> AssetReport {
    AssetReport {
        handle: asset.handle.clone(),
        frames: asset.sequence.len(),
        fps: asset.sequence.rate().fps(),
        duration_ms: u64::try_from(asset.sequence.duration().as_millis()).unwrap_or(u64::MAX),
        media_type: asset.media_type.clone(),
        source_bytes: asset.source_bytes,
    }
}

fn report_for_meta(meta: &crate::state::AssetMeta) -> AssetReport {
    AssetReport {
        handle: meta.handle.clone(),
        frames: meta.frames,
        fps: meta.fps,
        duration_ms: u64::try_from(meta.duration.as_millis()).unwrap_or(u64::MAX),
        media_type: meta.media_type.clone(),
        source_bytes: meta.source_bytes,
    }
}

/// Listing is a read path: it reports metadata without cloning any frame data.
pub async fn list_assets(engine: &Arc<Engine>) -> Vec<AssetReport> {
    engine
        .asset_metas()
        .await
        .iter()
        .map(report_for_meta)
        .collect()
}

/// The frame budget every text ingest path renders under — the same cap the media
/// path decodes under, derived from the engine's canvas and target rate.
fn text_frame_budget(engine: &Arc<Engine>) -> u64 {
    Limits::default().frame_cap_for_frame_size(engine.target_rate.fps(), engine.canvas.byte_len())
}

/// Mint, optionally play, then store a rendered text sequence.
///
/// The one place the text tools' asset discipline lives, so the two rasterizing
/// paths cannot drift apart. Playback starts before the asset is committed to the
/// store: a failed start — unreachable panel, refused socket — then costs nothing
/// and evicts nothing. Text always plays looping: a marquee or layout package
/// repeats until stopped, and a still holds anyway under the panel's realtime
/// timeout semantics.
async fn hold_text_asset(
    engine: &Arc<Engine>,
    sequence: matrix_frame::FrameSequence,
    source_bytes: u64,
    play_now: bool,
) -> Result<(AssetReport, Option<String>), ToolError> {
    let asset = crate::state::Asset {
        handle: engine.mint_asset_handle(),
        sequence,
        source_bytes,
        media_type: "text/plain".into(),
    };
    let report = report_for(&asset);

    let playback = if play_now {
        Some(engine.play_asset(&asset, true).await?)
    } else {
        None
    };

    engine.store_prepared_asset(asset).await;
    Ok((report, playback))
}

/// Rasterize text into a held asset, optionally playing it at once.
///
/// Text does not ride the media path: it carries no container, so there is nothing
/// to probe or decode and no subprocess is involved. Rendering still materializes a
/// full frame sequence just as a decode does, so it runs under the same aggregate
/// ingest slot — held until the sequence is committed, not just across the render.
/// The slot is the only aggregate bound on how many maximal sequences exist at
/// once, and the handoff parks on the device poll for up to the WLED timeout while
/// holding one; releasing at the render would let sequences pile up behind a slow
/// panel with nothing left to refuse them.
pub async fn show_text(
    engine: &Arc<Engine>,
    text: &str,
    play_now: bool,
) -> Result<(AssetReport, Option<String>), ToolError> {
    let style = matrix_text::TextStyle::default();
    let _slot = engine.acquire_decode_slot().await?;
    let sequence = matrix_text::render(
        text,
        engine.canvas,
        engine.target_rate,
        style,
        text_frame_budget(engine),
    )?;
    hold_text_asset(engine, sequence, text.len() as u64, play_now).await
}

/// Characters visible at once on the configured canvas at the default text scale.
pub fn text_visible_chars(engine: &Arc<Engine>) -> usize {
    matrix_text::visible_chars(engine.canvas, matrix_text::TextStyle::default().scale)
}

/// A region's bounds on the canvas, in pixels.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RectParam {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// How a region draws its text.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegionStyleParam {
    /// Integer glyph magnification, 1 to 4. Defaults to 2.
    #[serde(default = "default_region_scale")]
    #[schemars(range(min = 1, max = 4))]
    pub scale: u8,
    /// Text color as `#rrggbb`. Defaults to white.
    #[serde(default = "default_region_foreground")]
    pub foreground: String,
    /// Background painted across the whole rectangle, as `#rrggbb`. Defaults to
    /// black. A bright background over a large rectangle raises the frame's power
    /// draw, and the power clamp dims whole frames uniformly.
    #[serde(default = "default_region_background")]
    pub background: String,
}

fn default_region_scale() -> u8 {
    matrix_text::TextStyle::default().scale
}

fn default_region_foreground() -> String {
    "#ffffff".into()
}

fn default_region_background() -> String {
    "#000000".into()
}

impl Default for RegionStyleParam {
    fn default() -> Self {
        Self {
            scale: default_region_scale(),
            foreground: default_region_foreground(),
            background: default_region_background(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlignParam {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PathParam {
    LeftToRight,
    TopToBottom,
    TopLeftToBottomRight,
    BottomLeftToTopRight,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DirectionParam {
    #[default]
    Normal,
    Reverse,
}

/// A region's behavior over the package timeline.
///
/// Deserialization is stricter than serde's tagged-enum default: a typo'd or
/// misplaced key inside this object would otherwise be dropped silently and render
/// the wrong animation with a success response, so the hand-written impl below
/// flattens the object, refuses unknown keys, and refuses fields that belong to
/// the other variant.
#[derive(Debug, Clone, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BehaviorParam {
    /// Text drawn once, visible in every frame. Must fit its rectangle.
    Fixed {
        /// Horizontal placement inside the rectangle. Defaults to center.
        #[serde(default)]
        align: AlignParam,
    },
    /// Text that starts fully outside one edge, crosses the rectangle, and exits the
    /// far edge. `reverse` retraces the named path backwards; glyphs stay upright.
    Scroll {
        path: PathParam,
        #[serde(default)]
        direction: DirectionParam,
        /// Pixels per second along the dominant axis of travel, sampled into the
        /// panel's fixed frame rate. At least 1 and at most the panel's target
        /// frame rate — one pixel per frame — so motion never skips glyph strokes.
        /// `matrix_describe_device` reports the target rate as `target_fps`.
        #[schemars(range(min = 1))]
        speed_px_s: u16,
        /// `false` (the default) crosses once and parks off-screen for the rest of
        /// the package. `true` re-enters for as many evenly spaced crossings as fit
        /// the package, so the region stays animated for the whole loop.
        #[serde(default)]
        repeat: bool,
        /// Fraction of this region's cycle, at least 0 and below 1, by which its
        /// timeline starts advanced — regions with different phases enter at
        /// different moments instead of all together. Requires `repeat: true`.
        /// Applied in thousandths of a cycle, rounded.
        #[serde(default)]
        #[schemars(range(min = 0.0), extend("exclusiveMaximum" = 1.0))]
        phase: f64,
    },
}

/// The flat shape `BehaviorParam` deserializes through. Flattening lets serde's
/// `deny_unknown_fields` cover the whole object — the internally tagged derive
/// cannot — and the per-variant checks below catch known fields on the wrong
/// variant, which the flat shape would otherwise silently accept.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct BehaviorRaw {
    #[serde(rename = "type")]
    kind: BehaviorKind,
    align: Option<AlignParam>,
    path: Option<PathParam>,
    direction: Option<DirectionParam>,
    speed_px_s: Option<u16>,
    repeat: Option<bool>,
    phase: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum BehaviorKind {
    Fixed,
    Scroll,
}

impl<'de> Deserialize<'de> for BehaviorParam {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let raw = BehaviorRaw::deserialize(deserializer)?;
        match raw.kind {
            BehaviorKind::Fixed => {
                if raw.path.is_some()
                    || raw.direction.is_some()
                    || raw.speed_px_s.is_some()
                    || raw.repeat.is_some()
                    || raw.phase.is_some()
                {
                    return Err(D::Error::custom(
                        "`path`, `direction`, `speed_px_s`, `repeat`, and `phase` belong \
                         to `type: scroll`, not `type: fixed`",
                    ));
                }
                Ok(BehaviorParam::Fixed {
                    align: raw.align.unwrap_or_default(),
                })
            }
            BehaviorKind::Scroll => {
                if raw.align.is_some() {
                    return Err(D::Error::custom(
                        "`align` belongs to `type: fixed`, not `type: scroll`",
                    ));
                }
                let path = raw.path.ok_or_else(|| D::Error::missing_field("path"))?;
                let speed_px_s = raw
                    .speed_px_s
                    .ok_or_else(|| D::Error::missing_field("speed_px_s"))?;
                let repeat = raw.repeat.unwrap_or(false);
                if raw.phase.is_some() && !repeat {
                    return Err(D::Error::custom(
                        "`phase` requires `repeat: true`: a single crossing has no \
                         cycle to offset",
                    ));
                }
                let phase = raw.phase.unwrap_or(0.0);
                if !(0.0..1.0).contains(&phase) {
                    return Err(D::Error::custom("`phase` must be at least 0 and below 1"));
                }
                Ok(BehaviorParam::Scroll {
                    path,
                    direction: raw.direction.unwrap_or_default(),
                    speed_px_s,
                    repeat,
                    phase,
                })
            }
        }
    }
}

/// One rectangular text region of a layout.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegionParam {
    pub rect: RectParam,
    /// Up to 100 characters; anything the font cannot draw becomes a visible `?`.
    pub text: String,
    #[serde(default)]
    pub style: RegionStyleParam,
    pub behavior: BehaviorParam,
}

/// Turn wire-shaped regions into the layout engine's typed specs.
fn layout_specs(
    regions: &[RegionParam],
) -> Result<Vec<matrix_text::layout::RegionSpec>, ToolError> {
    use matrix_text::layout;

    regions
        .iter()
        .enumerate()
        .map(|(index, region)| {
            // The refusal names the offending region and echoes a bounded prefix
            // of the submitted value, never the whole string.
            let color = |value: &str| {
                layout::parse_color(value).ok_or_else(|| layout::LayoutError::BadColor {
                    region: index,
                    color: value.chars().take(24).collect(),
                })
            };
            let style = matrix_text::TextStyle {
                foreground: color(&region.style.foreground)?,
                background: color(&region.style.background)?,
                scale: region.style.scale,
            };
            let behavior = match &region.behavior {
                BehaviorParam::Fixed { align } => layout::RegionBehavior::Fixed {
                    align: match align {
                        AlignParam::Left => layout::Align::Left,
                        AlignParam::Center => layout::Align::Center,
                        AlignParam::Right => layout::Align::Right,
                    },
                },
                BehaviorParam::Scroll {
                    path,
                    direction,
                    speed_px_s,
                    repeat,
                    phase,
                } => layout::RegionBehavior::Scroll {
                    path: match path {
                        PathParam::LeftToRight => layout::ScrollPath::LeftToRight,
                        PathParam::TopToBottom => layout::ScrollPath::TopToBottom,
                        PathParam::TopLeftToBottomRight => layout::ScrollPath::TopLeftToBottomRight,
                        PathParam::BottomLeftToTopRight => layout::ScrollPath::BottomLeftToTopRight,
                    },
                    direction: match direction {
                        DirectionParam::Normal => layout::ScrollDirection::Normal,
                        DirectionParam::Reverse => layout::ScrollDirection::Reverse,
                    },
                    speed_px_s: *speed_px_s,
                    cadence: if *repeat {
                        layout::Cadence::Repeat {
                            // Deserialization bounds phase below 1, so the rounded
                            // thousandths stay within the engine's 0..=999 window.
                            phase_per_mille: (phase * 1000.0).round().min(999.0) as u16,
                        }
                    } else {
                        layout::Cadence::Once
                    },
                },
            };
            Ok(layout::RegionSpec {
                rect: layout::Rect {
                    x: region.rect.x,
                    y: region.rect.y,
                    width: region.rect.width,
                    height: region.rect.height,
                },
                text: region.text.clone(),
                style,
                behavior,
            })
        })
        .collect()
}

/// Rasterize a multi-region text layout into a held asset, optionally playing it.
///
/// Like plain text, a layout does not ride the media path — there is nothing to probe
/// or decode — but rendering it materializes a full frame sequence, so it runs under
/// the same ingest slot and frame budget the media path decodes under.
pub async fn show_text_layout(
    engine: &Arc<Engine>,
    regions: &[RegionParam],
    play_now: bool,
) -> Result<(AssetReport, Option<String>), ToolError> {
    // The cheap bounds run before any per-region work and before the ingest slot:
    // a refused shape never gets a conversion pass allocated for it and never
    // occupies a permit or a waiter. render_layout re-checks both behind the
    // typed boundary.
    if regions.is_empty() {
        return Err(matrix_text::layout::LayoutError::NoRegions.into());
    }
    if regions.len() > matrix_text::layout::MAX_REGIONS {
        return Err(matrix_text::layout::LayoutError::TooManyRegions {
            actual: regions.len(),
            limit: matrix_text::layout::MAX_REGIONS,
        }
        .into());
    }

    let specs = layout_specs(regions)?;
    let source_bytes: u64 = specs.iter().map(|spec| spec.text.len() as u64).sum();
    // Held until the sequence is committed, for the reason `show_text` documents:
    // the slot is the aggregate bound on concurrently-materialized sequences.
    let _slot = engine.acquire_decode_slot().await?;
    let sequence = matrix_text::layout::render_layout(
        &specs,
        engine.canvas,
        engine.target_rate,
        text_frame_budget(engine),
    )?;
    hold_text_asset(engine, sequence, source_bytes, play_now).await
}

pub async fn play(
    engine: &Arc<Engine>,
    asset_handle: &str,
    looping: bool,
) -> Result<String, ToolError> {
    Ok(engine.play(asset_handle, looping).await?)
}

pub async fn stop(engine: &Arc<Engine>, playback: Option<&str>) -> Result<String, ToolError> {
    Ok(engine.stop(playback).await?)
}

pub async fn status(engine: &Arc<Engine>) -> StatusReport {
    let playing = engine.playing().await;
    StatusReport {
        playing: playing.as_ref().map(|(p, _)| p.clone()),
        playing_asset: playing.map(|(_, a)| a),
        assets: engine.asset_count().await,
        reported_fps: engine.feedback().latest(),
    }
}

pub async fn set_brightness(engine: &Arc<Engine>, level: u8) -> Result<(), ToolError> {
    Ok(engine.set_brightness(level).await?)
}

pub async fn set_power(engine: &Arc<Engine>, on: bool) -> Result<(), ToolError> {
    Ok(engine.set_power(on).await?)
}

/// How long a caller may treat device capability data as fresh.
///
/// Dimensions, firmware, and the power ceiling change only on a reflash or a settings
/// change, so a short freshness hint would spend requests re-reading constants.
pub const DEVICE_CACHE_TTL: Duration = Duration::from_secs(300);

#[cfg(test)]
mod tests {
    use super::*;

    fn inline(payload: &str, meta: &str) -> FileValue {
        FileValue {
            uri: format!("data:{meta},{payload}"),
            name: None,
            mime_type: None,
            size: None,
        }
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn a_base64_data_uri_resolves_to_its_bytes() {
        let value = inline(&b64(b"hello"), "image/png;base64");
        let resolved = resolve_inline(&value).expect("resolves");
        assert_eq!(resolved.bytes, b"hello");
        assert_eq!(resolved.media_type, "image/png");
    }

    #[test]
    fn a_data_uri_without_a_declared_type_falls_back_to_the_hint() {
        let mut value = inline(&b64(b"x"), ";base64");
        value.mime_type = Some("image/gif".into());
        assert_eq!(
            resolve_inline(&value).expect("resolves").media_type,
            "image/gif"
        );
    }

    #[test]
    fn an_https_uri_is_refused_rather_than_fetched() {
        // Dereferencing a destination the caller named would make this server their
        // user agent; resolving a non-inline reference is an intermediary's job.
        let value = FileValue {
            uri: "https://example.invalid/clip.mp4".into(),
            name: None,
            mime_type: None,
            size: None,
        };
        let err = resolve_inline(&value).expect_err("must not fetch");
        assert_eq!(err.code(), "matrix_unsupported_source");
    }

    #[test]
    fn a_file_uri_is_refused() {
        let value = FileValue {
            uri: "file:///etc/passwd".into(),
            name: None,
            mime_type: None,
            size: None,
        };
        assert_eq!(
            resolve_inline(&value).expect_err("must not read").code(),
            "matrix_unsupported_source"
        );
    }

    #[test]
    fn an_oversized_inline_payload_is_refused_before_decoding() {
        let payload = "A".repeat(MAX_INLINE_BYTES * 2);
        let value = inline(&payload, "application/octet-stream;base64");
        let err = resolve_inline(&value).expect_err("over the inline cap");
        assert_eq!(err.code(), "matrix_inline_too_large");
    }

    #[test]
    fn a_payload_at_the_inline_cap_is_accepted() {
        let value = inline(&b64(&vec![0u8; MAX_INLINE_BYTES]), "image/png;base64");
        let resolved = resolve_inline(&value).expect("at the cap");
        assert_eq!(resolved.bytes.len(), MAX_INLINE_BYTES);
    }

    #[test]
    fn a_non_base64_data_uri_is_refused() {
        let value = inline("plain", "text/plain");
        assert_eq!(
            resolve_inline(&value).expect_err("not base64").code(),
            "matrix_inline_malformed"
        );
    }

    #[test]
    fn malformed_base64_is_reported_rather_than_panicking() {
        let value = inline("!!!not base64!!!", "image/png;base64");
        assert_eq!(
            resolve_inline(&value).expect_err("bad payload").code(),
            "matrix_inline_malformed"
        );
    }

    #[test]
    fn a_data_uri_without_a_comma_is_refused() {
        let value = FileValue {
            uri: "data:image/png;base64".into(),
            name: None,
            mime_type: None,
            size: None,
        };
        assert_eq!(
            resolve_inline(&value).expect_err("no payload").code(),
            "matrix_inline_malformed"
        );
    }

    #[test]
    fn every_wire_string_maps_to_its_own_engine_value() {
        use matrix_text::layout::{Align, Cadence, RegionBehavior, ScrollDirection, ScrollPath};

        // The wire contract for this seam is the exact snake_case strings; pin
        // every variant through the deserializer so a transposition in the match
        // arms or a rename on the serde side fails here rather than on a panel.
        let region = |behavior: serde_json::Value| {
            serde_json::json!({
                "rect": { "x": 0, "y": 0, "width": 64, "height": 16 },
                "text": "A",
                "behavior": behavior
            })
        };
        let spec_for = |behavior: serde_json::Value| {
            let param: RegionParam = serde_json::from_value(region(behavior)).expect("wire shape");
            layout_specs(std::slice::from_ref(&param))
                .expect("converts")
                .remove(0)
        };

        for (wire, align) in [
            ("left", Align::Left),
            ("center", Align::Center),
            ("right", Align::Right),
        ] {
            let spec = spec_for(serde_json::json!({ "type": "fixed", "align": wire }));
            assert_eq!(
                spec.behavior,
                RegionBehavior::Fixed { align },
                "align {wire:?}"
            );
        }

        let paths = [
            ("left_to_right", ScrollPath::LeftToRight),
            ("top_to_bottom", ScrollPath::TopToBottom),
            ("top_left_to_bottom_right", ScrollPath::TopLeftToBottomRight),
            ("bottom_left_to_top_right", ScrollPath::BottomLeftToTopRight),
        ];
        let directions = [
            ("normal", ScrollDirection::Normal),
            ("reverse", ScrollDirection::Reverse),
        ];
        for (path_wire, path) in paths {
            for (direction_wire, direction) in directions {
                let spec = spec_for(serde_json::json!({
                    "type": "scroll",
                    "path": path_wire,
                    "direction": direction_wire,
                    "speed_px_s": 10
                }));
                assert_eq!(
                    spec.behavior,
                    RegionBehavior::Scroll {
                        path,
                        direction,
                        speed_px_s: 10,
                        cadence: Cadence::Once,
                    },
                    "path {path_wire:?} direction {direction_wire:?}"
                );
            }
        }

        // 0.2506 rounds up to 251 thousandths — a truncating conversion would
        // produce 250 and fail here. 0.9999 rounds to 1000 and must saturate at
        // the engine's 999 ceiling, pinning the accepted upper edge below 1.
        for (phase, per_mille) in [(0.2506, 251), (0.9999, 999)] {
            let spec = spec_for(serde_json::json!({
                "type": "scroll",
                "path": "left_to_right",
                "speed_px_s": 10,
                "repeat": true,
                "phase": phase
            }));
            assert_eq!(
                spec.behavior,
                RegionBehavior::Scroll {
                    path: ScrollPath::LeftToRight,
                    direction: ScrollDirection::Normal,
                    speed_px_s: 10,
                    cadence: Cadence::Repeat {
                        phase_per_mille: per_mille
                    },
                },
                "phase {phase}"
            );
        }
    }

    #[test]
    fn cadence_fields_on_the_wrong_shape_are_refused() {
        let parse = |behavior: serde_json::Value| {
            serde_json::from_value::<RegionParam>(serde_json::json!({
                "rect": { "x": 0, "y": 0, "width": 64, "height": 16 },
                "text": "A",
                "behavior": behavior
            }))
        };

        // A phase with nothing repeating has no cycle to offset.
        let err = parse(serde_json::json!({
            "type": "scroll",
            "path": "left_to_right",
            "speed_px_s": 10,
            "phase": 0.5
        }))
        .expect_err("phase without repeat");
        assert!(err.to_string().contains("requires `repeat: true`"), "{err}");

        // Cadence belongs to scrollers, not fixed text.
        let err = parse(serde_json::json!({ "type": "fixed", "repeat": true }))
            .expect_err("repeat on fixed");
        assert!(err.to_string().contains("`type: scroll`"), "{err}");

        // A full cycle of phase is the same as none; refuse rather than wrap.
        let err = parse(serde_json::json!({
            "type": "scroll",
            "path": "left_to_right",
            "speed_px_s": 10,
            "repeat": true,
            "phase": 1.0
        }))
        .expect_err("phase at 1");
        assert!(err.to_string().contains("below 1"), "{err}");
    }

    #[test]
    fn region_style_maps_colors_scale_and_rect_faithfully() {
        let param: RegionParam = serde_json::from_value(serde_json::json!({
            "rect": { "x": 3, "y": 5, "width": 40, "height": 20 },
            "text": "HI",
            "style": { "scale": 1, "foreground": "#00E5FF", "background": "#101010" },
            "behavior": { "type": "fixed" }
        }))
        .expect("wire shape");
        let spec = layout_specs(std::slice::from_ref(&param))
            .expect("converts")
            .remove(0);

        assert_eq!(
            (spec.rect.x, spec.rect.y, spec.rect.width, spec.rect.height),
            (3, 5, 40, 20)
        );
        assert_eq!(spec.style.scale, 1);
        assert_eq!(spec.style.foreground, matrix_frame::Rgb::new(0, 229, 255));
        assert_eq!(spec.style.background, matrix_frame::Rgb::new(16, 16, 16));
    }

    #[test]
    fn a_bad_color_refusal_names_the_region() {
        let good: RegionParam = serde_json::from_value(serde_json::json!({
            "rect": { "x": 0, "y": 0, "width": 32, "height": 16 },
            "text": "A",
            "behavior": { "type": "fixed" }
        }))
        .expect("wire shape");
        let bad: RegionParam = serde_json::from_value(serde_json::json!({
            "rect": { "x": 32, "y": 0, "width": 32, "height": 16 },
            "text": "B",
            "style": { "foreground": "#nothex" },
            "behavior": { "type": "fixed" }
        }))
        .expect("wire shape");

        let err = layout_specs(&[good, bad]).expect_err("refused");
        assert_eq!(err.code(), "matrix_layout_bad_color");
        assert!(
            err.to_string().contains("region 1"),
            "the refusal names the offending region: {err}"
        );
    }

    #[test]
    fn the_published_schema_bounds_track_the_engine_limits() {
        // The wire schema restates engine limits as machine-readable bounds; pinning
        // them to the constants means a raised cap cannot leave the published schema
        // advertising the old number to every client.
        let layout_params =
            serde_json::to_value(schemars::schema_for!(crate::mcp::ShowTextLayoutParams))
                .expect("schema serializes");
        assert_eq!(
            layout_params["properties"]["regions"]["maxItems"],
            serde_json::json!(matrix_text::layout::MAX_REGIONS),
            "the regions bound tracks MAX_REGIONS"
        );
        assert_eq!(
            layout_params["properties"]["regions"]["minItems"],
            serde_json::json!(1)
        );

        // The scale range mirrors the rasterizer's validated 1..=4; matrix-text's
        // own tests pin the refusal at both ends of the same range.
        let style = serde_json::to_value(schemars::schema_for!(RegionStyleParam))
            .expect("schema serializes");
        assert_eq!(
            style["properties"]["scale"]["minimum"],
            serde_json::json!(1)
        );
        assert_eq!(
            style["properties"]["scale"]["maximum"],
            serde_json::json!(4)
        );

        // The published phase window must match what deserialization accepts —
        // half-open [0, 1) — or a schema-validating client cannot submit values
        // the server takes.
        let behavior =
            serde_json::to_value(schemars::schema_for!(BehaviorParam)).expect("schema serializes");
        let phase = behavior["oneOf"]
            .as_array()
            .expect("tagged variants")
            .iter()
            .find_map(|variant| {
                let phase = &variant["properties"]["phase"];
                (!phase.is_null()).then_some(phase)
            })
            .expect("a variant carries phase");
        assert_eq!(phase["minimum"], serde_json::json!(0.0));
        assert_eq!(phase["exclusiveMaximum"], serde_json::json!(1.0));
        assert_eq!(phase["maximum"], serde_json::Value::Null);
    }

    #[test]
    fn an_unknown_or_misplaced_region_field_is_refused_rather_than_ignored() {
        // A typo'd or misplaced key must not silently render the wrong layout:
        // every level of the region object refuses what it does not recognise,
        // including the behavior object, whose hand-written deserializer also
        // refuses known fields on the wrong variant.
        let region = |style: serde_json::Value, behavior: serde_json::Value| {
            serde_json::json!({
                "rect": { "x": 0, "y": 0, "width": 32, "height": 16 },
                "text": "A",
                "style": style,
                "behavior": behavior
            })
        };
        let default_style = serde_json::json!({});
        let cases = [
            (
                "misspelled style key",
                region(
                    serde_json::json!({ "colour": "#ff0000" }),
                    serde_json::json!({ "type": "fixed" }),
                ),
            ),
            (
                "misspelled behavior key",
                region(
                    default_style.clone(),
                    serde_json::json!({ "type": "scroll", "path": "left_to_right",
                                        "directon": "reverse", "speed_px_s": 10 }),
                ),
            ),
            (
                "scroll fields on a fixed region",
                region(
                    default_style.clone(),
                    serde_json::json!({ "type": "fixed", "path": "left_to_right",
                                        "speed_px_s": 10 }),
                ),
            ),
            (
                "align on a scrolling region",
                region(
                    default_style.clone(),
                    serde_json::json!({ "type": "scroll", "path": "left_to_right",
                                        "align": "right", "speed_px_s": 10 }),
                ),
            ),
        ];
        for (case, value) in cases {
            let result: Result<RegionParam, _> = serde_json::from_value(value);
            assert!(result.is_err(), "{case} must be refused");
        }
    }
}
