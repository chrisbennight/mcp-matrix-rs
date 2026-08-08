//! The send path: the component that actually connects device feedback and the power
//! budget to frames leaving for the panel.
//!
//! [`crate::rate`] and [`crate::power`] are decision functions. This is what calls them.
//! A frame reaching a sink without passing through here has been neither rate-governed
//! nor power-clamped, which is why the pump's `emit` is not the public send path.

use crate::power::PowerBudget;
use crate::pump::{FrameSink, PumpError};
use crate::rate;
use matrix_frame::{Frame, FrameSequence, Rate};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The device's most recently reported framerate, shared with whatever polls it.
///
/// An atomic rather than a channel because the pump wants the newest value, not every
/// value ever published — a backlog of stale framerate samples is worse than missing
/// one.
///
/// Each publication carries a generation, and that is load-bearing rather than
/// bookkeeping. Adaptation steps *per sample*, so a reader that consumed the same
/// stored value on every tick would apply one report dozens of times between polls and
/// walk the rate all the way down — defeating the property that a single bad sample
/// cannot collapse playback. The generation is what lets a reader apply a report once.
#[derive(Debug, Clone, Default)]
pub struct FpsFeedback(Arc<AtomicU64>);

/// A framerate report and the generation that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FpsSample {
    pub fps: u16,
    pub generation: u32,
}

impl FpsFeedback {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a report, superseding any unread one.
    pub fn publish(&self, fps: u16) {
        let previous = self.0.load(Ordering::Relaxed);
        let generation = ((previous >> 16) as u32).wrapping_add(1);
        self.0.store(
            (u64::from(generation) << 16) | u64::from(fps),
            Ordering::Relaxed,
        );
    }

    /// The newest report and its generation.
    pub fn sample(&self) -> FpsSample {
        let packed = self.0.load(Ordering::Relaxed);
        FpsSample {
            fps: (packed & 0xFFFF) as u16,
            generation: (packed >> 16) as u32,
        }
    }

    pub fn latest(&self) -> u16 {
        self.sample().fps
    }
}

/// Snap a scheduled boundary forward past the ones missed while suspended.
///
/// Returns the boundary to use now and how many were skipped. Computed rather than
/// stepped: walking one interval at a time makes the work grow with the length of the
/// suspension, which at the 240 fps ceiling is about 20.7 million iterations for a day.
pub fn snap_forward(
    scheduled: tokio::time::Instant,
    now: tokio::time::Instant,
    interval: Duration,
) -> (tokio::time::Instant, u64) {
    if interval.is_zero() {
        return (scheduled, 0);
    }
    let behind = now.saturating_duration_since(scheduled);
    let skipped = (behind.as_nanos() / interval.as_nanos()) as u64;
    if skipped == 0 {
        return (scheduled, 0);
    }
    let step = u32::try_from(skipped).unwrap_or(u32::MAX);
    (scheduled + interval.saturating_mul(step), skipped)
}

/// Outcome of a paced run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunSummary {
    pub sends: u64,
    pub dropped: u64,
    pub finished: bool,
}

/// What one send did, for a caller that reports or adapts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendReport {
    /// Frame index sent.
    pub index: usize,
    /// Frames skipped because the tick was late.
    pub dropped: u32,
    /// Scale applied to fit the power budget; 255 means untouched.
    pub power_scale: u8,
    /// Rate in force for this send.
    pub rate: Rate,
}

/// Drives a sequence to a sink under device feedback and a power budget.
#[derive(Debug)]
pub struct Playout<S> {
    sink: S,
    target_rate: Rate,
    current_rate: Rate,
    budget: Option<PowerBudget>,
    index: usize,
    started: bool,
    /// Generation of the last device report applied.
    ///
    /// On the Playout rather than inside `run`, because a run-local counter resets on
    /// every entry and re-applies the stored report: two bounded runs with no new
    /// publication between them would step the rate twice off one sample.
    consumed_generation: u32,
    /// Monotonic source-frame position, which may exceed the sequence length when
    /// looping; the index is this modulo the length.
    position: u64,
    /// Total time the sequence has been playing.
    ///
    /// Position is derived from this rather than from per-tick deltas. Flooring each
    /// delta to whole source frames discards its remainder, and the remainders
    /// accumulate: 25 fps content sent at 17 fps loses about 18 ms per tick and drifts
    /// to playing at the send rate. Deriving from a running total cannot drift.
    elapsed: Duration,
}

impl<S: FrameSink> Playout<S> {
    /// `budget` is `None` when the device enforces no ceiling, in which case frames are
    /// sent unscaled — a ceiling the operator disabled is not one to invent.
    pub fn new(sink: S, target_rate: Rate, budget: Option<PowerBudget>) -> Self {
        Self {
            sink,
            target_rate,
            current_rate: target_rate,
            budget,
            index: 0,
            started: false,
            consumed_generation: 0,
            position: 0,
            elapsed: Duration::ZERO,
        }
    }

    pub fn rate(&self) -> Rate {
        self.current_rate
    }

    pub fn index(&self) -> usize {
        self.index
    }

    /// Feed the device's reported framerate in.
    ///
    /// Backoff and recovery are both applied: a device behind its rate steps down, and
    /// one comfortably ahead of a reduced rate steps back toward the target.
    pub fn observe_device_fps(&mut self, observed_fps: u16) -> Rate {
        let backed_off = rate::adapt(self.current_rate, observed_fps);
        self.current_rate = if backed_off == self.current_rate {
            rate::recover(self.current_rate, self.target_rate, observed_fps)
        } else {
            backed_off
        };
        self.current_rate
    }

    /// Advance by elapsed time and send the resulting frame, power-clamped.
    pub async fn send_next(
        &mut self,
        sequence: &FrameSequence,
        elapsed: Duration,
        looping: bool,
    ) -> Result<SendReport, PumpError> {
        // The first send transmits the position it is already at. Advancing before
        // selecting would mean index 0 is never transmitted for any sequence.
        let (index, dropped) = if self.started {
            self.elapsed += elapsed;

            // Position comes from total elapsed time against the sequence's own rate.
            // The send rate governs how often frames go out; the sequence rate defines
            // where the content should be at a given moment. Advancing by the send rate
            // would stretch the content.
            let (position, index) =
                crate::pump::position_at(self.elapsed, sequence.rate(), sequence.len(), looping);

            // Frames between the last position and this one were never sent.
            let advanced = position.saturating_sub(self.position);
            self.position = position;
            (
                index,
                u32::try_from(advanced.saturating_sub(1)).unwrap_or(u32::MAX),
            )
        } else {
            self.started = true;
            (self.index, 0)
        };
        self.index = index;

        let source = sequence
            .get(index)
            .expect("advance returns an index inside the sequence");

        let power_scale = match self.budget {
            None => 255,
            Some(budget) => {
                let mut candidate: Frame = source.clone();
                let scale = budget.fit_frame(&mut candidate);
                if scale != 255 {
                    self.sink.send(&candidate).await?;
                    return Ok(SendReport {
                        index,
                        dropped,
                        power_scale: scale,
                        rate: self.current_rate,
                    });
                }
                255
            }
        };

        self.sink.send(source).await?;
        Ok(SendReport {
            index,
            dropped,
            power_scale,
            rate: self.current_rate,
        })
    }

    /// Run the pump, pacing sends at the rate device feedback dictates.
    ///
    /// This is what makes `observe_device_fps` mean anything. Each tick reads the latest
    /// reported framerate, adapts, and then sleeps for the resulting interval — so a
    /// device falling behind genuinely slows the send cadence rather than only changing
    /// a number in a report.
    ///
    /// Position still advances against the sequence's own rate, so slowing the cadence
    /// drops source frames instead of stretching the content.
    pub async fn run(
        &mut self,
        sequence: &FrameSequence,
        looping: bool,
        feedback: &FpsFeedback,
        max_sends: Option<u64>,
    ) -> Result<RunSummary, PumpError> {
        let mut summary = RunSummary::default();
        // Ticks are scheduled against an absolute clock rather than by sleeping a full
        // interval after each send. Sleeping afterwards adds the sink's latency to every
        // interval, so a 10 ms send at 25 fps puts starts 50 ms apart — the pump silently
        // runs at 20 fps while still advancing one frame per send, stretching the
        // content. Waiting until the next boundary absorbs the send cost instead.
        let mut next_tick = tokio::time::Instant::now();
        let mut last_wake: Option<tokio::time::Instant> = None;

        loop {
            if let Some(limit) = max_sends
                && summary.sends >= limit
            {
                break;
            }

            tokio::time::sleep_until(next_tick).await;
            let now = tokio::time::Instant::now();

            // Lateness is measured against the cadence the boundary was scheduled at,
            // before new feedback can change it. Applying a backoff first widens the
            // interval and undershoots the skip: a 25 fps run resuming 120 ms late that
            // simultaneously backs off to 17 fps would land on frame 2 while wall clock
            // is already at frame 3, putting a superseded frame on the panel.
            let scheduled_interval = self.current_rate.interval();
            let (snapped, _skipped) = snap_forward(next_tick, now, scheduled_interval);
            next_tick = snapped;

            // Applied once per report. Reading the stored value every tick would apply
            // one sample dozens of times between polls and step the rate down on each,
            // which is the collapse the stepping exists to prevent.
            let sample = feedback.sample();
            if sample.generation != self.consumed_generation {
                self.consumed_generation = sample.generation;
                self.observe_device_fps(sample.fps);
            }

            // Position advances by actual wake time, not by scheduled-boundary deltas.
            // Boundary deltas lag the wall clock by up to one send interval whenever a
            // wake lands late but inside the interval — at a backed-off cadence that is
            // long enough to put a visibly superseded frame on the panel.
            //
            // The first tick of a run has no previous wake. A playout that has already
            // started resumes by one cadence step, so bounded re-entry walks the
            // sequence rather than resending the frame it is parked on; a fresh playout
            // passes zero and `send_next` transmits the position it is already at.
            let elapsed = match last_wake {
                Some(previous) => now.saturating_duration_since(previous),
                None if self.started => self.current_rate.interval(),
                None => Duration::ZERO,
            };
            last_wake = Some(now);

            let report = self.send_next(sequence, elapsed, looping).await?;
            summary.sends += 1;
            summary.dropped += u64::from(report.dropped);

            if !looping && crate::pump::is_finished(report.index, sequence.len(), looping) {
                summary.finished = true;
                break;
            }

            // Advance to the next boundary. A send that overran its interval leaves
            // boundaries behind; those are skipped on the next wake rather than queued.
            next_tick += self.current_rate.interval();
        }

        Ok(summary)
    }

    pub fn into_sink(self) -> S {
        self.sink
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_frame::{Canvas, Rgb};
    use std::sync::{Arc, Mutex};

    fn canvas() -> Canvas {
        Canvas::new(64, 64).expect("valid")
    }

    fn rate_of(fps: u16) -> Rate {
        Rate::new(fps).expect("valid")
    }

    #[derive(Default, Clone)]
    struct RecordingSink {
        frames: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl FrameSink for RecordingSink {
        async fn send(&mut self, frame: &Frame) -> Result<(), PumpError> {
            self.frames
                .lock()
                .expect("lock")
                .push(frame.as_rgb().to_vec());
            Ok(())
        }
    }

    fn white_sequence(len: usize) -> FrameSequence {
        let frames = (0..len)
            .map(|_| {
                let mut f = Frame::blank(canvas());
                f.fill(Rgb::new(255, 255, 255));
                f
            })
            .collect();
        FrameSequence::new(rate_of(25), frames).expect("uniform")
    }

    fn dim_sequence(len: usize) -> FrameSequence {
        let frames = (0..len)
            .map(|_| {
                let mut f = Frame::blank(canvas());
                f.set(0, 0, Rgb::new(255, 255, 255));
                f
            })
            .collect();
        FrameSequence::new(rate_of(25), frames).expect("uniform")
    }

    #[tokio::test]
    async fn an_over_budget_frame_is_scaled_before_it_reaches_the_sink() {
        let sink = RecordingSink::default();
        // A ceiling below the panel's rated full-white draw, so the clamp must engage.
        let budget = PowerBudget::from_device(1_200).expect("a tighter ceiling");
        let mut playout = Playout::new(sink.clone(), rate_of(25), Some(budget));

        let report = playout
            .send_next(&white_sequence(3), rate_of(25).interval(), true)
            .await
            .expect("send");

        assert!(
            report.power_scale < 255,
            "full white over a derated ceiling must be scaled"
        );
        let sent = sink.frames.lock().expect("lock")[0].clone();
        assert!(
            sent.iter().any(|&b| b < 255),
            "the sink must receive scaled pixels, not the original frame"
        );

        let clamped = Frame::from_rgb(canvas(), sent).expect("valid frame");
        assert!(
            budget.estimate_ma(&clamped, 255) <= budget.ceiling_ma,
            "what actually went to the sink must fit the budget"
        );
    }

    #[tokio::test]
    async fn a_frame_inside_the_budget_reaches_the_sink_untouched() {
        let sink = RecordingSink::default();
        let budget = PowerBudget::from_device(3000).expect("a ceiling");
        let mut playout = Playout::new(sink.clone(), rate_of(25), Some(budget));

        let report = playout
            .send_next(&dim_sequence(2), rate_of(25).interval(), true)
            .await
            .expect("send");

        assert_eq!(report.power_scale, 255);
        let sent = &sink.frames.lock().expect("lock")[0];
        assert_eq!(sent.iter().filter(|&&b| b == 255).count(), 3);
    }

    #[tokio::test]
    async fn a_device_with_no_ceiling_sends_frames_unscaled() {
        let sink = RecordingSink::default();
        let mut playout = Playout::new(sink.clone(), rate_of(25), None);

        let report = playout
            .send_next(&white_sequence(2), rate_of(25).interval(), true)
            .await
            .expect("send");

        assert_eq!(report.power_scale, 255);
        assert!(
            sink.frames.lock().expect("lock")[0]
                .iter()
                .all(|&b| b == 255)
        );
    }

    #[tokio::test]
    async fn device_feedback_lowers_the_send_rate() {
        let mut playout = Playout::new(RecordingSink::default(), rate_of(25), None);
        assert_eq!(playout.rate(), rate_of(25));

        let adapted = playout.observe_device_fps(11);
        assert!(
            adapted < rate_of(25),
            "a struggling device must slow the pump"
        );
        assert_eq!(playout.rate(), adapted);
    }

    #[tokio::test]
    async fn device_recovery_raises_the_rate_back_toward_the_target() {
        let mut playout = Playout::new(RecordingSink::default(), rate_of(25), None);
        playout.observe_device_fps(11);
        let reduced = playout.rate();

        for _ in 0..20 {
            playout.observe_device_fps(60);
        }
        assert!(playout.rate() > reduced);
        assert_eq!(playout.rate(), rate_of(25), "recovery stops at the target");
    }

    #[tokio::test(start_paused = true)]
    async fn device_feedback_governs_how_often_frames_are_sent() {
        // The contract is fewer sends at the same position, not a smaller position: a
        // slower device receives frames less often while the content still covers its
        // own timeline. A smaller position would incorrectly specify stretched content.
        // Counted at the sink: a looping run never returns, so a timeout carries no
        // summary and reading sends from its result would compare two zeros.
        async fn one_second_at(reported_fps: u16) -> (usize, usize) {
            let feedback = FpsFeedback::new();
            feedback.publish(reported_fps);
            let sink = RecordingSink::default();
            let mut playout = Playout::new(sink.clone(), rate_of(25), None);
            let sequence = dim_sequence(500);
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                playout.run(&sequence, true, &feedback, None),
            )
            .await;
            let sends = sink.frames.lock().expect("lock").len();
            (sends, playout.index())
        }

        let (fast_sends, fast_pos) = one_second_at(25).await;
        let (slow_sends, slow_pos) = one_second_at(5).await;

        assert!(fast_sends > 0 && slow_sends > 0, "both runs must send");
        assert!(
            fast_sends > slow_sends,
            "a 5 fps device must receive fewer sends than a 25 fps one \
             (fast={fast_sends}, slow={slow_sends})"
        );
        assert!(
            slow_pos.abs_diff(fast_pos) <= 3,
            "both must cover the same second of content \
             (fast={fast_pos}, slow={slow_pos})"
        );
    }

    #[test]
    fn a_long_suspension_snaps_forward_in_one_step() {
        // Asserted on the arithmetic rather than on a running loop: a paused-clock test
        // cannot distinguish constant work from a walk, because virtual time advances
        // instantly either way. This fails if the skip goes back to stepping.
        let interval = rate_of(240).interval();
        let scheduled = tokio::time::Instant::now();
        let day = Duration::from_secs(86_400);

        let (snapped, skipped) = snap_forward(scheduled, scheduled + day, interval);

        // Derived from the interval rather than from 1/240 s: Rate::interval truncates
        // to whole nanoseconds, so 240 fps is 4,166,666 ns and a day yields slightly
        // more boundaries than exact division would.
        let expected = (day.as_nanos() / interval.as_nanos()) as u64;
        assert_eq!(skipped, expected);
        assert!(skipped > 20_000_000, "a day at 240 fps is tens of millions");
        assert!(snapped <= scheduled + day);
        assert!(snapped + interval > scheduled + day);
    }

    #[test]
    fn snapping_is_a_no_op_when_the_boundary_has_not_passed() {
        let interval = rate_of(25).interval();
        let scheduled = tokio::time::Instant::now();
        let (snapped, skipped) = snap_forward(scheduled, scheduled, interval);
        assert_eq!(skipped, 0);
        assert_eq!(snapped, scheduled);

        // Part of an interval late is still the same boundary.
        let (snapped, skipped) = snap_forward(scheduled, scheduled + interval / 2, interval);
        assert_eq!(skipped, 0);
        assert_eq!(snapped, scheduled);
    }

    #[test]
    fn snapping_reports_every_boundary_it_passed() {
        let interval = rate_of(25).interval();
        let scheduled = tokio::time::Instant::now();
        let (_, skipped) = snap_forward(scheduled, scheduled + interval * 7, interval);
        assert_eq!(skipped, 7);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_sink_does_not_stretch_the_content() {
        // A sink costing a large fraction of the interval must not push send starts
        // apart: the position advances against wall clock, so the timeline holds and the
        // frames that fell behind are dropped instead.
        #[derive(Clone, Default)]
        struct SlowSink {
            count: Arc<Mutex<usize>>,
        }
        impl FrameSink for SlowSink {
            async fn send(&mut self, _frame: &Frame) -> Result<(), PumpError> {
                tokio::time::sleep(Duration::from_millis(20)).await;
                *self.count.lock().expect("lock") += 1;
                Ok(())
            }
        }

        let feedback = FpsFeedback::new();
        feedback.publish(25);
        let sink = SlowSink::default();
        let mut playout = Playout::new(sink.clone(), rate_of(25), None);
        let sequence = dim_sequence(1000);

        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            playout.run(&sequence, true, &feedback, None),
        )
        .await;

        // One second of a 25 fps sequence is 25 frames of content regardless of how
        // many sends the slow sink allowed.
        let position = playout.index();
        assert!(
            (20..=30).contains(&position),
            "one second must cover ~25 source frames, reached {position}"
        );
    }

    #[test]
    fn each_publication_supersedes_the_last_and_advances_the_generation() {
        let feedback = FpsFeedback::new();
        assert_eq!(feedback.sample().generation, 0, "nothing published yet");

        feedback.publish(25);
        let first = feedback.sample();
        assert_eq!(first.fps, 25);
        assert!(first.generation > 0);

        feedback.publish(11);
        let second = feedback.sample();
        assert_eq!(second.fps, 11);
        assert_ne!(second.generation, first.generation);

        // Reading twice does not consume anything; the reader tracks what it applied.
        assert_eq!(feedback.sample(), second);
    }

    #[tokio::test(start_paused = true)]
    async fn one_bad_report_does_not_collapse_playback_across_many_ticks() {
        // The pump observes every tick but the device is polled far less often, so a
        // single report is visible for dozens of ticks. Applying it each time would step
        // the rate down repeatedly and walk 25 fps to the floor on one sample.
        let feedback = FpsFeedback::new();
        feedback.publish(1);

        let mut playout = Playout::new(RecordingSink::default(), rate_of(25), None);
        let sequence = dim_sequence(500);

        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            playout.run(&sequence, true, &feedback, Some(60)),
        )
        .await;

        // One sample, one step: the midpoint of 25 and 1, not a walk to the floor.
        assert_eq!(
            playout.rate(),
            rate_of(13),
            "a single report must move the rate exactly once"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_fresh_report_is_applied_after_an_earlier_one() {
        let feedback = FpsFeedback::new();
        feedback.publish(1);
        let mut playout = Playout::new(RecordingSink::default(), rate_of(25), None);
        let sequence = dim_sequence(500);

        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            playout.run(&sequence, true, &feedback, Some(10)),
        )
        .await;
        let after_first = playout.rate();

        // A new publication, so this is a genuinely fresh report rather than the stored
        // one read again.
        feedback.publish(1);
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            playout.run(&sequence, true, &feedback, Some(10)),
        )
        .await;

        assert!(
            playout.rate() < after_first,
            "a fresh report must move the rate ({} then {})",
            after_first.fps(),
            playout.rate().fps()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn re_entering_run_does_not_reapply_the_report_it_already_consumed() {
        // A run-local generation counter resets on every entry, so two bounded runs
        // with no publication between them would step the rate twice off one sample.
        let feedback = FpsFeedback::new();
        feedback.publish(1);
        let mut playout = Playout::new(RecordingSink::default(), rate_of(25), None);
        let sequence = dim_sequence(500);

        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            playout.run(&sequence, true, &feedback, Some(10)),
        )
        .await;
        let after_first = playout.rate();

        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            playout.run(&sequence, true, &feedback, Some(10)),
        )
        .await;

        assert_eq!(
            playout.rate(),
            after_first,
            "no new report means no further movement"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_re_entry_advances_the_sequence_rather_than_resending_one_frame() {
        // A caller draining a sequence one send at a time must walk it. Holding the
        // parked frame on every re-entry would make the bounded API unable to ever
        // finish a sequence.
        let feedback = FpsFeedback::new();
        feedback.publish(25);
        let mut playout = Playout::new(RecordingSink::default(), rate_of(25), None);
        let sequence = dim_sequence(50);

        let mut indices = Vec::new();
        for _ in 0..3 {
            playout
                .run(&sequence, true, &feedback, Some(1))
                .await
                .expect("run");
            indices.push(playout.index());
        }

        assert_eq!(
            indices,
            vec![0, 1, 2],
            "each single-send run must advance one cadence step"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_habitually_overrunning_sink_does_not_lag_the_content_behind_wall_clock() {
        // Every send costs 250 ms against a 200 ms cadence, so each wake lands 50 ms
        // further past its boundary without ever crossing a whole interval. Measuring
        // elapsed by scheduled boundaries would advance 200 ms of content per send and
        // fall a growing fraction of a second behind; actual wake time keeps the
        // content where the wall clock is.
        #[derive(Clone, Default)]
        struct OverrunningSink;
        impl FrameSink for OverrunningSink {
            async fn send(&mut self, _frame: &Frame) -> Result<(), PumpError> {
                tokio::time::sleep(Duration::from_millis(250)).await;
                Ok(())
            }
        }

        let feedback = FpsFeedback::new();
        feedback.publish(5);
        // Target 5 fps: a 200 ms cadence over a 25 fps sequence.
        let mut playout = Playout::new(OverrunningSink, rate_of(5), None);
        let sequence = dim_sequence(100);

        playout
            .run(&sequence, true, &feedback, Some(4))
            .await
            .expect("run");

        // Wakes at 0, 250, 500, and 750 ms. 750 ms of a 25 fps sequence is frame 18;
        // scheduled-boundary elapsed would report frame 15.
        assert_eq!(
            playout.index(),
            18,
            "content position must follow actual wake time"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_stops_at_the_end_of_a_non_looping_sequence() {
        let feedback = FpsFeedback::new();
        feedback.publish(25);
        let mut playout = Playout::new(RecordingSink::default(), rate_of(25), None);

        let summary = playout
            .run(&dim_sequence(5), false, &feedback, Some(50))
            .await
            .expect("run");

        assert!(
            summary.finished,
            "a non-looping sequence must report finishing"
        );
        assert!(summary.sends <= 50);
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_honours_its_send_budget() {
        let feedback = FpsFeedback::new();
        feedback.publish(25);
        let sink = RecordingSink::default();
        let mut playout = Playout::new(sink.clone(), rate_of(25), None);

        let summary = playout
            .run(&dim_sequence(500), true, &feedback, Some(7))
            .await
            .expect("run");

        assert_eq!(summary.sends, 7);
        assert_eq!(sink.frames.lock().expect("lock").len(), 7);
    }

    #[tokio::test]
    async fn the_adapted_rate_is_reported_on_each_send() {
        let sink = RecordingSink::default();
        let mut playout = Playout::new(sink, rate_of(30), None);
        playout.observe_device_fps(10);
        let slowed = playout.rate();

        // One 30 fps interval is less than one interval at the reduced rate, so the
        // position advances by one frame rather than by the ratio of the two.
        let report = playout
            .send_next(&dim_sequence(50), rate_of(30).interval(), true)
            .await
            .expect("send");

        assert_eq!(report.rate, slowed);
        assert_eq!(
            report.index, 0,
            "the first send transmits the current position"
        );
        assert_eq!(report.dropped, 0);
    }

    #[tokio::test]
    async fn a_late_tick_reports_the_frames_it_dropped() {
        let mut playout = Playout::new(RecordingSink::default(), rate_of(25), None);
        let sequence = dim_sequence(100);
        playout
            .send_next(&sequence, Duration::ZERO, true)
            .await
            .expect("first send");
        let report = playout
            .send_next(&sequence, Duration::from_millis(200), true)
            .await
            .expect("late send");

        assert_eq!(report.dropped, 4);
        assert_eq!(report.index, 5);
    }

    #[tokio::test]
    async fn the_first_frame_of_a_sequence_is_transmitted() {
        let sink = RecordingSink::default();
        let mut playout = Playout::new(sink.clone(), rate_of(25), None);
        let frames: Vec<Frame> = (0..4)
            .map(|i| {
                let mut f = Frame::blank(canvas());
                f.fill(Rgb::new(u8::try_from(i).unwrap_or(0), 0, 0));
                f
            })
            .collect();
        let sequence = FrameSequence::new(rate_of(25), frames).expect("uniform");

        playout
            .send_next(&sequence, rate_of(25).interval(), true)
            .await
            .expect("send");

        let first = sink.frames.lock().expect("lock")[0].clone();
        assert_eq!(first[0], 0, "index 0 must reach the wire, not be skipped");
    }

    #[tokio::test]
    async fn a_backed_off_rate_drops_source_frames_rather_than_stretching_playback() {
        // A 25 fps sequence played by a pump backed off to 10 fps must still cover its
        // own timeline: one second of wall clock advances 25 source frames regardless
        // of how many sends happened. Advancing by the send rate instead would stretch
        // the content to 2.5x its intended duration.
        let mut playout = Playout::new(RecordingSink::default(), rate_of(25), None);
        let sequence = dim_sequence(200);

        playout
            .send_next(&sequence, Duration::ZERO, true)
            .await
            .expect("first send");
        playout.observe_device_fps(10);
        assert!(playout.rate() < rate_of(25), "device forced a backoff");

        let report = playout
            .send_next(&sequence, Duration::from_secs(1), true)
            .await
            .expect("send after one second");

        assert_eq!(
            report.index, 25,
            "one second of a 25 fps sequence is 25 source frames"
        );
        assert_eq!(report.dropped, 24);
    }

    #[tokio::test]
    async fn successive_sends_walk_the_sequence() {
        let sink = RecordingSink::default();
        let mut playout = Playout::new(sink.clone(), rate_of(25), None);
        let sequence = dim_sequence(4);

        for _ in 0..3 {
            playout
                .send_next(&sequence, rate_of(25).interval(), true)
                .await
                .expect("send");
        }

        assert_eq!(sink.frames.lock().expect("lock").len(), 3);
        assert_eq!(
            playout.index(),
            2,
            "first send holds index 0, then two advances"
        );
    }
}
