//! The device plane: everything that talks to the panel.
//!
//! Two protocols, deliberately separated.
//!
//! [`wled`] is the WLED JSON API over HTTP — identity, dimensions, achieved framerate,
//! power headroom, brightness, and power. [`ddp`] is the Distributed Display Protocol
//! over UDP — frames, and nothing else.
//!
//! Frames never travel over the JSON API. A full 64x64 frame is 12,288 bytes against a
//! 24 KB command buffer on an ESP32, and WLED's own documentation says not to issue
//! several such calls in parallel. DDP exists for exactly this and is what the device
//! renders from.

pub mod ddp;
pub mod wled;

pub use ddp::{DDP_PORT, DdpError, DdpSender, Sequence, frame_packets};
pub use wled::{DeviceInfo, LedInfo, MatrixInfo, StateUpdate, WledClient, WledError};
