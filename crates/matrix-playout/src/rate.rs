//! Rate adaptation from device feedback.
//!
//! The panel reports the framerate it is actually achieving. That number, not a
//! constant chosen here, decides how fast the pump runs. Sending faster than the device
//! can render wastes bandwidth on frames that are overwritten before they are displayed,
//! and on a WiFi ESP32 also driving HUB75 refresh it makes the achieved rate worse
//! rather than better.

use matrix_frame::{MAX_RATE_FPS, Rate};

/// How far below the requested rate the device may fall before the pump backs off.
///
/// The reported figure fluctuates frame to frame, so reacting to every dip would
/// oscillate. A device asked for 25 and delivering 24 is keeping up.
const TOLERANCE_FPS: u16 = 2;

/// Smallest rate the pump will fall back to. Below this, content is not moving so much
/// as updating, and a further reduction saves little while looking broken.
pub const MIN_RATE_FPS: u16 = 5;

/// Decide the rate to send at, given what was asked for and what the device reports.
///
/// Returns the requested rate unchanged when the device is keeping up. When it is not,
/// the rate steps down toward what the device actually achieved rather than jumping
/// there, so a single bad sample cannot collapse playback.
pub fn adapt(requested: Rate, observed_fps: u16) -> Rate {
    // A device that has not rendered anything yet reports zero. That is absence of
    // evidence, not evidence of failure — keep the requested rate until it has an
    // opinion.
    if observed_fps == 0 {
        return requested;
    }

    // Saturating: leds.fps is device-reported and a malformed value near u16::MAX
    // would panic in a checked build and wrap in an unchecked one, turning a nonsense
    // reading into an adaptation instead of a no-op.
    if observed_fps.saturating_add(TOLERANCE_FPS) >= requested.fps() {
        return requested;
    }

    // The floor never raises the rate above what was asked for: a 4 fps target must
    // not become 5 fps because the device is struggling.
    let floor = MIN_RATE_FPS.min(requested.fps());
    let stepped = requested.fps().midpoint(observed_fps).max(floor);
    Rate::new(stepped.min(MAX_RATE_FPS)).unwrap_or(requested)
}

/// Whether a rate may be raised back toward a target after the device recovers.
///
/// Recovery is deliberately slower than backoff: one step up per evaluation, and only
/// when the device is comfortably ahead of the current rate. A device oscillating around
/// its limit should settle below it, not thrash across it.
pub fn recover(current: Rate, target: Rate, observed_fps: u16) -> Rate {
    if current >= target || observed_fps == 0 {
        return current;
    }

    // At or above the current cadence is the device keeping up. Requiring a margin
    // above it latches a transient backoff forever: a panel rendering every frame it is
    // sent reports exactly the reduced rate, which never clears a higher bar.
    if observed_fps < current.fps() {
        return current;
    }

    let stepped = current
        .fps()
        .saturating_add(TOLERANCE_FPS)
        .min(target.fps());
    Rate::new(stepped).unwrap_or(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(fps: u16) -> Rate {
        Rate::new(fps).expect("valid test rate")
    }

    #[test]
    fn a_device_keeping_up_does_not_change_the_rate() {
        assert_eq!(adapt(rate(25), 25), rate(25));
        assert_eq!(adapt(rate(25), 30), rate(25));
    }

    #[test]
    fn a_device_within_tolerance_is_treated_as_keeping_up() {
        // Reported fps fluctuates; reacting to every dip would oscillate.
        assert_eq!(adapt(rate(25), 24), rate(25));
        assert_eq!(adapt(rate(25), 23), rate(25));
    }

    #[test]
    fn a_device_falling_behind_steps_down_toward_it_not_straight_to_it() {
        // Asked for 25, achieving 11: step to the midpoint rather than collapsing to 11
        // on one sample.
        let adapted = adapt(rate(25), 11);
        assert!(adapted < rate(25));
        assert!(adapted > rate(11));
        assert_eq!(adapted, rate(18));
    }

    #[test]
    fn repeated_backoff_converges_rather_than_overshooting() {
        let mut current = rate(30);
        for _ in 0..10 {
            current = adapt(current, 10);
        }
        assert!(
            current.fps() >= 10,
            "must not fall below what the device achieves"
        );
        assert!(current.fps() <= 12, "must converge near the achieved rate");
    }

    #[test]
    fn backoff_never_raises_the_rate_above_a_target_below_the_floor() {
        // A 4 fps target is already under MIN_RATE_FPS; backing off must not speed up.
        let target = rate(4);
        let adapted = adapt(target, 1);
        assert!(
            adapted <= target,
            "adapt({}, 1) = {} must not exceed the target",
            target.fps(),
            adapted.fps()
        );
    }

    #[test]
    fn backoff_never_goes_below_the_floor() {
        let mut current = rate(30);
        for _ in 0..20 {
            current = adapt(current, 1);
        }
        assert_eq!(current.fps(), MIN_RATE_FPS);
    }

    #[test]
    fn an_absurd_device_reading_is_treated_as_keeping_up_not_as_an_overflow() {
        // leds.fps comes off the wire; a malformed maximum must not panic or wrap into
        // a spurious backoff.
        for reported in [u16::MAX, u16::MAX - 1, 60_000] {
            assert_eq!(
                adapt(rate(25), reported),
                rate(25),
                "reported {reported} must leave the rate alone"
            );
        }
        assert_eq!(recover(rate(15), rate(25), u16::MAX), rate(17));
    }

    #[test]
    fn a_silent_device_keeps_the_requested_rate() {
        // Zero is "has not rendered yet", not "achieving zero".
        assert_eq!(adapt(rate(25), 0), rate(25));
    }

    #[test]
    fn recovery_steps_up_once_the_device_is_keeping_up() {
        let current = rate(15);
        let target = rate(25);
        assert_eq!(recover(current, target, 14), current, "still behind holds");
        // A device rendering every frame it is sent reports the cadence it is sent at,
        // so this is the ordinary recovery case rather than an exceptional one.
        assert_eq!(
            recover(current, target, 15),
            rate(17),
            "keeping up steps up"
        );
        assert_eq!(
            recover(current, target, 20),
            rate(17),
            "ahead steps up once"
        );
    }

    #[test]
    fn a_transient_backoff_does_not_latch_below_the_target() {
        // The panel reports whatever cadence it is being sent, so recovery has to make
        // progress from that alone or a single bad sample is permanent.
        let target = rate(25);
        let mut current = adapt(target, 11);
        assert!(current < target);
        for _ in 0..20 {
            current = recover(current, target, current.fps());
        }
        assert_eq!(current, target, "must climb back to the target");
    }

    #[test]
    fn recovery_never_exceeds_the_target() {
        let mut current = rate(23);
        for _ in 0..10 {
            current = recover(current, rate(25), 60);
        }
        assert_eq!(current, rate(25));
    }

    #[test]
    fn recovery_is_slower_than_backoff() {
        // One step down from 30 against a 10 fps device covers more ground than one step
        // up. Settling below the limit beats thrashing across it.
        let down = rate(30).fps() - adapt(rate(30), 10).fps();
        let up = recover(rate(10), rate(30), 60).fps() - rate(10).fps();
        assert!(down > up, "backoff {down} must outpace recovery {up}");
    }

    #[test]
    fn a_device_already_at_target_is_not_pushed_further() {
        assert_eq!(recover(rate(25), rate(25), 60), rate(25));
    }
}
