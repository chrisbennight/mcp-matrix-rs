//! Canvas geometry and the fixed-rate frame representation.
//!
//! Every source format — text, still image, animated GIF, video — normalizes to a
//! [`FrameSequence`]: full-canvas RGB frames at one fixed rate. Playout is therefore a
//! branchless fixed tick, and no timing knowledge from any source format survives past
//! ingest.
//!
//! Pure: no I/O, no async, no device knowledge.

use thiserror::Error;

/// Bytes per pixel on the wire. DDP carries RGB24; there is no white channel on a
/// HUB75 panel.
pub const BYTES_PER_PIXEL: usize = 3;

/// Upper bound on playout rate. Well above anything an ESP32 driving HUB75 sustains,
/// and low enough that a caller-supplied rate cannot produce an absurd frame budget.
pub const MAX_RATE_FPS: u16 = 240;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("canvas dimensions must be non-zero, got {width}x{height}")]
    CanvasDegenerate { width: u16, height: u16 },

    #[error("frame buffer is {actual} bytes, canvas {width}x{height} requires {expected}")]
    BufferSizeMismatch {
        width: u16,
        height: u16,
        expected: usize,
        actual: usize,
    },

    #[error("rate must be between 1 and {MAX_RATE_FPS} fps, got {0}")]
    RateOutOfRange(u16),

    #[error("a frame sequence must contain at least one frame")]
    SequenceEmpty,

    #[error("frame {index} has canvas {actual:?}, sequence canvas is {expected:?}")]
    CanvasMismatch {
        index: usize,
        expected: Canvas,
        actual: Canvas,
    },
}

impl FrameError {
    /// Stable machine-readable code. Callers match on this, never on the message.
    pub fn code(&self) -> &'static str {
        match self {
            Self::CanvasDegenerate { .. } => "canvas_degenerate",
            Self::BufferSizeMismatch { .. } => "frame_buffer_size_mismatch",
            Self::RateOutOfRange(_) => "rate_out_of_range",
            Self::SequenceEmpty => "sequence_empty",
            Self::CanvasMismatch { .. } => "frame_canvas_mismatch",
        }
    }
}

/// Pixel dimensions of the render target.
///
/// Not assumed square or single-panel: the M-1 is 64x64 but ships in multi-panel kits,
/// so a wider canvas is an ordinary configuration rather than a special case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Canvas {
    width: u16,
    height: u16,
}

impl Canvas {
    pub fn new(width: u16, height: u16) -> Result<Self, FrameError> {
        if width == 0 || height == 0 {
            return Err(FrameError::CanvasDegenerate { width, height });
        }
        Ok(Self { width, height })
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// Pixel count.
    pub fn pixels(&self) -> usize {
        usize::from(self.width) * usize::from(self.height)
    }

    /// Byte length of a frame buffer for this canvas.
    pub fn byte_len(&self) -> usize {
        self.pixels() * BYTES_PER_PIXEL
    }

    /// Byte offset of a pixel in row-major order, or `None` if out of bounds.
    ///
    /// Row-major matches both the DDP payload layout and the panel's own pixel order,
    /// so no remapping happens between here and the wire.
    pub fn offset(&self, x: u16, y: u16) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some((usize::from(y) * usize::from(self.width) + usize::from(x)) * BYTES_PER_PIXEL)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0 };

    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// Playout rate in whole frames per second.
///
/// Whole frames because the pump ticks on a fixed interval and a fractional rate buys
/// nothing a caller can perceive on a 64x64 panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rate(u16);

impl Rate {
    pub fn new(fps: u16) -> Result<Self, FrameError> {
        if fps == 0 || fps > MAX_RATE_FPS {
            return Err(FrameError::RateOutOfRange(fps));
        }
        Ok(Self(fps))
    }

    pub fn fps(&self) -> u16 {
        self.0
    }

    /// Interval between ticks, truncated to whole nanoseconds.
    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(1_000_000_000 / u64::from(self.0))
    }
}

/// One full-canvas RGB frame.
///
/// The buffer is always exactly `canvas.byte_len()` bytes; that invariant is what lets
/// the transport hand the slice to the wire without a length check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    canvas: Canvas,
    buffer: Vec<u8>,
}

impl Frame {
    /// A frame with every pixel black.
    pub fn blank(canvas: Canvas) -> Self {
        Self {
            canvas,
            buffer: vec![0; canvas.byte_len()],
        }
    }

    /// Wrap an existing RGB24 buffer, rejecting one that does not match the canvas.
    pub fn from_rgb(canvas: Canvas, buffer: Vec<u8>) -> Result<Self, FrameError> {
        let expected = canvas.byte_len();
        if buffer.len() != expected {
            return Err(FrameError::BufferSizeMismatch {
                width: canvas.width(),
                height: canvas.height(),
                expected,
                actual: buffer.len(),
            });
        }
        Ok(Self { canvas, buffer })
    }

    pub fn canvas(&self) -> Canvas {
        self.canvas
    }

    /// Raw RGB24 bytes in row-major order, ready for the wire.
    pub fn as_rgb(&self) -> &[u8] {
        &self.buffer
    }

    pub fn get(&self, x: u16, y: u16) -> Option<Rgb> {
        let offset = self.canvas.offset(x, y)?;
        Some(Rgb::new(
            self.buffer[offset],
            self.buffer[offset + 1],
            self.buffer[offset + 2],
        ))
    }

    /// Set one pixel. Out-of-bounds coordinates are ignored rather than panicking:
    /// drawing routines clip against the canvas edge constantly and a fallible setter
    /// would push that check into every caller.
    pub fn set(&mut self, x: u16, y: u16, color: Rgb) {
        let Some(offset) = self.canvas.offset(x, y) else {
            return;
        };
        self.buffer[offset] = color.r;
        self.buffer[offset + 1] = color.g;
        self.buffer[offset + 2] = color.b;
    }

    pub fn fill(&mut self, color: Rgb) {
        for chunk in self.buffer.chunks_exact_mut(BYTES_PER_PIXEL) {
            chunk[0] = color.r;
            chunk[1] = color.g;
            chunk[2] = color.b;
        }
    }
}

/// A fixed-rate sequence of frames.
///
/// A still image is a one-frame sequence; a GIF whose own per-frame delays varied is
/// resampled to this single rate at ingest. Nothing downstream can tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSequence {
    canvas: Canvas,
    rate: Rate,
    frames: Vec<Frame>,
}

impl FrameSequence {
    pub fn new(rate: Rate, frames: Vec<Frame>) -> Result<Self, FrameError> {
        let canvas = frames.first().ok_or(FrameError::SequenceEmpty)?.canvas();
        for (index, frame) in frames.iter().enumerate() {
            if frame.canvas() != canvas {
                return Err(FrameError::CanvasMismatch {
                    index,
                    expected: canvas,
                    actual: frame.canvas(),
                });
            }
        }
        Ok(Self {
            canvas,
            rate,
            frames,
        })
    }

    /// A single still frame, held at the given rate.
    pub fn still(rate: Rate, frame: Frame) -> Self {
        Self {
            canvas: frame.canvas(),
            rate,
            frames: vec![frame],
        }
    }

    pub fn canvas(&self) -> Canvas {
        self.canvas
    }

    pub fn rate(&self) -> Rate {
        self.rate
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    pub fn get(&self, index: usize) -> Option<&Frame> {
        self.frames.get(index)
    }

    /// Wall-clock duration of one pass.
    pub fn duration(&self) -> std::time::Duration {
        self.rate.interval() * u32::try_from(self.frames.len()).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas() -> Canvas {
        Canvas::new(64, 64).expect("64x64 is valid")
    }

    fn rate() -> Rate {
        Rate::new(25).expect("25 fps is valid")
    }

    #[test]
    fn canvas_rejects_a_zero_dimension() {
        for (w, h) in [(0, 64), (64, 0), (0, 0)] {
            let err = Canvas::new(w, h).expect_err("degenerate canvas must be rejected");
            assert_eq!(err.code(), "canvas_degenerate");
        }
    }

    #[test]
    fn m1_panel_is_4096_pixels_and_12288_bytes() {
        let canvas = canvas();
        assert_eq!(canvas.pixels(), 4096);
        assert_eq!(canvas.byte_len(), 12_288);
    }

    #[test]
    fn offsets_are_row_major_and_bounded() {
        let canvas = canvas();
        assert_eq!(canvas.offset(0, 0), Some(0));
        assert_eq!(canvas.offset(1, 0), Some(3));
        assert_eq!(canvas.offset(0, 1), Some(64 * 3));
        assert_eq!(canvas.offset(63, 63), Some(12_288 - 3));
        assert_eq!(canvas.offset(64, 0), None);
        assert_eq!(canvas.offset(0, 64), None);
    }

    #[test]
    fn a_wide_multi_panel_canvas_is_ordinary() {
        let canvas = Canvas::new(128, 64).expect("two panels side by side");
        assert_eq!(canvas.pixels(), 8192);
        assert_eq!(canvas.offset(0, 1), Some(128 * 3));
    }

    #[test]
    fn blank_frame_matches_canvas_and_is_black() {
        let frame = Frame::blank(canvas());
        assert_eq!(frame.as_rgb().len(), 12_288);
        assert!(frame.as_rgb().iter().all(|&b| b == 0));
        assert_eq!(frame.get(0, 0), Some(Rgb::BLACK));
    }

    #[test]
    fn from_rgb_rejects_a_mismatched_buffer() {
        let err = Frame::from_rgb(canvas(), vec![0; 12_287])
            .expect_err("a short buffer must be rejected");
        assert_eq!(err.code(), "frame_buffer_size_mismatch");
        assert!(Frame::from_rgb(canvas(), vec![0; 12_288]).is_ok());
    }

    #[test]
    fn set_and_get_round_trip() {
        let mut frame = Frame::blank(canvas());
        let color = Rgb::new(10, 200, 30);
        frame.set(5, 7, color);
        assert_eq!(frame.get(5, 7), Some(color));
        assert_eq!(frame.get(5, 8), Some(Rgb::BLACK));
    }

    #[test]
    fn set_outside_the_canvas_is_ignored_not_fatal() {
        let mut frame = Frame::blank(canvas());
        frame.set(64, 0, Rgb::new(255, 255, 255));
        frame.set(0, 64, Rgb::new(255, 255, 255));
        assert!(frame.as_rgb().iter().all(|&b| b == 0));
        assert_eq!(frame.get(64, 0), None);
    }

    #[test]
    fn fill_covers_every_pixel() {
        let mut frame = Frame::blank(canvas());
        frame.fill(Rgb::new(1, 2, 3));
        assert_eq!(frame.get(0, 0), Some(Rgb::new(1, 2, 3)));
        assert_eq!(frame.get(63, 63), Some(Rgb::new(1, 2, 3)));
        assert_eq!(frame.as_rgb().len(), 12_288);
    }

    #[test]
    fn rate_bounds_are_enforced() {
        assert_eq!(
            Rate::new(0).expect_err("zero fps is not a rate").code(),
            "rate_out_of_range"
        );
        assert_eq!(
            Rate::new(MAX_RATE_FPS + 1)
                .expect_err("above the ceiling is not a rate")
                .code(),
            "rate_out_of_range"
        );
        assert!(Rate::new(1).is_ok());
        assert!(Rate::new(MAX_RATE_FPS).is_ok());
    }

    #[test]
    fn rate_interval_is_the_reciprocal() {
        assert_eq!(
            Rate::new(25).expect("valid").interval(),
            std::time::Duration::from_millis(40)
        );
        assert_eq!(
            Rate::new(1).expect("valid").interval(),
            std::time::Duration::from_secs(1)
        );
    }

    #[test]
    fn sequence_rejects_an_empty_frame_list() {
        let err =
            FrameSequence::new(rate(), Vec::new()).expect_err("an empty sequence is not playable");
        assert_eq!(err.code(), "sequence_empty");
    }

    #[test]
    fn sequence_rejects_a_frame_from_a_different_canvas() {
        let other = Canvas::new(32, 32).expect("valid");
        let frames = vec![Frame::blank(canvas()), Frame::blank(other)];
        let err = FrameSequence::new(rate(), frames).expect_err("mixed canvases must be rejected");
        assert_eq!(err.code(), "frame_canvas_mismatch");
    }

    #[test]
    fn a_still_is_a_one_frame_sequence() {
        let sequence = FrameSequence::still(rate(), Frame::blank(canvas()));
        assert_eq!(sequence.len(), 1);
        assert_eq!(sequence.canvas(), canvas());
        assert_eq!(sequence.duration(), std::time::Duration::from_millis(40));
    }

    #[test]
    fn sequence_duration_is_frame_count_over_rate() {
        let frames = vec![Frame::blank(canvas()); 50];
        let sequence = FrameSequence::new(rate(), frames).expect("uniform canvas");
        assert_eq!(sequence.duration(), std::time::Duration::from_secs(2));
    }
}
