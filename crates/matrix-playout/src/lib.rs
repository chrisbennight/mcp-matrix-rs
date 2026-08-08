//! Playout: the fixed-rate frame pump and the decisions around it.
//!
//! Three concerns, each isolated so its contract is testable without hardware:
//!
//! - [`rate`] adapts the send rate to what the device reports it is achieving. No
//!   framerate is chosen here; the panel's own `leds.fps` decides.
//! - [`power`] estimates a frame's draw and clamps brightness to stay inside the
//!   device's budget, so WLED's own limiter never has to engage visibly.
//! - [`pump`] advances a playback position on a fixed tick and drops late frames rather
//!   than queueing them.

pub mod driver;
pub mod power;
pub mod pump;
pub mod rate;

pub use driver::{FpsFeedback, FpsSample, Playout, RunSummary, SendReport};
pub use power::PowerBudget;
pub use pump::{FrameSink, PumpError, is_finished, position_at};
pub use rate::{MIN_RATE_FPS, adapt, recover};
