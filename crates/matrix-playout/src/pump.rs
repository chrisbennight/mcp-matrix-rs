//! The fixed-rate frame pump.
//!
//! Reads only fully resident, fully normalized sequences and emits frames on a fixed
//! tick. It never blocks on ingest, never queues a late frame, and holds no knowledge of
//! what produced the frames it sends.

use matrix_frame::{Frame, Rate};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PumpError {
    #[error("frame sink failed: {0}")]
    Sink(String),
}

impl PumpError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Sink(_) => "playout_sink_failed",
        }
    }
}

/// Where frames go.
///
/// A trait rather than a concrete DDP sender so the pump's timing behaviour is testable
/// without a socket, and so a second output (a preview, a second panel) does not require
/// changing the pump.
pub trait FrameSink {
    fn send(&mut self, frame: &Frame) -> impl std::future::Future<Output = Result<(), PumpError>>;
}

/// The panel itself as a frame sink.
///
/// Connects the governed send path to the DDP transport that reaches the hardware.
impl FrameSink for matrix_device::DdpSender {
    async fn send(&mut self, frame: &Frame) -> Result<(), PumpError> {
        self.send_frame(frame)
            .await
            .map_err(|e| PumpError::Sink(format!("{}: {e}", e.code())))
    }
}

/// Where a sequence's content stands after `elapsed` of total playing time.
///
/// Returns the monotonic source-frame position and the index to display. The drop
/// policy lives here: playout is realtime, so a frame whose moment has passed is
/// superseded by the one after it, and the position lands where wall clock says rather
/// than walking the backlog.
///
/// Derived from the running total rather than from per-tick deltas — flooring each
/// delta to whole frames discards its remainder, and the remainders accumulate into
/// content drift. This is the function [`crate::driver::Playout`] advances with, so the
/// pure-function tests exercise the same policy as the paced send path.
pub fn position_at(
    elapsed: Duration,
    sequence_rate: Rate,
    sequence_len: usize,
    looping: bool,
) -> (u64, usize) {
    debug_assert!(
        sequence_len > 0,
        "a sequence is never empty by construction"
    );

    let interval = sequence_rate.interval();
    let position = if interval.is_zero() {
        0
    } else {
        (elapsed.as_nanos() / interval.as_nanos()) as u64
    };

    let index = if looping {
        (position % sequence_len as u64) as usize
    } else {
        (position as usize).min(sequence_len - 1)
    };

    (position, index)
}

/// Whether a non-looping sequence has finished.
pub fn is_finished(index: usize, sequence_len: usize, looping: bool) -> bool {
    !looping && index >= sequence_len - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(fps: u16) -> Rate {
        Rate::new(fps).expect("valid")
    }

    #[test]
    fn position_tracks_whole_elapsed_frames() {
        assert_eq!(position_at(rate(25).interval(), rate(25), 10, true), (1, 1));
        // Part of a frame interval is still the same frame.
        assert_eq!(
            position_at(Duration::from_millis(1), rate(25), 10, true),
            (0, 0)
        );
    }

    #[test]
    fn a_late_position_lands_where_wall_clock_says_not_on_the_backlog() {
        // 200 ms at 25 fps is five intervals: show frame 5, the four between skipped.
        assert_eq!(
            position_at(Duration::from_millis(200), rate(25), 100, true),
            (5, 5)
        );
    }

    #[test]
    fn a_long_stall_does_not_accumulate_unbounded_work() {
        // Ten seconds of stall resolves to one position, not 250 replays.
        let (position, index) = position_at(Duration::from_secs(10), rate(25), 100, true);
        assert_eq!(position, 250);
        assert_eq!(index, 250 % 100);
    }

    #[test]
    fn sub_frame_remainders_accumulate_rather_than_being_discarded() {
        // Three periods of 1.5 frame intervals each: per-delta flooring would show
        // frame 3, but the running total has genuinely covered 4.5 intervals.
        let interval = rate(25).interval();
        let total = interval * 9 / 2;
        assert_eq!(position_at(total, rate(25), 100, true), (4, 4));
    }

    #[test]
    fn a_looping_sequence_wraps_at_the_end() {
        let (_, index) = position_at(rate(25).interval() * 10, rate(25), 10, true);
        assert_eq!(index, 0);
    }

    #[test]
    fn a_non_looping_sequence_holds_its_last_frame() {
        let (_, index) = position_at(Duration::from_secs(60), rate(25), 10, false);
        assert_eq!(index, 9);
        assert!(is_finished(index, 10, false));
    }

    #[test]
    fn a_looping_sequence_is_never_finished() {
        assert!(!is_finished(9, 10, true));
        assert!(!is_finished(0, 10, true));
    }

    #[test]
    fn a_single_frame_still_sequence_stays_on_frame_zero() {
        let (_, index) = position_at(Duration::from_secs(5), rate(25), 1, true);
        assert_eq!(index, 0);
    }
}
