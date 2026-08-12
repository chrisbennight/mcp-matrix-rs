//! Ingest and normalization.
//!
//! Arbitrary caller-supplied media in; one [`matrix_frame::FrameSequence`] out. Every
//! source format is reduced to the same fixed-rate sequence of full-canvas RGB frames,
//! so scheduling and playout never learn what a GIF is.
//!
//! Two properties define this crate:
//!
//! **It is the untrusted boundary.** Media decoders parse complex caller-supplied
//! formats. Video decoding happens in a subprocess with a hard deadline and explicit
//! caps ([`limits`]), never in-process, so a decoder fault costs a subprocess rather
//! than the server.
//!
//! **It absorbs all timing variance.** A GIF's per-frame delays vary within one file;
//! video carries its own rate. Both are resampled to a single rate here, which is what
//! lets the playout pump be a branchless fixed tick.

pub mod decode;
pub mod ffmpeg;
pub mod limits;

pub use decode::{
    DEFAULT_FFMPEG_BIN, Probed, Source, decode, preflight, probe_and_check_dimensions,
};
pub use limits::{LimitError, Limits};

use matrix_frame::{Canvas, Frame, FrameError, FrameSequence, Rate};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("source rejected: {0}")]
    Limit(#[from] LimitError),

    #[error("frame construction failed: {0}")]
    Frame(#[from] FrameError),

    #[error("decoder produced a truncated frame: {remainder} trailing bytes")]
    TruncatedOutput { remainder: usize },

    #[error("decoder produced no frames")]
    NoFrames,

    #[error("decoder failed: {0}")]
    Decoder(String),
}

impl MediaError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Limit(inner) => inner.code(),
            Self::Frame(inner) => inner.code(),
            Self::TruncatedOutput { .. } => "media_truncated_output",
            Self::NoFrames => "media_no_frames",
            Self::Decoder(_) => "media_decoder_failed",
        }
    }
}

/// What a normalized asset should look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NormalizeParams {
    pub canvas: Canvas,
    pub rate: Rate,
    pub limits: Limits,
}

/// Assemble a decoder's raw RGB24 output into a frame sequence.
///
/// Kept separate from the decoder itself so the assembly contract is testable without
/// a decoder present and every decoder implementation feeds the same path.
///
/// Raw video carries no dimensions, so a stream produced for a different canvas is only
/// detectable when its length is not a whole multiple of this canvas's frame size — two
/// 64x64 frames are exactly eight 32x32 frames and would assemble silently. The
/// protection is structural rather than checked: [`ffmpeg::decode_argv`] scales to
/// `params.canvas` and this function assembles with the same value, so the two cannot
/// disagree unless a caller pairs output from one `NormalizeParams` with another.
pub fn sequence_from_raw(
    raw: &[u8],
    params: &NormalizeParams,
) -> Result<FrameSequence, MediaError> {
    let frame_len = params.canvas.byte_len();
    let count = validated_frame_count(raw.len(), params)?;

    // Validation precedes every allocation, so refused input costs nothing beyond the
    // borrow, and accepted input is copied exactly once — per frame, never as a whole
    // intermediate buffer.
    let mut frames = Vec::with_capacity(count);
    for chunk in raw.chunks_exact(frame_len) {
        frames.push(Frame::from_rgb(params.canvas, chunk.to_vec())?);
    }

    FrameSequence::new(params.rate, frames).map_err(MediaError::Frame)
}

/// Assemble a decoder's output, consuming the buffer.
///
/// Assembly holds the raw output and the frame sequence at the same time, and no amount
/// of taking pieces off the buffer changes that: `Vec` keeps its allocation and capacity
/// when it shrinks, so the raw block stays resident until it is dropped whole. The peak
/// is therefore about twice the decoded size, and `max_normalized_bytes` is sized as the
/// budget for that peak rather than for one copy — the raw ceiling is half of it, which
/// is what makes the documented figure the real bound.
pub fn sequence_from_owned(
    raw: Vec<u8>,
    params: &NormalizeParams,
) -> Result<FrameSequence, MediaError> {
    sequence_from_raw(&raw, params)
}

/// Every ceiling on assembly, checked from the length alone so it can run before any
/// byte is copied.
///
/// Both ceilings, not just the frame count: the decoder pre-bounds its own buffer, but
/// the assembly functions are exported and a caller reaching them directly must get the
/// same contract — 1,500 frames of a 256x256 canvas passes the frame cap while being an
/// order of magnitude past the byte budget.
fn validated_frame_count(raw_len: usize, params: &NormalizeParams) -> Result<usize, MediaError> {
    let frame_len = params.canvas.byte_len();
    let remainder = raw_len % frame_len;
    if remainder != 0 {
        return Err(MediaError::TruncatedOutput { remainder });
    }

    let count = raw_len / frame_len;
    if count == 0 {
        return Err(MediaError::NoFrames);
    }

    let cap = params
        .limits
        .frame_cap_for_frame_size(params.rate.fps(), frame_len);
    if cap == 0 {
        return Err(MediaError::Limit(LimitError::FrameExceedsMemoryCeiling {
            frame_bytes: frame_len,
            limit: params.limits.max_normalized_bytes,
        }));
    }
    if count as u64 > cap {
        return Err(MediaError::Limit(LimitError::TooManyFrames {
            actual: count as u64,
            limit: cap,
        }));
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> NormalizeParams {
        NormalizeParams {
            canvas: Canvas::new(64, 64).expect("valid"),
            rate: Rate::new(25).expect("valid"),
            limits: Limits::default(),
        }
    }

    fn raw_frames(n: usize, params: &NormalizeParams) -> Vec<u8> {
        let stride = params.canvas.byte_len();
        (0..n)
            .flat_map(|i| vec![u8::try_from(i % 256).unwrap_or(0); stride])
            .collect()
    }

    #[test]
    fn raw_output_becomes_a_sequence_at_the_requested_rate() {
        let params = params();
        let sequence =
            sequence_from_raw(&raw_frames(50, &params), &params).expect("50 whole frames");

        assert_eq!(sequence.len(), 50);
        assert_eq!(sequence.rate(), params.rate);
        assert_eq!(sequence.canvas(), params.canvas);
        assert_eq!(sequence.duration(), std::time::Duration::from_secs(2));
    }

    #[test]
    fn frame_content_survives_assembly_in_order() {
        let params = params();
        let sequence = sequence_from_raw(&raw_frames(3, &params), &params).expect("three frames");

        assert!(
            sequence
                .get(0)
                .expect("frame 0")
                .as_rgb()
                .iter()
                .all(|&b| b == 0)
        );
        assert!(
            sequence
                .get(1)
                .expect("frame 1")
                .as_rgb()
                .iter()
                .all(|&b| b == 1)
        );
        assert!(
            sequence
                .get(2)
                .expect("frame 2")
                .as_rgb()
                .iter()
                .all(|&b| b == 2)
        );
    }

    #[test]
    fn a_single_frame_source_is_a_valid_sequence() {
        let params = params();
        let sequence = sequence_from_raw(&raw_frames(1, &params), &params).expect("a still");
        assert_eq!(sequence.len(), 1);
    }

    #[test]
    fn a_truncated_decode_is_refused_rather_than_padded() {
        let params = params();
        let mut raw = raw_frames(2, &params);
        raw.truncate(raw.len() - 5);

        let err = sequence_from_raw(&raw, &params).expect_err("a partial frame must not render");
        assert_eq!(err.code(), "media_truncated_output");
    }

    #[test]
    fn an_empty_decode_is_reported_as_no_frames_not_an_empty_sequence() {
        let params = params();
        let err = sequence_from_raw(&[], &params).expect_err("nothing decoded");
        assert_eq!(err.code(), "media_no_frames");
    }

    #[test]
    fn output_over_the_frame_cap_is_refused_even_if_ffmpeg_ignored_its_own_limit() {
        let params = NormalizeParams {
            limits: Limits {
                max_frames: 10,
                ..Limits::default()
            },
            ..params()
        };
        let err = sequence_from_raw(&raw_frames(11, &params), &params)
            .expect_err("the cap is enforced on output, not only on the argv");
        assert_eq!(err.code(), "media_too_many_frames");
    }

    #[test]
    fn output_within_the_frame_cap_but_over_the_byte_ceiling_is_refused() {
        // The frame count alone is not the contract: a large enough canvas exceeds the
        // memory budget while well under max_frames. Five 64x64 frames fit the default
        // frame cap easily; a ceiling sized for four must still refuse them.
        let stride = Canvas::new(64, 64).expect("valid").byte_len() as u64;
        let params = NormalizeParams {
            limits: Limits {
                // Halved by assembly's raw-plus-frames residency, this admits 4 frames.
                max_normalized_bytes: stride * 8,
                ..Limits::default()
            },
            ..params()
        };
        let at_the_ceiling =
            sequence_from_raw(&raw_frames(4, &params), &params).expect("exactly at the ceiling");
        assert_eq!(at_the_ceiling.len(), 4);

        let err = sequence_from_raw(&raw_frames(5, &params), &params)
            .expect_err("the byte ceiling binds before the frame cap");
        assert_eq!(err.code(), "media_too_many_frames");
    }

    #[test]
    fn a_canvas_whose_single_frame_exceeds_the_byte_ceiling_is_refused_loudly() {
        let params = NormalizeParams {
            limits: Limits {
                max_normalized_bytes: 1024,
                ..Limits::default()
            },
            ..params()
        };
        let err = sequence_from_raw(&raw_frames(1, &params), &params)
            .expect_err("one frame alone breaks the budget");
        assert_eq!(err.code(), "media_frame_exceeds_memory_ceiling");
    }

    #[test]
    fn a_canvas_mismatch_is_caught_when_the_stride_does_not_divide() {
        let params = params();
        let raw = raw_frames(2, &params);
        let mismatched = NormalizeParams {
            canvas: Canvas::new(30, 30).expect("valid"),
            ..params
        };
        let err = sequence_from_raw(&raw, &mismatched).expect_err("stride mismatch");
        assert_eq!(err.code(), "media_truncated_output");
    }

    #[test]
    fn a_canvas_mismatch_is_undetectable_when_the_stride_divides_evenly() {
        // Two 64x64 frames are exactly eight 32x32 frames. Raw video carries no
        // dimensions, so nothing in the byte stream distinguishes them. This pins the
        // real limitation rather than pretending a check exists: the guarantee comes
        // from decode_argv and sequence_from_raw sharing one canvas, not from detection.
        let params = params();
        let raw = raw_frames(2, &params);
        let mismatched = NormalizeParams {
            canvas: Canvas::new(32, 32).expect("valid"),
            ..params
        };
        let sequence = sequence_from_raw(&raw, &mismatched).expect("assembles silently");
        assert_eq!(sequence.len(), 8);
    }

    #[test]
    fn the_decoder_argv_and_the_assembler_agree_on_the_canvas() {
        // The structural guarantee the test above says we depend on: a filtergraph built
        // from these params scales to exactly the frame size the assembler expects.
        for canvas in [
            Canvas::new(64, 64).expect("valid"),
            Canvas::new(128, 64).expect("valid"),
            Canvas::new(32, 96).expect("valid"),
        ] {
            let params = NormalizeParams { canvas, ..params() };
            let argv = ffmpeg::decode_argv(params.canvas, params.rate, &params.limits);
            let filter = argv
                .iter()
                .position(|a| a == "-vf")
                .and_then(|i| argv.get(i + 1))
                .expect("filtergraph");

            assert!(filter.contains(&format!("scale={}:{}:", canvas.width(), canvas.height())));

            let one_frame = vec![0u8; ffmpeg::frame_stride(canvas)];
            let sequence = sequence_from_raw(&one_frame, &params).expect("one whole frame");
            assert_eq!(sequence.len(), 1);
            assert_eq!(sequence.canvas(), canvas);
        }
    }
}
