//! Bounds applied to every ingest.
//!
//! These exist because this crate decodes media supplied by a caller. Decoder formats
//! are complex, and an unbounded decode is a denial-of-service vector even when the
//! decoder is sound: a small input can declare enormous dimensions or duration.
//!
//! Every bound is enforced before or during decode, never only after.

use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LimitError {
    #[error("source is {actual} bytes, limit is {limit}")]
    SourceTooLarge { actual: u64, limit: u64 },

    #[error("source declares {actual:?}, limit is {limit:?}")]
    DurationTooLong { actual: Duration, limit: Duration },

    #[error("source declares {width}x{height}, limit is {limit}x{limit}")]
    DimensionsTooLarge { width: u32, height: u32, limit: u32 },

    #[error("normalized output would be {actual} frames, limit is {limit}")]
    TooManyFrames { actual: u64, limit: u64 },

    #[error("one {frame_bytes}-byte frame exceeds the {limit}-byte normalized ceiling")]
    FrameExceedsMemoryCeiling { frame_bytes: usize, limit: u64 },

    #[error("decode exceeded its {0:?} deadline")]
    DecodeTimeout(Duration),
}

impl LimitError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::SourceTooLarge { .. } => "media_source_too_large",
            Self::DurationTooLong { .. } => "media_duration_too_long",
            Self::DimensionsTooLarge { .. } => "media_dimensions_too_large",
            Self::TooManyFrames { .. } => "media_too_many_frames",
            Self::FrameExceedsMemoryCeiling { .. } => "media_frame_exceeds_memory_ceiling",
            Self::DecodeTimeout(_) => "media_decode_timeout",
        }
    }
}

/// Caps on what a single ingest may consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest accepted source payload.
    pub max_source_bytes: u64,

    /// Longest accepted source. Anything longer is refused rather than truncated,
    /// because silently playing the first N seconds of a caller's video is a worse
    /// outcome than telling them it was too long.
    pub max_duration: Duration,

    /// Largest accepted source dimension on either axis. A source far larger than the
    /// panel is legitimate — everything gets downscaled — but a declared 65535x65535
    /// frame is an allocation attack, not a photograph.
    pub max_source_dimension: u32,

    /// Ceiling on normalized frames held in memory. At 12,288 bytes per 64x64 frame
    /// this is the real memory bound, and it is what `max_duration` and the target rate
    /// have to agree with.
    pub max_frames: u64,

    /// Wall-clock deadline for the whole decode, enforced by killing the subprocess.
    pub decode_timeout: Duration,

    /// Ceiling on normalized bytes the parent will hold at peak.
    ///
    /// `max_frames` alone is not a memory bound: it is a frame count, and a frame's
    /// size follows the canvas. A 256x256 canvas at the default 1,800-frame cap is
    /// about 338 MiB. This bounds the product rather than one factor of it.
    ///
    /// The peak is assembly, where the raw decoder output and the frame sequence are
    /// both live — a `Vec` keeps its allocation as it shrinks, so nothing about the
    /// order of taking frames off it avoids that. The raw output is therefore capped at
    /// half this figure, which is what makes this the real ceiling rather than half of
    /// one.
    pub max_normalized_bytes: u64,

    /// Address-space ceiling applied to the decoder process itself.
    ///
    /// The deadline bounds time and the output ceiling bounds what the parent holds,
    /// but neither constrains the decoder's own allocations. A source declaring an
    /// enormous frame can exhaust the container inside FFmpeg before either check runs,
    /// so the limit is imposed on the child by the kernel rather than by watching it.
    ///
    /// Most of what FFmpeg reserves is a startup cost that does not follow the source.
    /// Worker threads are sized from the host's visible CPU count, and glibc reserves a
    /// 64 MiB arena per thread. Measured across a 3600-fold range of source pixels the
    /// requirement moves by single-digit percent, while going from one visible core to
    /// eight nearly doubles it. A ceiling set to cover only the decode therefore fails
    /// at startup on a wider host instead of refusing the pathological source it exists
    /// for, and it fails for scaled media first, because the scale filter is what adds
    /// the threads.
    ///
    /// The default is sized for an eight-core host with headroom, and is deliberately
    /// well above what any admissible source needs: `max_source_dimension` squared at
    /// three bytes per pixel is under a quarter of it, so a frame far beyond that is
    /// still refused. Operators on wider machines may have to raise it.
    pub decoder_address_space_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_source_bytes: 64 * 1024 * 1024,
            max_duration: Duration::from_secs(60),
            max_source_dimension: 8192,
            max_frames: 1_800,
            decode_timeout: Duration::from_secs(30),
            max_normalized_bytes: 48 * 1024 * 1024,
            decoder_address_space_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

impl Limits {
    pub fn check_source_bytes(&self, actual: u64) -> Result<(), LimitError> {
        if actual > self.max_source_bytes {
            return Err(LimitError::SourceTooLarge {
                actual,
                limit: self.max_source_bytes,
            });
        }
        Ok(())
    }

    pub fn check_duration(&self, actual: Duration) -> Result<(), LimitError> {
        if actual > self.max_duration {
            return Err(LimitError::DurationTooLong {
                actual,
                limit: self.max_duration,
            });
        }
        Ok(())
    }

    pub fn check_dimensions(&self, width: u32, height: u32) -> Result<(), LimitError> {
        if width > self.max_source_dimension || height > self.max_source_dimension {
            return Err(LimitError::DimensionsTooLarge {
                width,
                height,
                limit: self.max_source_dimension,
            });
        }
        Ok(())
    }

    pub fn check_frame_count(&self, actual: u64) -> Result<(), LimitError> {
        if actual > self.max_frames {
            return Err(LimitError::TooManyFrames {
                actual,
                limit: self.max_frames,
            });
        }
        Ok(())
    }

    /// The frame count that actually bounds a decode at `fps`.
    ///
    /// `max_frames` and `max_duration` are separate ceilings and whichever binds first
    /// is the real one. At 25 fps a 60-second limit is 1,500 frames, well under an
    /// 1,800-frame cap, so checking only `max_frames` lets an over-duration source
    /// through as a truncated prefix.
    pub fn effective_frame_cap(&self, fps: u16) -> u64 {
        let from_duration = (self.max_duration.as_secs_f64() * f64::from(fps)).ceil() as u64;
        self.max_frames.min(from_duration)
    }

    /// The frame cap that also respects the byte ceiling for a given frame size.
    ///
    /// Whichever of the three ceilings binds first is the real one, and which that is
    /// depends on the canvas: a large canvas hits bytes long before frames.
    ///
    /// Zero when a single frame already exceeds the ceiling. Rounding up to one there
    /// would let the parent hold two such frames — the raw buffer and the clone
    /// assembly makes — and quietly exceed the bound it documents. A canvas that cannot
    /// fit one frame in the memory budget is a configuration error, and failing loudly
    /// requires either a smaller canvas or a larger ceiling.
    pub fn frame_cap_for_frame_size(&self, fps: u16, frame_bytes: usize) -> u64 {
        if frame_bytes == 0 {
            return self.effective_frame_cap(fps);
        }
        // Half, because assembly holds the raw block and the assembled frames together.
        let by_bytes = (self.max_normalized_bytes / 2) / frame_bytes as u64;
        self.effective_frame_cap(fps).min(by_bytes)
    }

    /// Frames a source of `duration` would produce at `fps`, refused if over any cap.
    ///
    /// Checked before decode starts rather than by counting frames as they arrive, so a
    /// source that would blow the budget never gets to allocate any of it. The frame
    /// size makes the projection byte-aware: on a large canvas the memory ceiling binds
    /// long before the frame count, and a projection that ignored it would admit a
    /// known-duration source the decoder only refuses after accumulating output.
    pub fn projected_frames(
        &self,
        duration: Duration,
        fps: u16,
        frame_bytes: usize,
    ) -> Result<u64, LimitError> {
        self.check_duration(duration)?;
        let projected = (duration.as_secs_f64() * f64::from(fps)).ceil() as u64;
        let cap = self.frame_cap_for_frame_size(fps, frame_bytes);
        if cap == 0 {
            return Err(LimitError::FrameExceedsMemoryCeiling {
                frame_bytes,
                limit: self.max_normalized_bytes,
            });
        }
        if projected > cap {
            return Err(LimitError::TooManyFrames {
                actual: projected,
                limit: cap,
            });
        }
        Ok(projected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_bound_memory_to_something_a_panel_server_can_hold() {
        let limits = Limits::default();
        // 1800 frames x 12,288 bytes for a 64x64 canvas is a little over 22 MiB.
        let worst_case_bytes = limits.max_frames * 12_288;
        assert!(worst_case_bytes < 32 * 1024 * 1024);
    }

    #[test]
    fn an_oversized_source_is_refused_by_byte_count() {
        let limits = Limits::default();
        assert!(limits.check_source_bytes(limits.max_source_bytes).is_ok());
        let err = limits
            .check_source_bytes(limits.max_source_bytes + 1)
            .expect_err("one byte over the cap must be refused");
        assert_eq!(err.code(), "media_source_too_large");
    }

    #[test]
    fn declared_dimensions_are_checked_on_both_axes() {
        let limits = Limits::default();
        assert!(limits.check_dimensions(8192, 8192).is_ok());
        assert_eq!(
            limits
                .check_dimensions(8193, 64)
                .expect_err("wide source refused")
                .code(),
            "media_dimensions_too_large"
        );
        assert_eq!(
            limits
                .check_dimensions(64, 8193)
                .expect_err("tall source refused")
                .code(),
            "media_dimensions_too_large"
        );
    }

    #[test]
    fn a_source_far_larger_than_the_panel_is_still_legitimate() {
        // Downscaling is the entire point; a 4K still must not be refused for being big.
        assert!(Limits::default().check_dimensions(3840, 2160).is_ok());
    }

    #[test]
    fn frame_count_is_projected_before_decode_not_counted_after() {
        let limits = Limits::default();
        assert_eq!(
            limits.projected_frames(Duration::from_secs(10), 25, 12_288),
            Ok(250)
        );
    }

    #[test]
    fn projection_refuses_a_source_that_would_blow_the_frame_budget() {
        let limits = Limits {
            max_duration: Duration::from_secs(600),
            ..Limits::default()
        };
        let err = limits
            .projected_frames(Duration::from_secs(300), 25, 12_288)
            .expect_err("7500 frames is over the 1800 cap");
        assert_eq!(err.code(), "media_too_many_frames");
    }

    #[test]
    fn projection_refuses_an_over_long_source_before_counting_frames() {
        let limits = Limits::default();
        let err = limits
            .projected_frames(Duration::from_secs(61), 1, 12_288)
            .expect_err("over the duration cap");
        // Duration is checked first: at 1 fps the frame count alone would have passed.
        assert_eq!(err.code(), "media_duration_too_long");
    }

    #[test]
    fn the_effective_cap_is_whichever_ceiling_binds_first() {
        let limits = Limits::default();
        // 60 s at 25 fps is 1,500 frames, under the 1,800-frame cap. Checking only
        // max_frames would let an over-duration source through as a 1,500-frame prefix.
        assert_eq!(limits.effective_frame_cap(25), 1_500);
        assert!(limits.effective_frame_cap(25) < limits.max_frames);

        // At a high rate the frame cap binds first instead.
        assert_eq!(limits.effective_frame_cap(120), limits.max_frames);
    }

    #[test]
    fn a_large_canvas_is_bounded_by_bytes_rather_than_by_frame_count() {
        let limits = Limits::default();
        // 64x64 is 12,288 bytes a frame. 24 MiB of raw output is about 2,048 frames, so
        // the duration cap still binds first at 25 fps.
        assert_eq!(limits.frame_cap_for_frame_size(25, 12_288), 1_500);

        // 256x256 is 196,608 bytes a frame; 1,500 of those is about 281 MiB, so the
        // byte ceiling binds instead.
        let large = limits.frame_cap_for_frame_size(25, 196_608);
        assert!(large < 1_500, "byte ceiling must bind, got {large}");
        // Both copies together stay inside the documented peak.
        assert!(large * 196_608 * 2 <= limits.max_normalized_bytes);
    }

    #[test]
    fn a_canvas_too_large_for_the_byte_ceiling_yields_no_budget() {
        // Rounding up to one frame would let the parent hold two — the raw buffer and
        // the clone assembly makes — and exceed the ceiling it documents. Zero is what
        // makes that a loud configuration error instead.
        assert_eq!(Limits::default().frame_cap_for_frame_size(25, 1 << 30), 0);
    }

    #[test]
    fn the_effective_cap_tracks_the_duration_limit() {
        let limits = Limits {
            max_duration: Duration::from_secs(10),
            ..Limits::default()
        };
        assert_eq!(limits.effective_frame_cap(25), 250);
    }

    #[test]
    fn projection_is_bounded_by_bytes_on_a_large_canvas() {
        // 60 s at 25 fps is 1,500 frames — inside the duration and frame caps — but
        // 1,500 frames of a 256x256 canvas is an order of magnitude past the byte
        // budget. The projection must refuse before the decoder produces any of it.
        let limits = Limits::default();
        let err = limits
            .projected_frames(Duration::from_secs(60), 25, 196_608)
            .expect_err("the byte ceiling must bind before decode starts");
        assert_eq!(err.code(), "media_too_many_frames");
    }

    #[test]
    fn projection_rounds_a_partial_frame_up() {
        let limits = Limits::default();
        assert_eq!(
            limits.projected_frames(Duration::from_millis(1500), 25, 12_288),
            Ok(38)
        );
    }
}
