//! The status LED in AI mode: what the robot is up to, at a glance. The robot itself shows when it
//! is talking (bright), connecting and when it cannot reach the plane; the plane picks the rest
//! from the agent, the ears and focus mode. The LED is off, dim or bright, and its dim (a pull-up
//! glow) is faint, so states differ by blink rhythm and dim is only a background hint.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bbproto::ctrl::{Rhythm, ToRobot};

use crate::control::Robot;

/// An agent that has not asked for the next event for this long is gone (crashed or stopped).
const AGENT_GONE: Duration = Duration::from_secs(60);
/// Stop showing "thinking" if the agent never comes back for the next event.
const THINKING_MAX: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Waiting for its name: faint glow, a short bright tick every 3 s.
    Listening,
    /// In a conversation, no name needed: bright, with a short wink off every 2 s.
    Attentive,
    /// Heard a command, the agent is working on it: quick flicker.
    Thinking,
    /// Focus mode: faint glow, a brief tick every 10 s.
    Focus,
    /// AI mode without an agent (paused, not running): two quick ticks every 4 s.
    NoBrain,
}

impl State {
    pub fn rhythm(self) -> Rhythm {
        let r = |on_ms, off_ms, count, period_ms, base, flash| Rhythm {
            on_ms,
            off_ms,
            count,
            period_ms,
            base,
            flash,
        };
        match self {
            State::Listening => r(80, 0, 1, 3000, Rhythm::DIM, Rhythm::BRIGHT),
            State::Attentive => r(150, 0, 1, 2000, Rhythm::BRIGHT, Rhythm::OFF),
            State::Thinking => r(100, 0, 1, 200, Rhythm::OFF, Rhythm::BRIGHT),
            State::Focus => r(40, 0, 1, 10000, Rhythm::DIM, Rhythm::BRIGHT),
            State::NoBrain => r(60, 200, 2, 4000, Rhythm::OFF, Rhythm::BRIGHT),
        }
    }
}

/// What the light needs to know about the agent.
#[derive(Default)]
pub struct Light {
    /// A command is waiting for (or being worked on by) the agent since then.
    thinking_since: Mutex<Option<Instant>>,
    /// Agents waiting in `Robot::next_event` right now.
    waiting: AtomicUsize,
    /// When an agent last stopped waiting for an event.
    last_seen: Mutex<Option<Instant>>,
}

impl Light {
    /// Someone gave the agent a command.
    pub fn thinking(&self) {
        *self.thinking_since.lock().unwrap() = Some(Instant::now());
    }

    /// The agent is waiting for the next event (so it has finished the last one) until the guard
    /// is dropped.
    pub fn waiting(&self) -> Waiting<'_> {
        self.waiting.fetch_add(1, Ordering::Relaxed);
        *self.thinking_since.lock().unwrap() = None;
        Waiting(self)
    }

    fn agent_present(&self) -> bool {
        self.waiting.load(Ordering::Relaxed) > 0
            || self
                .last_seen
                .lock()
                .unwrap()
                .is_some_and(|t| t.elapsed() < AGENT_GONE)
    }

    /// The agent is working on something someone said (it may speak any moment).
    pub fn answering(&self) -> bool {
        self.is_thinking()
    }

    fn is_thinking(&self) -> bool {
        self.thinking_since
            .lock()
            .unwrap()
            .is_some_and(|t| t.elapsed() < THINKING_MAX)
    }
}

pub struct Waiting<'a>(&'a Light);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::Relaxed);
        *self.0.last_seen.lock().unwrap() = Some(Instant::now());
    }
}

/// What the light should show now.
pub fn state(robot: &Robot) -> State {
    let paused = robot.hub.config.agent.enabled && robot.agent.paused.load(Ordering::Relaxed);
    if paused || !robot.light.agent_present() {
        State::NoBrain
    } else if robot.light.is_thinking() {
        State::Thinking
    } else if robot.life.focused().is_some() {
        State::Focus
    } else if robot.ears.window_open() {
        State::Attentive
    } else {
        State::Listening
    }
}

/// What the handheld controller's top bar shows (see `ToRobot::AiStatus`).
fn ai_status(robot: &Robot, s: State) -> ToRobot {
    use crate::life::Activity;
    ToRobot::AiStatus {
        mode: match s {
            State::Listening => 1,
            State::Attentive => 2,
            State::Thinking => 3,
            State::Focus => 4,
            State::NoBrain => 5,
        },
        activity: match robot.life.activity_level() {
            Activity::Quiet => 0,
            Activity::Calm => 1,
            Activity::Normal => 2,
            Activity::Lively => 3,
        },
        focus_minutes: robot
            .life
            .focused()
            .map(|d| d.as_secs().div_ceil(60).min(255) as u8)
            .unwrap_or(0),
    }
}

/// Keep the robot's status LED (and the controller's AI status) in step with `state`. Spawned
/// once by `crate::start`.
pub async fn run(robot: Robot) {
    let mut shown: Option<State> = None;
    let mut shown_status: Option<ToRobot> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(150));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        // Sent again whenever the robot (re)enters AI mode: it starts from its own default.
        if !robot.hub.is_connected().await || robot.mode().await.as_deref() != Some("remote") {
            shown = None;
            shown_status = None;
            continue;
        }
        let s = state(&robot);
        let status = ai_status(&robot, s);
        if shown_status.as_ref() != Some(&status) && robot.hub.send(status.clone()).await.is_ok() {
            shown_status = Some(status);
        }
        if shown == Some(s) {
            continue;
        }
        match robot.hub.send(ToRobot::StatusLight(s.rhythm())).await {
            Ok(()) => {
                tracing::debug!("status light: {s:?}");
                shown = Some(s);
            }
            Err(e) => tracing::debug!("status light: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_lasts_until_the_agent_waits_again() {
        let l = Light::default();
        assert!(!l.agent_present());
        {
            let _w = l.waiting();
            assert!(l.agent_present());
            l.thinking(); // a command arrived and was handed over
        }
        assert!(l.is_thinking());
        assert!(l.agent_present()); // seen just now
        let _w = l.waiting();
        assert!(!l.is_thinking());
    }

    #[test]
    fn states_look_different() {
        use State::*;
        let all = [Listening, Attentive, Thinking, Focus, NoBrain];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.rhythm(), b.rhythm(), "{a:?} and {b:?}");
            }
        }
    }
}
