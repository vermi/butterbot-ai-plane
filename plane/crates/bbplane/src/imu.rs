//! The robot's accelerometer, streamed by firmware 0.6 at 50 Hz (`bbproto::frame::ImuChunk`):
//! bumps and lifts are felt here, where they are easy to tune.
//!
//! - A bump: a sharp sideways or front-back jolt. While the robot drives it stops at once (a
//!   collision the short-sighted front sensor missed) and the drive ends early (`bump`);
//!   standing still it is news for the agent (someone knocked it), but only after a calm
//!   moment: being lifted, carried or docked jolts it again and again, and that is no bump.
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

/// Horizontal jolt (g) that counts as a bump, standing and driving. Driving shakes the robot by
/// itself, up to about 0.32 g at full speed (at 0.3 nearly every drive stopped after a moment);
/// the front sensor catches most of what it would drive into.
const BUMP_STILL_G: f32 = 0.4;
const BUMP_DRIVING_G: f32 = 0.45;
/// The motors starting jolt it about half a g: no bump this soon after a drive or turn starts.
const MOTOR_START: Duration = Duration::from_millis(500);
/// Upward acceleration (g), held for `LIFT_HOLD`, that feels like a lift.
const LIFT_G: f32 = 0.08;
const LIFT_HOLD: Duration = Duration::from_millis(120);
/// After one, the next of a kind waits this long (a bump rings on for a moment).
const BUMP_QUIET: Duration = Duration::from_millis(1500);
/// Standing still, a bump counts only after this long without any jolt of `ACTIVE_G`: someone
/// handling the robot jolts it continually.
const STILL_REARM: Duration = Duration::from_secs(5);
const ACTIVE_G: f32 = 0.2;
/// A standing bump waits this long before it is news, and is dropped if the charger came or
/// went around it (docking and undocking jolt the robot), the robot is off the floor, or it was
/// talking: its speaker thumps the body as it starts (one sample of half a g).
const STILL_CONFIRM: Duration = Duration::from_secs(1);
const CHARGER_SETTLE: Duration = Duration::from_secs(3);
const OWN_VOICE_SETTLE: Duration = Duration::from_millis(1500);
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
    /// The last jolt standing still (any of `ACTIVE_G` or more).
    last_jolt: Option<Instant>,
    /// When the motors last started (a drive or a turn).
    pub started: Option<Instant>,
    /// The largest sideways jolt since the last look (g), for tuning.
    pub peak: f32,
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
            self.peak = self.peak.max(side);
            let limit = if moving { BUMP_DRIVING_G } else { BUMP_STILL_G };
            let armed = if moving {
                self.last_bump
                    .is_none_or(|t| at.duration_since(t) >= BUMP_QUIET)
            } else {
                self.last_jolt
                    .is_none_or(|t| at.duration_since(t) >= STILL_REARM)
            };
            let starting = moving && self.started.is_some_and(|t| at < t + MOTOR_START);
            if !moving && side >= ACTIVE_G {
                self.last_jolt = Some(at);
            }
            if side >= limit && armed && !starting {
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
    let mut prev: Vec<[i16; 3]> = Vec::new();
    // A standing bump waiting to be confirmed: when, how strong, from where.
    let mut pending: Option<(Instant, f32, &'static str)> = None;
    let mut plugged = robot.plugged_in().await;
    let mut plug_changed: Option<Instant> = None;
    // The last time its speaker was playing.
    let mut talked_at: Option<Instant> = None;
    let mut peak_at = Instant::now();
    loop {
        let chunk = match rx.recv().await {
            Ok(c) => c,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        let moving = robot.is_moving();
        watch.started = robot.motors_started();
        let now = Instant::now();
        if now.duration_since(peak_at) >= Duration::from_secs(30) {
            // Standing still it stays under 0.1 g, camera on or off.
            tracing::debug!("largest sideways jolt in 30 s: {:.2} g", watch.peak);
            watch.peak = 0.0;
            peak_at = now;
        }
        if robot.hub.status.lock().await.speaker_playing {
            talked_at = Some(now);
        }
        let p = robot.plugged_in().await;
        if p != plugged {
            if plugged.is_some() {
                plug_changed = Some(now);
            }
            plugged = p;
        }
        if let Some((at, g, side)) = pending
            && now.duration_since(at) >= STILL_CONFIRM
        {
            pending = None;
            let near = |t: Instant, d: Duration| t.max(at).duration_since(t.min(at)) < d;
            let docking = plug_changed.is_some_and(|c| near(c, CHARGER_SETTLE));
            let talking = talked_at.is_some_and(|t| near(t, OWN_VOICE_SETTLE));
            if docking || talking || !robot.on_the_floor() {
                tracing::info!(
                    "bump: {g} g from the {side} ignored ({})",
                    if docking {
                        "the charger"
                    } else if talking {
                        "its own voice"
                    } else {
                        "off the floor"
                    }
                );
            } else {
                robot
                    .hub
                    .push_event("motion", json!({ "kind": "bump", "g": g, "side": side }))
                    .await;
            }
        }
        for felt in watch.push(&chunk.samples, chunk.rate_hz, moving, now) {
            match felt {
                Felt::Bump { g, side } => {
                    tracing::info!(
                        "bump: {g} g from the {side}{} (samples, mg: {prev:?} {:?})",
                        if moving { " while driving" } else { "" },
                        chunk.samples
                    );
                    if moving {
                        robot.hub.send(ToRobot::Stop).await.ok();
                        robot
                            .hub
                            .push_event("drive_stopped", json!({ "reason": "bump", "side": side }))
                            .await;
                        robot
                            .hub
                            .push_event("motion", json!({ "kind": "bump", "g": g, "side": side }))
                            .await;
                    } else {
                        pending = Some((now, g, side));
                    }
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
        prev = chunk.samples.clone();
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
    fn driving_shakes_it_without_bumps() {
        let t = Instant::now();
        let jolt = |mg| {
            let mut s = still(10);
            s[5] = [1000, mg, 0];
            s
        };
        // Its own shaking at full speed.
        let mut w = Watch::default();
        w.push(&still(50), 50, true, t);
        assert!(
            w.push(&jolt(350), 50, true, t + Duration::from_secs(2))
                .is_empty()
        );
        // A collision.
        let felt = w.push(&jolt(600), 50, true, t + Duration::from_secs(4));
        assert!(
            matches!(felt[..], [Felt::Bump { side: "left", .. }]),
            "{felt:?}"
        );
        // The same jolt just as the motors start.
        let mut w = Watch::default();
        w.push(&still(50), 50, true, t);
        w.started = Some(t + Duration::from_secs(2));
        assert!(
            w.push(&jolt(600), 50, true, t + Duration::from_millis(2200))
                .is_empty()
        );
    }

    #[test]
    fn handling_is_one_bump_not_many() {
        let mut w = Watch::default();
        let t = Instant::now();
        w.push(&still(50), 50, false, t);
        // Jolted every 2 s for 20 s (lifted, carried, docked).
        let mut felt = Vec::new();
        for k in 1..=10u64 {
            let mut s = still(100);
            s[10] = [1000, 0, 600];
            felt.extend(w.push(&s, 50, false, t + Duration::from_secs(2 * k)));
        }
        assert_eq!(felt.len(), 1, "{felt:?}");
        // After a calm spell, a knock counts again.
        w.push(&still(400), 50, false, t + Duration::from_secs(28));
        let mut s = still(10);
        s[3] = [1000, 0, -600];
        let felt = w.push(&s, 50, false, t + Duration::from_millis(28_200));
        assert!(
            matches!(felt[..], [Felt::Bump { side: "front", .. }]),
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
