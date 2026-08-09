//! Strings become frame sequences.
//!
//! Text carries no container, so it does not ride the media path: there is nothing to
//! probe, decode, or bound with a subprocess. A string
//! rasterizes directly into the same [`FrameSequence`] every other source normalizes
//! to, so scheduling and playout never learn that these frames spell anything.
//!
//! A string that fits the canvas is one centered still frame. A longer one becomes a
//! marquee: it enters from the right edge and scrolls until it has fully left, one
//! pixel per frame, which at the panel's rates reads at a comfortable pace.
//!
//! The [`layout`] module composes multiple rectangular text regions — fixed or
//! scrolling along eight directions — into one such sequence, for chyron-style
//! displays that mix a fixed headline with moving tickers.

use font8x8::{BASIC_FONTS, UnicodeFonts};
use matrix_frame::{Canvas, Frame, FrameError, FrameSequence, Rate, Rgb};
use thiserror::Error;

pub mod layout;

/// Longest accepted string.
///
/// Bounds the frame count the scroll produces: at the default scale a character is 16
/// pixels wide, so this cap keeps a full marquee within the same order as the media
/// path's frame ceiling rather than minting an hour of frames from one tool call.
pub const MAX_TEXT_CHARS: usize = 100;

/// Glyph cell edge in the source font.
pub(crate) const GLYPH_PX: usize = 8;

#[derive(Debug, Error)]
pub enum TextError {
    #[error("text is empty")]
    Empty,

    #[error("text is {actual} characters, limit is {limit}")]
    TooLong { actual: usize, limit: usize },

    #[error("text would render {frames} frames, budget is {budget}")]
    OverBudget { frames: u64, budget: u64 },

    #[error("scale {0} is outside 1..=4")]
    BadScale(u8),

    #[error("frame construction failed: {0}")]
    Frame(#[from] FrameError),
}

impl TextError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Empty => "matrix_text_empty",
            Self::TooLong { .. } => "matrix_text_too_long",
            Self::OverBudget { .. } => "matrix_text_over_budget",
            Self::BadScale(_) => "matrix_text_bad_scale",
            Self::Frame(inner) => inner.code(),
        }
    }
}

/// Characters visible at once on `canvas` at `scale`.
///
/// The pre-render visibility contract: a caller can size a message to fit as a still,
/// or know how much of a marquee shows at any instant, without rendering anything.
pub fn visible_chars(canvas: Canvas, scale: u8) -> usize {
    let cell = GLYPH_PX * usize::from(scale.max(1));
    usize::from(canvas.width()) / cell
}

/// The longest string that renders within `frame_budget` on `canvas` at `scale`.
///
/// Text at or under [`visible_chars`] is a single still and always fits; past that,
/// each character adds one glyph cell of one-pixel scroll steps.
pub fn max_chars_within_budget(canvas: Canvas, scale: u8, frame_budget: u64) -> usize {
    let cell = (GLYPH_PX * usize::from(scale.max(1))) as u64;
    let width = u64::from(canvas.width());
    let visible = visible_chars(canvas, scale.max(1));
    // A marquee of n characters is `width + n*cell` frames.
    let scrollable = frame_budget
        .saturating_sub(width)
        .checked_div(cell)
        .unwrap_or(0) as usize;
    scrollable.max(visible).min(MAX_TEXT_CHARS)
}

/// How the text is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextStyle {
    pub foreground: Rgb,
    pub background: Rgb,
    /// Integer glyph magnification. 2 puts a 16-pixel-tall line on a 64-pixel panel,
    /// which is what reads comfortably at across-the-room distance.
    pub scale: u8,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            foreground: Rgb::new(255, 255, 255),
            background: Rgb::new(0, 0, 0),
            scale: 2,
        }
    }
}

/// Rasterize `text` into a sequence at `rate`.
///
/// Characters outside the font's coverage render as `?` rather than being refused: a
/// caller's emoji costs one visible placeholder, not the whole message.
pub fn render(
    text: &str,
    canvas: Canvas,
    rate: Rate,
    style: TextStyle,
    frame_budget: u64,
) -> Result<FrameSequence, TextError> {
    let glyphs = validated_glyphs(text, style.scale)?;

    let scale = style.scale as i32;
    let cell = GLYPH_PX as i32 * scale;
    let text_width = glyphs.len() as i32 * cell;
    let width = i32::from(canvas.width());
    // Vertical centering leans up when the leftover is odd; the panel's rows are few
    // enough that a consistent choice matters more than which one.
    let top = (i32::from(canvas.height()) - cell) / 2;

    // The same budget discipline the media path applies before decoding: a message
    // whose marquee outruns the frame budget is refused before any frame exists.
    let projected = if text_width <= width {
        1u64
    } else {
        (width + text_width) as u64
    };
    if projected > frame_budget {
        return Err(TextError::OverBudget {
            frames: projected,
            budget: frame_budget,
        });
    }

    let frames: Vec<Frame> = if text_width <= width {
        // A still: centered, one frame. The sequence's rate is irrelevant to a single
        // frame but keeps the type honest about how it would advance.
        vec![compose(
            canvas,
            &glyphs,
            (width - text_width) / 2,
            top,
            style,
        )]
    } else {
        // A marquee: enter from the right edge, leave to the left, one pixel per
        // frame. The pixel step is deliberate — at 15 to 25 send fps this reads as a
        // steady crawl, and a faster step under-samples glyph strokes into shimmer.
        let steps = width + text_width;
        (0..steps)
            .map(|step| compose(canvas, &glyphs, width - step, top, style))
            .collect()
    };

    FrameSequence::new(rate, frames).map_err(TextError::Frame)
}

/// One frame with the glyph run drawn at `(left, top)`.
fn compose(
    canvas: Canvas,
    glyphs: &[[u8; GLYPH_PX]],
    left: i32,
    top: i32,
    style: TextStyle,
) -> Frame {
    let mut frame = Frame::blank(canvas);
    if style.background != Rgb::new(0, 0, 0) {
        frame.fill(style.background);
    }
    let clip = Clip {
        x0: 0,
        y0: 0,
        x1: i32::from(canvas.width()),
        y1: i32::from(canvas.height()),
    };
    draw_glyph_run(&mut frame, clip, glyphs, left, top, style);
    frame
}

/// Half-open pixel bounds a glyph run may light.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Clip {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

/// Draw a glyph run at `(left, top)` into `frame`, lighting only pixels inside `clip`.
///
/// Pixels outside the clip are skipped silently: a scrolling run is partly outside
/// its bounds for most of its life, and clipping at draw time is what motion means.
/// Work scales with the glyphs actually visible, not the run's length: the visible
/// index window is computed once, and row and column extents are clamped against the
/// clip before the per-pixel loops, so a run parked outside its bounds costs two
/// comparisons per frame no matter how long its text is.
///
/// Preconditions: `style.scale >= 1` (the window arithmetic divides by the cell
/// size) and a non-empty clip. Both callers route through [`validated_glyphs`] and
/// validated rects, which is what makes the divisions and the sign of the window
/// arithmetic safe.
pub(crate) fn draw_glyph_run(
    frame: &mut Frame,
    clip: Clip,
    glyphs: &[[u8; GLYPH_PX]],
    left: i32,
    top: i32,
    style: TextStyle,
) {
    debug_assert!(style.scale >= 1, "scale is validated before drawing");
    debug_assert!(
        clip.x0 < clip.x1 && clip.y0 < clip.y1,
        "the clip comes from a validated, non-degenerate rect"
    );
    let scale = i32::from(style.scale);
    let cell = GLYPH_PX as i32 * scale;
    // Entirely outside on either axis: nothing to draw.
    if top + cell <= clip.y0 || top >= clip.y1 {
        return;
    }
    let run_width = glyphs.len() as i32 * cell;
    if left + run_width <= clip.x0 || left >= clip.x1 {
        return;
    }

    // Only glyphs whose cells intersect the clip horizontally.
    let first = if left >= clip.x0 {
        0
    } else {
        ((clip.x0 - left) / cell) as usize
    };
    let last = ((clip.x1 - left + cell - 1) / cell).min(glyphs.len() as i32) as usize;

    // Vertical extent shared by every glyph in the run, clamped to the clip.
    let clamp_span =
        |base: i32, lo: i32, hi: i32| (lo.max(base) - base, hi.min(base + scale) - base);

    for (index, rows) in glyphs.iter().enumerate().take(last).skip(first) {
        let glyph_left = left + index as i32 * cell;
        for (row, bits) in rows.iter().enumerate() {
            let y_base = top + row as i32 * scale;
            let (dy0, dy1) = clamp_span(y_base, clip.y0, clip.y1);
            if dy0 >= dy1 {
                continue;
            }
            for column in 0..GLYPH_PX {
                if bits & (1 << column) == 0 {
                    continue;
                }
                let x_base = glyph_left + column as i32 * scale;
                let (dx0, dx1) = clamp_span(x_base, clip.x0, clip.x1);
                if dx0 >= dx1 {
                    continue;
                }
                for dy in dy0..dy1 {
                    for dx in dx0..dx1 {
                        // The clamps prove the coordinates non-negative and inside
                        // the clip; `set` still guards the canvas edge, so a clip
                        // wider than the frame clips there rather than writing out
                        // of bounds.
                        if let (Ok(x), Ok(y)) =
                            (u16::try_from(x_base + dx), u16::try_from(y_base + dy))
                        {
                            frame.set(x, y, style.foreground);
                        }
                    }
                }
            }
        }
    }
}

/// The font's cell for a character, or the placeholder for one it cannot draw.
pub(crate) fn glyph_for(ch: char) -> [u8; GLYPH_PX] {
    BASIC_FONTS
        .get(ch)
        .unwrap_or_else(|| BASIC_FONTS.get('?').expect("the font covers ASCII"))
}

/// Validate text and scale, resolving the glyph run.
///
/// The single gate both rasterizers pass through, so the accepted character count
/// and scale range cannot drift between the plain-text and layout paths. Checks run
/// before the glyph vector is allocated, so refused text costs a scan of the string
/// it arrived in and nothing more.
pub(crate) fn validated_glyphs(text: &str, scale: u8) -> Result<Vec<[u8; GLYPH_PX]>, TextError> {
    if text.is_empty() {
        return Err(TextError::Empty);
    }
    let count = text.chars().count();
    if count > MAX_TEXT_CHARS {
        return Err(TextError::TooLong {
            actual: count,
            limit: MAX_TEXT_CHARS,
        });
    }
    if !(1..=4).contains(&scale) {
        return Err(TextError::BadScale(scale));
    }
    Ok(text.chars().map(glyph_for).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas() -> Canvas {
        Canvas::new(64, 64).expect("valid")
    }

    fn rate() -> Rate {
        Rate::new(25).expect("valid")
    }

    #[test]
    fn text_that_fits_is_one_centered_still_frame() {
        let sequence =
            render("HI", canvas(), rate(), TextStyle::default(), 1_500).expect("renders");
        assert_eq!(sequence.len(), 1);

        let frame = sequence.get(0).expect("frame");
        let lit: Vec<usize> = frame
            .as_rgb()
            .chunks(3)
            .enumerate()
            .filter(|(_, px)| px.iter().any(|&b| b > 0))
            .map(|(i, _)| i)
            .collect();
        assert!(!lit.is_empty(), "the glyphs must light pixels");

        // Two characters at scale 2 are 32 px wide on a 64 px canvas: nothing may be
        // drawn in the outer 16-px margins, which is what centering means.
        assert!(
            lit.iter().all(|&i| {
                let x = i % 64;
                (16..48).contains(&x)
            }),
            "a centered still must keep its pixels in the middle band"
        );
    }

    #[test]
    fn text_wider_than_the_canvas_scrolls_through_completely() {
        let text = "THE QUICK BROWN FOX";
        let sequence =
            render(text, canvas(), rate(), TextStyle::default(), 1_500).expect("renders");

        // One frame per pixel of travel: enter from the right edge, leave to the left.
        let expected = 64 + text.chars().count() * 16;
        assert_eq!(sequence.len(), expected);

        // First frame: text has not entered yet.
        assert!(
            sequence
                .get(0)
                .expect("first")
                .as_rgb()
                .iter()
                .all(|&b| b == 0),
            "at step zero the text is still off the right edge"
        );
        // Middle frame: something is on screen.
        assert!(
            sequence
                .get(expected / 2)
                .expect("middle")
                .as_rgb()
                .iter()
                .any(|&b| b > 0)
        );
        // Last frame: the text has fully left.
        assert!(
            sequence
                .get(expected - 1)
                .expect("last")
                .as_rgb()
                .iter()
                .all(|&b| b == 0),
            "on the final step the text has scrolled off the left edge"
        );
    }

    #[test]
    fn an_unsupported_character_renders_as_the_placeholder() {
        let emoji = render("🎨", canvas(), rate(), TextStyle::default(), 1_500).expect("renders");
        let question = render("?", canvas(), rate(), TextStyle::default(), 1_500).expect("renders");
        assert_eq!(
            emoji.get(0).expect("frame").as_rgb(),
            question.get(0).expect("frame").as_rgb(),
            "an undrawable character costs a visible placeholder, not the message"
        );
    }

    #[test]
    fn empty_text_is_refused() {
        let err = render("", canvas(), rate(), TextStyle::default(), 1_500).expect_err("empty");
        assert_eq!(err.code(), "matrix_text_empty");
    }

    #[test]
    fn over_long_text_is_refused_rather_than_minting_an_hour_of_frames() {
        let text = "x".repeat(MAX_TEXT_CHARS + 1);
        let err =
            render(&text, canvas(), rate(), TextStyle::default(), 1_500).expect_err("too long");
        assert_eq!(err.code(), "matrix_text_too_long");
    }

    #[test]
    fn the_frame_budget_binds_before_the_character_cap() {
        // The longest string the budget admits renders; one more character is refused
        // with the budget code, exactly as an over-budget media source would be.
        let style = TextStyle::default();
        let longest = max_chars_within_budget(canvas(), style.scale, 1_500);
        assert!(
            longest < MAX_TEXT_CHARS,
            "the budget is the binding cap here"
        );

        let fits = "x".repeat(longest);
        let sequence = render(&fits, canvas(), rate(), style, 1_500).expect("renders");
        assert!(sequence.len() as u64 <= 1_500);

        let over = "x".repeat(longest + 1);
        let err = render(&over, canvas(), rate(), style, 1_500).expect_err("over budget");
        assert_eq!(err.code(), "matrix_text_over_budget");
    }

    #[test]
    fn visibility_is_knowable_without_rendering() {
        // 64 px wide at scale 2 shows four 16-px cells.
        assert_eq!(visible_chars(canvas(), 2), 4);
        assert_eq!(visible_chars(canvas(), 1), 8);
        // Anything at or under the visible width is a still and always fits a budget
        // of at least one frame's worth.
        assert!(max_chars_within_budget(canvas(), 2, 1_500) >= visible_chars(canvas(), 2));
    }

    #[test]
    fn a_zero_or_oversized_scale_is_refused() {
        for scale in [0u8, 5] {
            let style = TextStyle {
                scale,
                ..TextStyle::default()
            };
            let err = render("x", canvas(), rate(), style, 1_500).expect_err("bad scale");
            assert_eq!(err.code(), "matrix_text_bad_scale");
        }
    }

    #[test]
    fn foreground_and_background_colours_are_honoured() {
        let style = TextStyle {
            foreground: Rgb::new(255, 0, 0),
            background: Rgb::new(0, 0, 255),
            scale: 2,
        };
        let sequence = render("A", canvas(), rate(), style, 1_500).expect("renders");
        let rgb = sequence.get(0).expect("frame").as_rgb().to_vec();
        let has_fg = rgb.chunks(3).any(|px| px == [255, 0, 0]);
        let has_bg = rgb.chunks(3).any(|px| px == [0, 0, 255]);
        assert!(has_fg, "glyph pixels take the foreground colour");
        assert!(has_bg, "everything else takes the background colour");
    }
}
