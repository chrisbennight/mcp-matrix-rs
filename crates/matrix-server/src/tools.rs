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
        }
    }
}

/// A reference to media, shaped so an intermediary-minted reference and an inline
/// payload use one contract.
///
/// The shape matches SEP-2631's file object, so adopting that draft later changes how a
/// `uri` is produced and not what this tool accepts.
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
        matrix_media::decode(&source.bytes, None, None, &params, ffmpeg_bin, ffprobe_bin).await?
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

/// Rasterize text into a held asset, optionally playing it at once.
///
/// Text does not ride the media path: it carries no container, so there is nothing to
/// probe or decode, and no subprocess or decode permit is involved.
pub async fn show_text(
    engine: &Arc<Engine>,
    text: &str,
    play_now: bool,
) -> Result<(AssetReport, Option<String>), ToolError> {
    let style = matrix_text::TextStyle::default();
    // The same frame budget the media path decodes under: text obeys the shared
    // ingest limits, and a message that would outrun them is refused before any
    // frame exists.
    let budget = Limits::default()
        .frame_cap_for_frame_size(engine.target_rate.fps(), engine.canvas.byte_len());

    // Text rendering is ingest — it materializes a full frame sequence just as a
    // decode does — so it runs under the same aggregate slot. Without it, concurrent
    // calls could each hold a maximal sequence outside every ceiling while the
    // decode path stays politely bounded next door.
    let _slot = engine.acquire_decode_slot().await?;
    let sequence = matrix_text::render(text, engine.canvas, engine.target_rate, style, budget)?;

    let asset = crate::state::Asset {
        handle: engine.mint_asset_handle(),
        sequence,
        source_bytes: text.len() as u64,
        media_type: "text/plain".into(),
    };
    let report = report_for(&asset);

    // Playback starts before the asset is committed to the store: a failed start —
    // unreachable panel, refused socket — then costs nothing and evicts nothing.
    // A marquee loops until stopped; a still holds anyway under the panel's realtime
    // timeout semantics, so looping is the right default for both shapes.
    let playback = if play_now {
        Some(engine.play_asset(&asset, true).await?)
    } else {
        None
    };

    engine.store_prepared_asset(asset).await;
    Ok((report, playback))
}

/// Characters visible at once on the configured canvas at the default text scale.
pub fn text_visible_chars(engine: &Arc<Engine>) -> usize {
    matrix_text::visible_chars(engine.canvas, matrix_text::TextStyle::default().scale)
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
}
