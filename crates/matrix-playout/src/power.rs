//! Power estimation and the brightness clamp.
//!
//! A 64x64 panel at full white draws far more than the controller's 5V rail supplies.
//! WLED has its own automatic brightness limiter, but letting it engage is a poor
//! outcome: it reacts after the fact and the correction is visible as a dip. Estimating
//! the draw of a frame before sending it, and asking for a brightness that stays inside
//! the budget, keeps the limiter out of the picture.
//!
//! The estimate deliberately mirrors the shape of WLED's own model — per-LED current
//! proportional to the mean of its channels, scaled by brightness — so the two agree
//! about when a frame is expensive. It is an estimate, not a measurement; the
//! authoritative figure is what the device reports back as `leds.pwr`.

use matrix_frame::{BYTES_PER_PIXEL, Frame};

/// Milliamps one pixel draws at full white, before brightness scaling, expressed in
/// thousandths of a milliamp so a sub-milliamp figure survives integer arithmetic.
///
/// 55 mA is the figure WLED uses for a discrete addressable LED, and it is wrong here by
/// two orders of magnitude: a HUB75 panel multiplexes, so 4096 pixels draw about 3 A at
/// full white rather than 225 A. Applying the addressable figure would reduce a
/// full-white frame to roughly 1% even though the panel can display it at its rated draw.
///
/// 3 A across 4096 pixels is ~0.732 mA per pixel.
pub const DEFAULT_UA_PER_PIXEL_FULL_WHITE: u32 = 732;

/// Current the controller can supply, minus what it needs for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerBudget {
    pub ceiling_ma: u32,
    /// Microamps per pixel at full white. Panel construction decides this; the default
    /// suits a HUB75 matrix and is wrong for a discrete addressable strip.
    pub ua_per_pixel_full_white: u32,
}

impl PowerBudget {
    /// A budget from the device's own reported ceiling.
    ///
    /// `None` when the device enforces no ceiling — `maxpwr` of zero means automatic
    /// brightness limiting is switched off, and inventing a budget the operator
    /// deliberately disabled would be the wrong call.
    pub fn from_device(max_power_ma: u32) -> Option<Self> {
        (max_power_ma > 0).then_some(Self {
            ceiling_ma: max_power_ma,
            ua_per_pixel_full_white: DEFAULT_UA_PER_PIXEL_FULL_WHITE,
        })
    }

    /// Estimated draw of a frame at a given brightness, in milliamps.
    ///
    /// Sums each pixel's mean channel value, which is what makes a mostly-black frame
    /// nearly free and a full-white frame the worst case.
    pub fn estimate_ma(&self, frame: &Frame, brightness: u8) -> u32 {
        let channel_sum: u64 = frame.as_rgb().iter().map(|&b| u64::from(b)).sum();

        // channel_sum / 3 is the summed per-pixel mean; / 255 normalizes a pixel at full
        // white to 1.0 of a LED's full-white draw; brightness scales the whole frame.
        // channel_sum / 3 is the summed per-pixel mean; / 255 normalizes a full-white
        // pixel to one pixel's draw; brightness scales the frame; / 1000 converts the
        // microamp figure to milliamps.
        let numerator =
            channel_sum * u64::from(self.ua_per_pixel_full_white) * u64::from(brightness);
        let denominator = 3u64 * 255 * 255 * 1000;

        u32::try_from(numerator / denominator).unwrap_or(u32::MAX)
    }

    /// Highest brightness that keeps this frame inside the budget.
    ///
    /// Returns `requested` unchanged when the frame already fits, so ordinary content is
    /// never dimmed for a ceiling it was not going to reach.
    pub fn clamp_brightness(&self, frame: &Frame, requested: u8) -> u8 {
        if self.estimate_ma(frame, requested) <= self.ceiling_ma {
            return requested;
        }

        let channel_sum: u64 = frame.as_rgb().iter().map(|&b| u64::from(b)).sum();
        if channel_sum == 0 {
            return requested;
        }

        let permitted = u64::from(self.ceiling_ma) * 3 * 255 * 255 * 1000
            / (channel_sum * u64::from(self.ua_per_pixel_full_white));

        u8::try_from(permitted.min(u64::from(requested))).unwrap_or(requested)
    }

    /// Scale a frame's pixels so its estimated draw fits the budget.
    ///
    /// Applied on the frame itself rather than by setting device brightness, because
    /// brightness lives on the JSON plane and a round trip per frame is not possible at
    /// playout rates. Scaling the pixels achieves the same reduction on the frame that
    /// is about to be sent, and takes effect on that frame rather than a later one.
    ///
    /// Returns the scale applied, where 255 means the frame already fit and was left
    /// untouched.
    pub fn fit_frame(&self, frame: &mut Frame) -> u8 {
        let permitted = self.clamp_brightness(frame, 255);
        if permitted == 255 {
            return 255;
        }

        let scaled: Vec<u8> = frame
            .as_rgb()
            .iter()
            .map(|&channel| {
                u8::try_from(u16::from(channel) * u16::from(permitted) / 255).unwrap_or(channel)
            })
            .collect();

        *frame = Frame::from_rgb(frame.canvas(), scaled)
            .expect("scaling preserves the buffer length exactly");
        permitted
    }
}

/// Worst-case draw for a canvas: every pixel full white at full brightness.
pub fn worst_case_ma(pixels: usize, ua_per_pixel_full_white: u32) -> u32 {
    u32::try_from(pixels as u64 * u64::from(ua_per_pixel_full_white) / 1000).unwrap_or(u32::MAX)
}

/// Bytes a frame occupies, for callers sizing a transmission budget.
pub fn frame_bytes(pixels: usize) -> usize {
    pixels * BYTES_PER_PIXEL
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_frame::{Canvas, Rgb};

    fn m1() -> Canvas {
        Canvas::new(64, 64).expect("valid")
    }

    fn budget() -> PowerBudget {
        PowerBudget::from_device(3000).expect("a real ceiling")
    }

    #[test]
    fn a_device_with_limiting_disabled_yields_no_budget() {
        // maxpwr of zero is a deliberate operator choice, not a missing value.
        assert_eq!(PowerBudget::from_device(0), None);
        assert!(PowerBudget::from_device(3000).is_some());
    }

    #[test]
    fn a_black_frame_draws_nothing() {
        assert_eq!(budget().estimate_ma(&Frame::blank(m1()), 255), 0);
    }

    #[test]
    fn a_full_white_m1_panel_estimates_its_rated_draw() {
        let mut frame = Frame::blank(m1());
        frame.fill(Rgb::new(255, 255, 255));

        let draw = budget().estimate_ma(&frame, 255);
        assert_eq!(draw, worst_case_ma(4096, DEFAULT_UA_PER_PIXEL_FULL_WHITE));
        // The panel's rated full-white draw, which is what the 3 A ceiling describes.
        assert!(
            (2900..=3100).contains(&draw),
            "full white must estimate near the panel's rated 3 A, got {draw} mA"
        );
    }

    #[test]
    fn draw_scales_with_brightness() {
        let mut frame = Frame::blank(m1());
        frame.fill(Rgb::new(255, 255, 255));
        let budget = budget();

        let full = budget.estimate_ma(&frame, 255);
        let half = budget.estimate_ma(&frame, 128);
        assert!(half < full);
        // Within rounding of half.
        assert!(half.abs_diff(full / 2) < full / 100);
    }

    #[test]
    fn draw_scales_with_lit_area() {
        let budget = budget();
        let mut sparse = Frame::blank(m1());
        for x in 0..10u16 {
            sparse.set(x, 0, Rgb::new(255, 255, 255));
        }
        let mut dense = Frame::blank(m1());
        dense.fill(Rgb::new(255, 255, 255));

        assert!(budget.estimate_ma(&sparse, 255) < budget.estimate_ma(&dense, 255));
    }

    #[test]
    fn a_frame_inside_the_budget_is_not_dimmed() {
        let budget = budget();
        let mut frame = Frame::blank(m1());
        // A handful of lit pixels cannot approach a 3A ceiling.
        for x in 0..20u16 {
            frame.set(x, 0, Rgb::new(255, 255, 255));
        }
        assert_eq!(budget.clamp_brightness(&frame, 255), 255);
    }

    #[test]
    fn a_full_white_frame_fits_a_ceiling_at_the_panel_rating() {
        // 3 A is what the panel draws at full white, so the clamp must leave it alone.
        let mut frame = Frame::blank(m1());
        frame.fill(Rgb::new(255, 255, 255));
        assert_eq!(budget().clamp_brightness(&frame, 255), 255);
    }

    #[test]
    fn a_frame_over_the_budget_is_clamped_to_something_that_fits() {
        // A ceiling below the panel's rated draw, e.g. a shared or derated supply.
        let budget = PowerBudget::from_device(1_500).expect("a tighter ceiling");
        let mut frame = Frame::blank(m1());
        frame.fill(Rgb::new(255, 255, 255));

        let clamped = budget.clamp_brightness(&frame, 255);
        assert!(clamped < 255, "a frame over the ceiling must be reduced");
        assert!(
            budget.estimate_ma(&frame, clamped) <= budget.ceiling_ma,
            "the clamped brightness must actually fit the budget"
        );
    }

    #[test]
    fn clamping_never_raises_a_brightness_the_caller_did_not_ask_for() {
        let budget = budget();
        let frame = Frame::blank(m1());
        assert_eq!(budget.clamp_brightness(&frame, 10), 10);

        let mut white = Frame::blank(m1());
        white.fill(Rgb::new(255, 255, 255));
        assert!(budget.clamp_brightness(&white, 10) <= 10);
    }

    #[test]
    fn a_black_frame_is_never_clamped_even_at_full_brightness() {
        // Guards the divide-by-zero path: zero draw cannot exceed any ceiling.
        assert_eq!(budget().clamp_brightness(&Frame::blank(m1()), 255), 255);
    }

    #[test]
    fn a_larger_ceiling_permits_more_brightness() {
        let mut frame = Frame::blank(m1());
        frame.fill(Rgb::new(255, 255, 255));

        let small = PowerBudget::from_device(500).expect("valid");
        let large = PowerBudget::from_device(1_500).expect("valid");
        assert!(large.clamp_brightness(&frame, 255) > small.clamp_brightness(&frame, 255));
    }

    #[test]
    fn frame_bytes_matches_the_canvas_buffer() {
        assert_eq!(frame_bytes(4096), 12_288);
    }
}
