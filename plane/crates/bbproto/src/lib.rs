//! Wire protocol between the ButterBot firmware (`firmware-rs`, which uses this crate) and the
//! laptop control plane.
//!
//! Transport: UDP, like the stock RC video feed. Every datagram carries one frame:
//!
//! ```text
//! magic u16 LE (0xBB01) | type u8 | flags u8 | len u32 LE | payload[len]
//! ```

pub mod bb;
pub mod ctrl;
pub mod frame;
pub mod provision;

pub use ctrl::{FromRobot, ToRobot};
pub use frame::{Frame, FrameError, FrameType, MediaHeader, VideoAssembler, VideoFragment};

/// UDP port the plane listens on for the robot.
pub const LINK_PORT: u16 = 7531;
/// Protocol version announced in `hello`.
pub const PROTO_VERSION: u32 = 2;
/// Audio format in both directions: signed 16-bit little-endian, mono.
pub const SAMPLE_RATE: u32 = 16_000;
