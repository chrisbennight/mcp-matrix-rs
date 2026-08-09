//! Multi-region text layouts.
//!
//! A layout composes rectangular text regions — some fixed, some scrolling — into one
//! bounded, fixed-rate [`FrameSequence`]. Downstream, the result is indistinguishable
//! from any other sequence: scheduling and playout never learn that the frames carry a
//! layout, and the whole package materializes here before playout sees a single frame.
//!
//! Every scroller starts at frame zero fully outside its entry edge, crosses its
//! rectangle, and exits completely. The longest scroller sets the package length; a
//! shorter one parks outside its destination edge for the remainder, so looping the
//! package restarts every region together. Because both endpoints are fully outside,
//! a looped package shows one blank beat — the exited final frame followed by the
//! not-yet-entered first frame — at each wrap, which is what makes the restart
//! unambiguous rather than a mid-glyph jump.

use crate::{Clip, GLYPH_PX, TextError, TextStyle, draw_glyph_run, validated_glyphs};
use matrix_frame::{Canvas, Frame, FrameError, FrameSequence, Rate, Rgb};
use thiserror::Error;

/// Most regions one layout may hold.
///
/// Bounds validation work (overlap is a pairwise check) and keeps a layout legible on
/// a panel: even a large multi-panel canvas has no room for more distinct text areas.
pub const MAX_REGIONS: usize = 16;

/// A region's bounds on the canvas, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// Horizontal placement of fixed text inside its rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    Left,
    #[default]
    Center,
    Right,
}

/// The canonical scroll paths. `Reverse` retraces the named path backwards; glyphs
/// stay upright either way, only the block's position changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollPath {
    LeftToRight,
    TopToBottom,
    TopLeftToBottomRight,
    BottomLeftToTopRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollDirection {
    #[default]
    Normal,
    Reverse,
}

/// How a region's text behaves over the package timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionBehavior {
    /// Drawn once, visible in every frame.
    Fixed { align: Align },
    /// Enters fully outside one edge, crosses the rectangle, exits the far edge.
    ///
    /// `speed_px_s` is pixels per second along the dominant axis of travel, sampled
    /// into the sequence's fixed rate and capped at the rate itself, so consecutive
    /// frames never skip a pixel of travel. On a diagonal path the on-screen speed
    /// along the hypotenuse is accordingly up to √2 faster than the named figure,
    /// and because both axes advance together, text wider than the rectangle is
    /// revealed as a moving band across the block rather than whole characters at
    /// a time — the shorter the rectangle relative to the text, the narrower the
    /// band.
    Scroll {
        path: ScrollPath,
        direction: ScrollDirection,
        speed_px_s: u16,
    },
}

/// One text region of a layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionSpec {
    pub rect: Rect,
    pub text: String,
    pub style: TextStyle,
    pub behavior: RegionBehavior,
}

#[derive(Debug, Error)]
pub enum LayoutError {
    #[error("layout has no regions")]
    NoRegions,

    #[error("layout has {actual} regions, limit is {limit}")]
    TooManyRegions { actual: usize, limit: usize },

    #[error("region {region} has a zero-size rectangle")]
    RectDegenerate { region: usize },

    #[error("region {region} extends beyond the canvas")]
    RectOutOfBounds { region: usize },

    #[error("region {region} rejected: {source}")]
    Text { region: usize, source: TextError },

    #[error("region {region}: {color:?} is not a #rrggbb color")]
    BadColor { region: usize, color: String },

    #[error("region {region}: fixed text does not fit its rectangle")]
    FixedOverflow { region: usize },

    #[error(
        "region {region}: scrolling text overflows its rectangle on the axis it does not travel"
    )]
    ScrollOverflow { region: usize },

    #[error("region {region}: scroll speed {speed} is outside 1..={limit}")]
    BadSpeed {
        region: usize,
        speed: u16,
        limit: u16,
    },

    #[error("regions {first} and {second} overlap")]
    Overlap { first: usize, second: usize },

    #[error("layout would render {frames} frames, budget is {budget}")]
    OverBudget { frames: u64, budget: u64 },

    #[error("frame construction failed: {0}")]
    Frame(#[from] FrameError),
}

impl LayoutError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoRegions => "matrix_layout_no_regions",
            Self::TooManyRegions { .. } => "matrix_layout_too_many_regions",
            Self::RectDegenerate { .. } => "matrix_layout_rect_degenerate",
            Self::RectOutOfBounds { .. } => "matrix_layout_rect_out_of_bounds",
            Self::Text { source, .. } => source.code(),
            Self::BadColor { .. } => "matrix_layout_bad_color",
            Self::FixedOverflow { .. } => "matrix_layout_fixed_overflow",
            Self::ScrollOverflow { .. } => "matrix_layout_scroll_overflow",
            Self::BadSpeed { .. } => "matrix_layout_bad_speed",
            Self::Overlap { .. } => "matrix_layout_overlap",
            Self::OverBudget { .. } => "matrix_layout_over_budget",
            Self::Frame(inner) => inner.code(),
        }
    }
}

/// Parse a `#rrggbb` color, case-insensitive.
///
/// Exactly this shape and nothing else: shorthand `#rgb`, named colors, and alpha
/// channels are refused rather than guessed at, so a typo surfaces as a refusal
/// instead of an unexpected hue on the panel. Returns `None` for anything else; the
/// caller owns the refusal and the region index it reports.
pub fn parse_color(value: &str) -> Option<Rgb> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(Rgb::new(channel(0)?, channel(2)?, channel(4)?))
}

/// A validated region, ready to draw: glyphs resolved, geometry in signed pixels.
struct Prepared {
    clip: Clip,
    glyphs: Vec<[u8; GLYPH_PX]>,
    style: TextStyle,
    kind: PreparedKind,
}

enum PreparedKind {
    Fixed {
        left: i32,
        top: i32,
    },
    Scroll {
        /// Start and delta always describe the path's normal direction; `reverse`
        /// mirrors the sampled progress instead of negating the delta, so both
        /// directions feed identical arguments through the rounding division. When
        /// the speed divides the span evenly the reverse package is the normal one
        /// frame-reversed; otherwise the final partial step offsets the reverse
        /// grid by under one pixel of progress.
        start_x: i64,
        start_y: i64,
        dx: i64,
        dy: i64,
        /// Dominant-axis travel scaled by the frame rate: `travel * fps`. Progress
        /// is measured against this, so positions stay exact rational samples.
        span: i64,
        /// Frames this scroller needs to fully enter, cross, and exit.
        frames: u64,
        speed: i64,
        reverse: bool,
    },
}

/// Signed division rounding half away from zero.
fn round_div(numerator: i64, denominator: i64) -> i64 {
    debug_assert!(denominator > 0);
    if numerator >= 0 {
        (numerator + denominator / 2) / denominator
    } else {
        -((-numerator + denominator / 2) / denominator)
    }
}

/// Rasterize a layout into a sequence at `rate`.
///
/// Everything is validated and the package length projected before any frame is
/// allocated, so a refused layout costs a scan of its regions and nothing more.
pub fn render_layout(
    regions: &[RegionSpec],
    canvas: Canvas,
    rate: Rate,
    frame_budget: u64,
) -> Result<FrameSequence, LayoutError> {
    if regions.is_empty() {
        return Err(LayoutError::NoRegions);
    }
    if regions.len() > MAX_REGIONS {
        return Err(LayoutError::TooManyRegions {
            actual: regions.len(),
            limit: MAX_REGIONS,
        });
    }

    let mut prepared = Vec::with_capacity(regions.len());
    for (index, region) in regions.iter().enumerate() {
        prepared.push(prepare_region(index, region, canvas, rate)?);
    }

    // Overlap is checked over the raw rectangles after each region is individually
    // valid, so the reported pair always names two well-formed regions.
    for first in 0..regions.len() {
        for second in first + 1..regions.len() {
            if rects_overlap(regions[first].rect, regions[second].rect) {
                return Err(LayoutError::Overlap { first, second });
            }
        }
    }

    // The longest scroller sets the package length; an all-fixed layout is a still.
    let package_frames = prepared
        .iter()
        .filter_map(|p| match p.kind {
            PreparedKind::Scroll { frames, .. } => Some(frames),
            PreparedKind::Fixed { .. } => None,
        })
        .max()
        .unwrap_or(1);
    if package_frames > frame_budget {
        return Err(LayoutError::OverBudget {
            frames: package_frames,
            budget: frame_budget,
        });
    }

    // The template carries everything the timeline never changes: region backgrounds
    // and fixed text. Scrollers redraw on top of a clone each frame.
    let mut template = Frame::blank(canvas);
    for (region, prep) in regions.iter().zip(&prepared) {
        if prep.style.background != Rgb::BLACK {
            fill_rect(&mut template, region.rect, prep.style.background);
        }
    }
    for prep in &prepared {
        if let PreparedKind::Fixed { left, top } = prep.kind {
            draw_glyph_run(
                &mut template,
                prep.clip,
                &prep.glyphs,
                left,
                top,
                prep.style,
            );
        }
    }

    let has_scroller = prepared
        .iter()
        .any(|p| matches!(p.kind, PreparedKind::Scroll { .. }));
    if !has_scroller {
        return Ok(FrameSequence::still(rate, template));
    }

    // A frame's bytes are fully determined by the template plus each scroller's
    // rounded position. At speeds below the frame rate a position repeats for
    // several consecutive frames, so those frames are clones of their predecessor
    // and skip the draw pass entirely.
    let mut frames: Vec<Frame> = Vec::with_capacity(package_frames as usize);
    let mut last_positions: Vec<(i64, i64)> = Vec::new();
    for i in 0..package_frames {
        let mut positions = Vec::with_capacity(prepared.len());
        for prep in &prepared {
            let PreparedKind::Scroll {
                start_x,
                start_y,
                dx,
                dy,
                span,
                speed,
                reverse,
                ..
            } = prep.kind
            else {
                continue;
            };
            // Progress is clamped at full travel: a scroller done before the
            // package ends holds its final, fully-exited position, which draws
            // nothing — parked outside its destination edge. Reverse mirrors
            // the progress, entering at the normal path's exit.
            let forward = (i as i64 * speed).min(span);
            let prog = if reverse { span - forward } else { forward };
            let left = start_x + round_div(prog * dx, span);
            let top = start_y + round_div(prog * dy, span);
            positions.push((left, top));
        }

        if i > 0 && positions == last_positions {
            let previous = frames.last().expect("a prior frame exists").clone();
            frames.push(previous);
            continue;
        }

        let mut frame = template.clone();
        let mut drawn = positions.iter();
        for prep in &prepared {
            if !matches!(prep.kind, PreparedKind::Scroll { .. }) {
                continue;
            }
            let &(left, top) = drawn.next().expect("one position per scroller");
            draw_glyph_run(
                &mut frame,
                prep.clip,
                &prep.glyphs,
                left as i32,
                top as i32,
                prep.style,
            );
        }
        frames.push(frame);
        last_positions = positions;
    }

    FrameSequence::new(rate, frames).map_err(LayoutError::Frame)
}

fn prepare_region(
    index: usize,
    region: &RegionSpec,
    canvas: Canvas,
    rate: Rate,
) -> Result<Prepared, LayoutError> {
    let rect = region.rect;
    if rect.width == 0 || rect.height == 0 {
        return Err(LayoutError::RectDegenerate { region: index });
    }
    if u32::from(rect.x) + u32::from(rect.width) > u32::from(canvas.width())
        || u32::from(rect.y) + u32::from(rect.height) > u32::from(canvas.height())
    {
        return Err(LayoutError::RectOutOfBounds { region: index });
    }

    let glyphs =
        validated_glyphs(&region.text, region.style.scale).map_err(|source| LayoutError::Text {
            region: index,
            source,
        })?;
    let scale = i64::from(region.style.scale);
    let cell = GLYPH_PX as i64 * scale;
    let text_width = glyphs.len() as i64 * cell;
    let text_height = cell;

    let rx = i64::from(rect.x);
    let ry = i64::from(rect.y);
    let rw = i64::from(rect.width);
    let rh = i64::from(rect.height);
    let clip = Clip {
        x0: rect.x as i32,
        y0: rect.y as i32,
        x1: (rx + rw) as i32,
        y1: (ry + rh) as i32,
    };

    // Centering on the axis perpendicular to travel leans toward the origin when the
    // leftover is odd, matching the single-text path's convention.
    let hmid = rx + (rw - text_width) / 2;
    let vmid = ry + (rh - text_height) / 2;

    let kind = match region.behavior {
        RegionBehavior::Fixed { align } => {
            // Fixed text must genuinely fit: unlike a scroller, it has no motion to
            // eventually reveal what a clipped edge hides.
            if text_width > rw || text_height > rh {
                return Err(LayoutError::FixedOverflow { region: index });
            }
            let left = match align {
                Align::Left => rx,
                Align::Center => hmid,
                Align::Right => rx + rw - text_width,
            };
            PreparedKind::Fixed {
                left: left as i32,
                top: vmid as i32,
            }
        }
        RegionBehavior::Scroll {
            path,
            direction,
            speed_px_s,
        } => {
            // Motion reveals clipping only along the axis of travel; on the
            // perpendicular axis a scroller is as blind as fixed text, so overflow
            // there would be cropped in every frame and is refused instead. A
            // diagonal ties both axes to one progress scalar: text taller than the
            // rectangle loses whole glyph rows of its end characters permanently,
            // so the height bound applies there too. Width overflow is what
            // scrolling exists to reveal and stays legal on every path — though on
            // a diagonal, long text is revealed as a moving band rather than the
            // whole block at once.
            let overflows = match path {
                ScrollPath::LeftToRight
                | ScrollPath::TopLeftToBottomRight
                | ScrollPath::BottomLeftToTopRight => text_height > rh,
                ScrollPath::TopToBottom => text_width > rw,
            };
            if overflows {
                return Err(LayoutError::ScrollOverflow { region: index });
            }
            // Capped at the frame rate so the step never exceeds one pixel per
            // frame — the same pacing the plain marquee fixes by construction; a
            // faster step under-samples glyph strokes into shimmer.
            let max_speed = rate.fps();
            if !(1..=max_speed).contains(&speed_px_s) {
                return Err(LayoutError::BadSpeed {
                    region: index,
                    speed: speed_px_s,
                    limit: max_speed,
                });
            }
            // Start fully outside the entry edge; the delta carries the block fully
            // past the far edge, so frame zero and the final frame draw nothing.
            // Reverse is handled at sample time by mirroring progress, so the
            // table always describes the normal direction.
            let (start, delta) = match path {
                ScrollPath::LeftToRight => ((rx - text_width, vmid), (rw + text_width, 0)),
                ScrollPath::TopToBottom => ((hmid, ry - text_height), (0, rh + text_height)),
                ScrollPath::TopLeftToBottomRight => (
                    (rx - text_width, ry - text_height),
                    (rw + text_width, rh + text_height),
                ),
                ScrollPath::BottomLeftToTopRight => (
                    (rx - text_width, ry + rh),
                    (rw + text_width, -(rh + text_height)),
                ),
            };
            let travel = delta.0.abs().max(delta.1.abs());
            let speed = i64::from(speed_px_s);
            let span = travel * i64::from(rate.fps());
            // Ceil covers the last partial step; the +1 is the fully-exited frame at
            // clamped progress, so the block never vanishes mid-rectangle.
            let frames = ((span + speed - 1) / speed) as u64 + 1;
            PreparedKind::Scroll {
                start_x: start.0,
                start_y: start.1,
                dx: delta.0,
                dy: delta.1,
                span,
                frames,
                speed,
                reverse: matches!(direction, ScrollDirection::Reverse),
            }
        }
    };

    Ok(Prepared {
        clip,
        glyphs,
        style: region.style,
        kind,
    })
}

fn rects_overlap(a: Rect, b: Rect) -> bool {
    let (ax1, ay1) = (
        u32::from(a.x) + u32::from(a.width),
        u32::from(a.y) + u32::from(a.height),
    );
    let (bx1, by1) = (
        u32::from(b.x) + u32::from(b.width),
        u32::from(b.y) + u32::from(b.height),
    );
    u32::from(a.x) < bx1 && u32::from(b.x) < ax1 && u32::from(a.y) < by1 && u32::from(b.y) < ay1
}

fn fill_rect(frame: &mut Frame, rect: Rect, color: Rgb) {
    for y in rect.y..rect.y.saturating_add(rect.height) {
        for x in rect.x..rect.x.saturating_add(rect.width) {
            frame.set(x, y, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_TEXT_CHARS;

    fn canvas() -> Canvas {
        Canvas::new(64, 64).expect("valid")
    }

    fn rate() -> Rate {
        Rate::new(25).expect("valid")
    }

    fn fixed(rect: Rect, text: &str) -> RegionSpec {
        RegionSpec {
            rect,
            text: text.into(),
            style: TextStyle::default(),
            behavior: RegionBehavior::Fixed {
                align: Align::Center,
            },
        }
    }

    fn scroller(
        rect: Rect,
        text: &str,
        path: ScrollPath,
        direction: ScrollDirection,
    ) -> RegionSpec {
        RegionSpec {
            rect,
            text: text.into(),
            style: TextStyle {
                scale: 1,
                ..TextStyle::default()
            },
            behavior: RegionBehavior::Scroll {
                path,
                direction,
                // 25 px/s at 25 fps is exactly one pixel per frame.
                speed_px_s: 25,
            },
        }
    }

    fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// Lit pixels of `frame` as `(x, y)` coordinates.
    fn lit(frame: &Frame) -> Vec<(usize, usize)> {
        frame
            .as_rgb()
            .chunks(3)
            .enumerate()
            .filter(|(_, px)| px.iter().any(|&b| b > 0))
            .map(|(i, _)| (i % 64, i / 64))
            .collect()
    }

    fn centroid(points: &[(usize, usize)]) -> (f64, f64) {
        let n = points.len() as f64;
        let sx: usize = points.iter().map(|p| p.0).sum();
        let sy: usize = points.iter().map(|p| p.1).sum();
        (sx as f64 / n, sy as f64 / n)
    }

    #[test]
    fn an_all_fixed_layout_is_a_single_still_frame() {
        let regions = [
            fixed(rect(0, 0, 64, 16), "HI"),
            fixed(rect(0, 20, 64, 16), "YO"),
        ];
        let sequence = render_layout(&regions, canvas(), rate(), 1_500).expect("renders");
        assert_eq!(
            sequence.len(),
            1,
            "with no scroller there is nothing to animate"
        );
        assert!(!lit(sequence.get(0).expect("frame")).is_empty());
    }

    #[test]
    fn fixed_alignment_places_text_left_center_and_right() {
        // "HI" at scale 2 is 32 px wide in a 64 px rect: alignment decides which
        // 32-px band lights up.
        for (align, band) in [
            (Align::Left, 0..32),
            (Align::Center, 16..48),
            (Align::Right, 32..64),
        ] {
            let region = RegionSpec {
                behavior: RegionBehavior::Fixed { align },
                ..fixed(rect(0, 0, 64, 16), "HI")
            };
            let sequence = render_layout(&[region], canvas(), rate(), 1_500).expect("renders");
            let points = lit(sequence.get(0).expect("frame"));
            assert!(!points.is_empty());
            assert!(
                points.iter().all(|&(x, _)| band.contains(&x)),
                "alignment {align:?} must keep pixels in {band:?}"
            );
        }
    }

    #[test]
    fn a_scroller_starts_outside_crosses_and_exits_its_rect() {
        let region = scroller(
            rect(0, 52, 64, 12),
            "HELLO WORLD",
            ScrollPath::LeftToRight,
            ScrollDirection::Normal,
        );
        let sequence = render_layout(&[region], canvas(), rate(), 1_500).expect("renders");
        // travel = 64 + 88 = 152 px at 1 px/frame, plus the parked final frame.
        assert_eq!(sequence.len(), 153);

        assert!(
            lit(sequence.get(0).expect("first")).is_empty(),
            "at frame zero the text is fully outside its entry edge"
        );
        assert!(
            !lit(sequence.get(76).expect("middle")).is_empty(),
            "mid-travel the text is inside the rectangle"
        );
        assert!(
            lit(sequence.get(152).expect("last")).is_empty(),
            "on the final frame the text has fully exited"
        );
    }

    #[test]
    fn each_path_and_direction_moves_the_expected_way() {
        // Expected centroid drift signs between an early and a later mid-travel
        // frame: +1 grows, -1 shrinks, 0 stays put on that axis.
        let cases = [
            (ScrollPath::LeftToRight, ScrollDirection::Normal, (1, 0)),
            (ScrollPath::LeftToRight, ScrollDirection::Reverse, (-1, 0)),
            (ScrollPath::TopToBottom, ScrollDirection::Normal, (0, 1)),
            (ScrollPath::TopToBottom, ScrollDirection::Reverse, (0, -1)),
            (
                ScrollPath::TopLeftToBottomRight,
                ScrollDirection::Normal,
                (1, 1),
            ),
            (
                ScrollPath::TopLeftToBottomRight,
                ScrollDirection::Reverse,
                (-1, -1),
            ),
            (
                ScrollPath::BottomLeftToTopRight,
                ScrollDirection::Normal,
                (1, -1),
            ),
            (
                ScrollPath::BottomLeftToTopRight,
                ScrollDirection::Reverse,
                (-1, 1),
            ),
        ];
        for (path, direction, (sign_x, sign_y)) in cases {
            let region = scroller(rect(8, 8, 48, 48), "OO", path, direction);
            let sequence = render_layout(&[region], canvas(), rate(), 1_500).expect("renders");
            // Sample two frames while the block is well inside the rectangle.
            let mid = sequence.len() / 2;
            let a = lit(sequence.get(mid - 4).expect("early"));
            let b = lit(sequence.get(mid + 4).expect("late"));
            assert!(!a.is_empty() && !b.is_empty(), "{path:?} {direction:?}");
            let (ax, ay) = centroid(&a);
            let (bx, by) = centroid(&b);
            let drift = |sign: i32, before: f64, after: f64, axis: &str| match sign {
                1 => assert!(after > before, "{path:?} {direction:?}: {axis} must grow"),
                -1 => assert!(after < before, "{path:?} {direction:?}: {axis} must shrink"),
                _ => assert!(
                    (after - before).abs() < 1.0,
                    "{path:?} {direction:?}: {axis} must hold steady"
                ),
            };
            drift(sign_x, ax, bx, "x");
            drift(sign_y, ay, by, "y");
        }
    }

    #[test]
    fn glyph_pixels_never_escape_their_rect() {
        let bounds = rect(16, 16, 32, 16);
        let region = scroller(
            bounds,
            "WWWW",
            ScrollPath::LeftToRight,
            ScrollDirection::Normal,
        );
        let sequence = render_layout(&[region], canvas(), rate(), 1_500).expect("renders");
        let mut ever_lit = false;
        for i in 0..sequence.len() {
            for (x, y) in lit(sequence.get(i).expect("frame")) {
                ever_lit = true;
                assert!(
                    (16..48).contains(&x) && (16..32).contains(&y),
                    "frame {i}: lit pixel ({x}, {y}) is outside the region's rectangle"
                );
            }
        }
        // Containment over zero pixels proves nothing: the clip must admit the
        // glyphs mid-crossing, or a clip that culls everything passes vacuously.
        assert!(ever_lit, "the scroller lights pixels while crossing");
    }

    #[test]
    fn fixed_text_stays_visible_while_a_scroller_crosses() {
        let headline = fixed(rect(0, 0, 64, 16), "HI");
        let ticker = scroller(
            rect(0, 32, 64, 8),
            "NEWS",
            ScrollPath::LeftToRight,
            ScrollDirection::Normal,
        );
        let sequence =
            render_layout(&[headline, ticker], canvas(), rate(), 1_500).expect("renders");
        assert!(sequence.len() > 1, "the ticker animates the package");

        let mut ticker_ever_lit = false;
        for i in 0..sequence.len() {
            let points = lit(sequence.get(i).expect("frame"));
            assert!(
                points.iter().any(|&(_, y)| y < 16),
                "frame {i}: the fixed headline must stay visible for the whole package"
            );
            ticker_ever_lit |= points.iter().any(|&(_, y)| y >= 32);
        }
        assert!(ticker_ever_lit, "the ticker crosses its rectangle");
    }

    #[test]
    fn a_finished_scroller_parks_outside_while_the_longest_runs() {
        let short = scroller(
            rect(0, 0, 64, 8),
            "A",
            ScrollPath::LeftToRight,
            ScrollDirection::Normal,
        );
        let long = RegionSpec {
            behavior: RegionBehavior::Scroll {
                path: ScrollPath::LeftToRight,
                direction: ScrollDirection::Normal,
                // Half the short region's speed: this scroller runs roughly twice
                // as long and defines the package length.
                speed_px_s: 12,
            },
            ..scroller(
                rect(0, 32, 64, 8),
                "A",
                ScrollPath::LeftToRight,
                ScrollDirection::Normal,
            )
        };
        let sequence = render_layout(&[short, long], canvas(), rate(), 1_500).expect("renders");

        // short: travel 72 at 1 px/frame -> 73 frames. long: ceil(72*25/12)+1 = 151.
        assert_eq!(sequence.len(), 151);

        // Past the short scroller's own timeline, its rows show nothing while the
        // slow one is still crossing.
        let late = lit(sequence.get(100).expect("frame"));
        assert!(
            late.iter().all(|&(_, y)| y >= 32),
            "the finished scroller's rectangle must be empty"
        );
        assert!(
            late.iter().any(|&(_, y)| y >= 32),
            "the long scroller is still visible"
        );
    }

    #[test]
    fn package_length_is_the_longest_scroller() {
        // 25 fps at speed 5 is one pixel every five frames: travel 64 + 8 = 72 px
        // takes ceil(72*25/5) = 360 steps, plus the parked final frame.
        let region = RegionSpec {
            behavior: RegionBehavior::Scroll {
                path: ScrollPath::LeftToRight,
                direction: ScrollDirection::Normal,
                speed_px_s: 5,
            },
            ..scroller(
                rect(0, 0, 64, 8),
                "A",
                ScrollPath::LeftToRight,
                ScrollDirection::Normal,
            )
        };
        let sequence = render_layout(&[region], canvas(), rate(), 1_500).expect("renders");
        assert_eq!(sequence.len(), 361);
    }

    #[test]
    fn an_empty_region_list_is_refused() {
        let err = render_layout(&[], canvas(), rate(), 1_500).expect_err("no regions");
        assert_eq!(err.code(), "matrix_layout_no_regions");
    }

    #[test]
    fn too_many_regions_are_refused() {
        // Seventeen 8x8 rects tiled without overlap, each holding one scale-1
        // character that genuinely fits — every region passes validation on its
        // own, so the refusal can only come from the count.
        let regions: Vec<RegionSpec> = (0..17u16)
            .map(|i| RegionSpec {
                style: TextStyle {
                    scale: 1,
                    ..TextStyle::default()
                },
                ..fixed(rect((i % 8) * 8, (i / 8) * 8, 8, 8), ".")
            })
            .collect();
        for region in &regions[..2] {
            render_layout(std::slice::from_ref(region), canvas(), rate(), 1_500)
                .expect("each region is individually valid");
        }
        let err = render_layout(&regions, canvas(), rate(), 1_500).expect_err("too many");
        assert_eq!(err.code(), "matrix_layout_too_many_regions");
    }

    #[test]
    fn a_zero_size_rect_is_refused() {
        let err = render_layout(&[fixed(rect(0, 0, 0, 16), "HI")], canvas(), rate(), 1_500)
            .expect_err("degenerate");
        assert_eq!(err.code(), "matrix_layout_rect_degenerate");
    }

    #[test]
    fn a_rect_past_the_canvas_edge_is_refused() {
        let err = render_layout(&[fixed(rect(40, 0, 32, 16), "HI")], canvas(), rate(), 1_500)
            .expect_err("out of bounds");
        assert_eq!(err.code(), "matrix_layout_rect_out_of_bounds");
    }

    #[test]
    fn overlapping_regions_are_refused() {
        let regions = [
            fixed(rect(0, 0, 32, 16), "A"),
            fixed(rect(16, 8, 32, 16), "B"),
        ];
        let err = render_layout(&regions, canvas(), rate(), 1_500).expect_err("overlap");
        assert_eq!(err.code(), "matrix_layout_overlap");
    }

    #[test]
    fn adjacent_regions_sharing_an_edge_do_not_overlap() {
        let regions = [
            fixed(rect(0, 0, 64, 16), "A"),
            fixed(rect(0, 16, 64, 16), "B"),
        ];
        render_layout(&regions, canvas(), rate(), 1_500)
            .expect("half-open bounds make shared edges legal");
    }

    #[test]
    fn fixed_text_that_does_not_fit_its_rect_is_refused() {
        // "HELLO" at scale 2 is 80 px wide; the rect offers 64.
        let err = render_layout(
            &[fixed(rect(0, 0, 64, 16), "HELLO")],
            canvas(),
            rate(),
            1_500,
        )
        .expect_err("overflow");
        assert_eq!(err.code(), "matrix_layout_fixed_overflow");
    }

    #[test]
    fn scroll_speed_outside_the_bounds_is_refused() {
        // The ceiling is the frame rate: anything past one pixel per frame skips
        // travel between consecutive frames.
        for speed in [0u16, rate().fps() + 1] {
            let region = RegionSpec {
                behavior: RegionBehavior::Scroll {
                    path: ScrollPath::LeftToRight,
                    direction: ScrollDirection::Normal,
                    speed_px_s: speed,
                },
                ..fixed(rect(0, 0, 64, 16), "HI")
            };
            let err = render_layout(&[region], canvas(), rate(), 1_500).expect_err("bad speed");
            assert_eq!(err.code(), "matrix_layout_bad_speed");
        }
    }

    #[test]
    fn a_scroller_overflowing_the_axis_it_does_not_travel_is_refused() {
        // Motion never fully reveals text taller than the rectangle: horizontal
        // and diagonal scrollers would crop glyph rows in every frame, and a
        // top-to-bottom scroller does the same for text wider than its rect.
        let too_tall = RegionSpec {
            // Scale 2 text is 16 px tall; the rect offers 12.
            style: TextStyle::default(),
            ..scroller(
                rect(0, 52, 64, 12),
                "HELLO",
                ScrollPath::LeftToRight,
                ScrollDirection::Normal,
            )
        };
        let too_tall_diagonal = RegionSpec {
            style: TextStyle::default(),
            ..scroller(
                rect(0, 52, 64, 12),
                "HELLO",
                ScrollPath::TopLeftToBottomRight,
                ScrollDirection::Normal,
            )
        };
        let too_wide = scroller(
            // Scale 1 "STATUS" is 48 px wide; the rect offers 16.
            rect(0, 0, 16, 64),
            "STATUS",
            ScrollPath::TopToBottom,
            ScrollDirection::Reverse,
        );
        for region in [too_tall, too_tall_diagonal, too_wide] {
            let err = render_layout(&[region], canvas(), rate(), 1_500).expect_err("overflow");
            assert_eq!(err.code(), "matrix_layout_scroll_overflow");
        }
    }

    #[test]
    fn long_diagonal_text_renders_as_a_band_inside_its_rect() {
        // Width overflow stays legal on a diagonal: both axes share one progress
        // scalar, so the text is revealed as a moving band across the block rather
        // than whole characters at a time. The contract here is that the band
        // renders, stays inside the rectangle, and enters and exits cleanly.
        let diagonal = scroller(
            rect(24, 24, 16, 16),
            "OVERSIZED FOR THE RECT",
            ScrollPath::TopLeftToBottomRight,
            ScrollDirection::Normal,
        );
        let sequence = render_layout(&[diagonal], canvas(), rate(), 1_500).expect("renders");
        assert!(
            lit(sequence.get(0).expect("first")).is_empty(),
            "the band starts outside the rectangle"
        );
        assert!(
            lit(sequence.get(sequence.len() - 1).expect("last")).is_empty(),
            "the band exits the rectangle"
        );
        let mut ever_lit = false;
        for i in 0..sequence.len() {
            for (x, y) in lit(sequence.get(i).expect("frame")) {
                ever_lit = true;
                assert!(
                    (24..40).contains(&x) && (24..40).contains(&y),
                    "frame {i}: lit pixel ({x}, {y}) escapes the rectangle"
                );
            }
        }
        assert!(ever_lit, "the band lights pixels mid-crossing");
    }

    #[test]
    fn a_reversed_path_retraces_the_normal_positions_exactly() {
        // Speed 25 at 25 fps steps one pixel per frame and divides the span
        // evenly, so the reverse package is the normal package frame-reversed.
        let build = |direction| {
            scroller(
                rect(8, 8, 48, 48),
                "OO",
                ScrollPath::BottomLeftToTopRight,
                direction,
            )
        };
        let normal = render_layout(&[build(ScrollDirection::Normal)], canvas(), rate(), 1_500)
            .expect("renders");
        let reverse = render_layout(&[build(ScrollDirection::Reverse)], canvas(), rate(), 1_500)
            .expect("renders");
        assert_eq!(normal.len(), reverse.len());
        for i in 0..normal.len() {
            assert_eq!(
                normal.get(i).expect("frame").as_rgb(),
                reverse.get(normal.len() - 1 - i).expect("frame").as_rgb(),
                "frame {i}: reverse must retrace the normal path's positions"
            );
        }
    }

    #[test]
    fn an_over_budget_package_is_refused_before_any_frame_exists() {
        let region = scroller(
            rect(0, 0, 64, 8),
            "HELLO",
            ScrollPath::LeftToRight,
            ScrollDirection::Normal,
        );
        let err = render_layout(&[region], canvas(), rate(), 10).expect_err("over budget");
        assert_eq!(err.code(), "matrix_layout_over_budget");
    }

    #[test]
    fn per_region_text_refusals_reuse_the_text_codes() {
        let cases: [(RegionSpec, &str); 3] = [
            (fixed(rect(0, 0, 64, 16), ""), "matrix_text_empty"),
            (
                RegionSpec {
                    style: TextStyle {
                        scale: 1,
                        ..TextStyle::default()
                    },
                    ..fixed(rect(0, 0, 64, 16), &"x".repeat(MAX_TEXT_CHARS + 1))
                },
                "matrix_text_too_long",
            ),
            (
                RegionSpec {
                    style: TextStyle {
                        scale: 5,
                        ..TextStyle::default()
                    },
                    ..fixed(rect(0, 0, 64, 16), "HI")
                },
                "matrix_text_bad_scale",
            ),
        ];
        for (region, code) in cases {
            let err = render_layout(&[region], canvas(), rate(), 1_500).expect_err(code);
            assert_eq!(err.code(), code);
        }
    }

    #[test]
    fn colors_parse_exactly_or_not_at_all() {
        assert_eq!(
            parse_color("#FFAA00").expect("parses"),
            Rgb::new(255, 170, 0)
        );
        assert_eq!(
            parse_color("#ffffff").expect("parses"),
            Rgb::new(255, 255, 255)
        );
        for bad in ["fff", "#fff", "#gg0000", "#00e5ff00", "red", ""] {
            assert!(parse_color(bad).is_none(), "{bad:?} must be refused");
        }
        let err = LayoutError::BadColor {
            region: 3,
            color: "#gg0000".into(),
        };
        assert_eq!(err.code(), "matrix_layout_bad_color");
        assert!(
            err.to_string().contains("region 3"),
            "the refusal names the offending region"
        );
    }

    #[test]
    fn region_backgrounds_paint_only_their_own_rect() {
        let region = RegionSpec {
            style: TextStyle {
                background: Rgb::new(0, 0, 255),
                ..TextStyle::default()
            },
            ..fixed(rect(0, 0, 64, 16), "HI")
        };
        let sequence = render_layout(&[region], canvas(), rate(), 1_500).expect("renders");
        let frame = sequence.get(0).expect("frame");
        for (x, y) in lit(frame) {
            assert!(
                y < 16,
                "pixel ({x}, {y}) outside the region must stay unlit"
            );
        }
        assert_eq!(
            frame.get(0, 0).expect("in bounds"),
            Rgb::new(0, 0, 255),
            "a background corner the glyphs do not cover shows the region background"
        );
    }
}
