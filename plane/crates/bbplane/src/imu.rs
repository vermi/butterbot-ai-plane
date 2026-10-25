//! The robot's accelerometer, streamed by firmware 0.6 at 50 Hz (`bbproto::frame::ImuChunk`):
//! bumps and lifts are felt here, where they are easy to tune.
//!
//! - A bump: a sharp sideways or front-back jolt. While the robot drives it stops at once (a
//!   collision the short-sighted front sensor missed) and the drive ends early (`bump`);
//!   standing still it is news for the agent (someone knocked it).
//! - A lift: upward acceleration held for a moment. It counts together with the floor sensor
//!   (see `control::on_body`), which alone flickers.
//!
//! The sensor's axes: x points up when the robot stands on its tracks; y and z lie in the floor's
//! plane.

use std::time::{Duration, Instant};

use bbproto::ToRobot;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::control::Robot;

/// Horizontal jolt (g) that counts as a bump, standing and driving. Driving jolts the robot a
/// little itself; a collision at its few cm/s still stops it within a sample or two.
const BUMP_STILL_G: f32 = 0.4;
const BUMP_DRIVING_G: f32 = 0.3;
/// Upward acceleration (g), held for `LIFT_HOLD`, that feels like a lift.
const LIFT_G: f32 = 0.08;
const LIFT_HOLD: Duration = Duration::from_millis(120);
/// After one, the next of a kind waits this long (a bump rings on for a moment).
const BUMP_QUIET: Duration = Duration::from_millis(1500);
const LIFT_QUIET: Duration = Duration::from_secs(3);
/// How fast the gravity estimate follows (time constant).
const GRAVITY_TAU_S: f32 = 0.5;

#[derive(Debug, Clone, PartialEq)]
pub enum Felt {
    /// Strength in g, and the side it came from as far as the axes tell (`front`, `back`,
    /// `left`, `right`).
    Bump {
        g: f32,
        side: &'static str,
    },
    Lift,
}

#[derive(Default)]
pub struct Watch {
    gravity: Option<[f32; 3]>,
    up_since: Option<Instant>,
    last_bump: Option<Instant>,
    last_lift: Option<Instant>,
}

impl Watch {
    /// New samples (milli-g, `rate_hz`), the last taken at `now`.
    pub fn push(
        &mut self,
        samples: &[[i16; 3]],
        rate_hz: u16,
        moving: bool,
        now: Instant,
    ) -> Vec<Felt> {
        let dt = 1.0 / rate_hz.max(1) as f32;
        let alpha = 1.0 - (-dt / GRAVITY_TAU_S).exp();
        let mut out = Vec::new();
        for (i, s) in samples.iter().enumerate() {
            let at = now - Duration::from_secs_f32(dt * (samples.len() - 1 - i) as f32);
            let a = [
                s[0] as f32 / 1000.0,
                s[1] as f32 / 1000.0,
                s[2] as f32 / 1000.0,
            ];
            let g = *self.gravity.get_or_insert(a);
            let d = [a[0] - g[0], a[1] - g[1], a[2] - g[2]];
            for k in 0..3 {
                self.gravity.as_mut().unwrap()[k] += alpha * (a[k] - g[k]);
            }

            let side = (d[1] * d[1] + d[2] * d[2]).sqrt();
            let limit = if moving { BUMP_DRIVING_G } else { BUMP_STILL_G };
            if side >= limit
                && self
                    .last_bump
                    .is_none_or(|t| at.duration_since(t) >= BUMP_QUIET)
            {
                self.last_bump = Some(at);
                // Pushed one way means hit from the other.
                let from = if d[2].abs() >= d[1].abs() {
                    if d[2] < 0.0 { "front" } else { "back" }
                } else if d[1] < 0.0 {
                    "right"
                } else {
                    "left"
                };
                out.push(Felt::Bump {
                    g: (side * 100.0).round() / 100.0,
                    side: from,
                });
            }

            if moving || d[0] < LIFT_G {
                self.up_since = None;
            } else {
                let since = *self.up_since.get_or_insert(at);
                if at.duration_since(since) >= LIFT_HOLD
                    && self
                        .last_lift
                        .is_none_or(|t| at.duration_since(t) >= LIFT_QUIET)
                {
                    self.last_lift = Some(at);
                    self.up_since = None;
                    out.push(Felt::Lift);
                }
            }
        }
        out
    }
}

/// Feel the stream. Spawned once by `crate::start`.
pub async fn run(robot: Robot) {
    let mut rx = robot.hub.imu.subscribe();
    let mut watch = Watch::default();
    loop {
        let chunk = match rx.recv().await {
            Ok(c) => c,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        let moving = robot.is_moving();
        for felt in watch.push(&chunk.samples, chunk.rate_hz, moving, Instant::now()) {
            match felt {
                Felt::Bump { g, side } => {
                    tracing::info!(
                        "bump: {g} g from the {side}{}",
                        if moving { " while driving" } else { "" }
                    );
                    if moving {
                        robot.hub.send(ToRobot::Stop).await.ok();
                        robot
                            .hub
                            .push_event("drive_stopped", json!({ "reason": "bump", "side": side }))
                            .await;
                    }
                    robot
                        .hub
                        .push_event("motion", json!({ "kind": "bump", "g": g, "side": side }))
                        .await;
                }
                Felt::Lift => {
                    robot
                        .hub
                        .push_event(
                            "motion",
                            json!({ "kind": "pickup", "felt": "accelerometer" }),
                        )
                        .await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn still(n: usize) -> Vec<[i16; 3]> {
        vec![[1000, 0, 0]; n]
    }

    #[test]
    fn standing_still_is_nothing() {
        let mut w = Watch::default();
        let t = Instant::now();
        assert!(w.push(&still(100), 50, false, t).is_empty());
    }

    #[test]
    fn a_knock_is_a_bump_once() {
        let mut w = Watch::default();
        let t = Instant::now();
        w.push(&still(50), 50, false, t);
        let mut s = still(10);
        s[3] = [1000, 0, 600]; // pushed backward: hit from the front... of z
        s[4] = [1000, 0, -300];
        let felt = w.push(&s, 50, false, t + Duration::from_millis(200));
        assert_eq!(felt.len(), 1, "{felt:?}");
        assert!(matches!(felt[0], Felt::Bump { side: "back", .. }));
    }

    #[test]
    fn small_jolts_count_only_while_driving() {
        let t = Instant::now();
        let mut s = still(10);
        s[5] = [1000, 350, 0];
        let mut w = Watch::default();
        w.push(&still(50), 50, false, t);
        assert!(
            w.push(&s, 50, false, t + Duration::from_millis(200))
                .is_empty()
        );
        let mut w = Watch::default();
        w.push(&still(50), 50, true, t);
        let felt = w.push(&s, 50, true, t + Duration::from_millis(200));
        assert!(
            matches!(felt[..], [Felt::Bump { side: "left", .. }]),
            "{felt:?}"
        );
    }

    #[test]
    fn a_held_upward_push_is_a_lift() {
        let mut w = Watch::default();
        let t = Instant::now();
        w.push(&still(50), 50, false, t);
        // A footstep: one sample up.
        let mut s = still(10);
        s[2] = [1200, 0, 0];
        assert!(
            w.push(&s, 50, false, t + Duration::from_millis(200))
                .is_empty()
        );
        // Lifted: up for a quarter second.
        let lift = vec![[1200, 0, 0]; 12];
        let felt = w.push(&lift, 50, false, t + Duration::from_millis(500));
        assert_eq!(felt, vec![Felt::Lift]);
        // Not again right away.
        assert!(
            w.push(&lift, 50, false, t + Duration::from_millis(800))
                .is_empty()
        );
    }
}
