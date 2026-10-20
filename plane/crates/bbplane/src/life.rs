//! The robot's own initiative while an agent runs it.
//!
//! Between commands, `hear` and `reply` also return *nudges*: someone familiar arrived, or it has
//! been quiet for a while. The agent decides what to do with them (look around, wander, start some
//! chit-chat, or rest). Nudges come every few minutes while someone is in view and rarely when the
//! room is empty. In focus mode there are none until the robot is called by name or the focus time
//! runs out.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tokio::time::Instant;

use crate::config::LifeConfig;

/// An arrival not picked up within this long is old news (they may have left again).
const ARRIVAL_FRESH: Duration = Duration::from_secs(60);
/// Someone keeps talking: do not end a session in the middle of a conversation.
const SESSION_END_QUIET: Duration = Duration::from_secs(90);

/// Ideas offered with an idle nudge; the agent is free to ignore them.
const SUGGESTIONS_IN_VIEW: &[&str] = &[
    "start a little chit-chat with whoever is in view",
    "comment on something you notice in the picture",
    "ask the person in view how their day is going",
    "share a fun fact or a robot joke",
    "look around (turn a little) and say what you find",
    "stay quiet for now",
];
const SUGGESTIONS_ALONE: &[&str] = &[
    "look around (turn a little) and see what has changed",
    "wander a short way and check out a new spot",
    "stay quiet and rest",
    "stay quiet and rest",
];

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "nudge", rename_all = "snake_case")]
pub enum Nudge {
    /// A recognised person came into view.
    Arrived { name: String },
    /// Nobody has said anything to the robot for a while.
    Idle {
        quiet_for_minutes: u64,
        suggestion: &'static str,
    },
}

struct State {
    focus_until: Option<Instant>,
    last_activity: Instant,
    wait_in_view: Duration,
    wait_alone: Duration,
    arrivals: VecDeque<(String, Instant)>,
    session_started: Instant,
    rng: u64,
}

pub struct Life {
    cfg: LifeConfig,
    state: Mutex<State>,
    /// An agent runs the robot (`crate::agent`): it greets people itself, so the plane does not.
    pub agent_running: std::sync::atomic::AtomicBool,
}

impl Life {
    pub fn new(cfg: LifeConfig) -> Life {
        let seed = crate::hub::now_ms() | 1;
        let mut state = State {
            focus_until: None,
            last_activity: Instant::now(),
            wait_in_view: Duration::ZERO,
            wait_alone: Duration::ZERO,
            arrivals: VecDeque::new(),
            session_started: Instant::now(),
            rng: seed,
        };
        reschedule(&cfg, &mut state);
        Life {
            cfg,
            state: Mutex::new(state),
            agent_running: Default::default(),
        }
    }

    pub fn owner(&self) -> &str {
        &self.cfg.owner
    }

    /// Someone talked to the robot, it spoke, or it acted on a nudge: idle time starts over.
    pub fn activity(&self) {
        let mut s = self.state.lock().unwrap();
        s.last_activity = Instant::now();
        reschedule(&self.cfg, &mut s);
    }

    /// Stay quiet (no nudges) for `d`, or until called by name.
    pub fn focus(&self, d: Duration) {
        let mut s = self.state.lock().unwrap();
        s.focus_until = Some(Instant::now() + d);
        s.arrivals.clear();
    }

    pub fn unfocus(&self) {
        let mut s = self.state.lock().unwrap();
        s.focus_until = None;
        s.last_activity = Instant::now();
        reschedule(&self.cfg, &mut s);
    }

    /// Time left in focus mode.
    pub fn focused(&self) -> Option<Duration> {
        let mut s = self.state.lock().unwrap();
        match s.focus_until {
            Some(t) if Instant::now() < t => Some(t - Instant::now()),
            Some(_) => {
                // Focus ran out: back to normal, starting the idle clock now.
                s.focus_until = None;
                s.last_activity = Instant::now();
                reschedule(&self.cfg, &mut s);
                None
            }
            None => None,
        }
    }

    /// A recognised person came into view.
    pub fn arrived(&self, name: &str) {
        if !self.cfg.enabled || self.focused().is_some() {
            return;
        }
        let mut s = self.state.lock().unwrap();
        s.arrivals.retain(|(n, _)| n != name);
        s.arrivals.push_back((name.to_string(), Instant::now()));
    }

    /// A nudge that is due now. `anyone_in_view`: a face (known or not) is in front of the camera.
    pub fn poll(&self, anyone_in_view: bool) -> Option<Nudge> {
        if !self.cfg.enabled || self.focused().is_some() {
            return None;
        }
        let mut s = self.state.lock().unwrap();
        s.arrivals.retain(|(_, at)| at.elapsed() < ARRIVAL_FRESH);
        if let Some((name, _)) = s.arrivals.pop_front() {
            s.last_activity = Instant::now();
            reschedule(&self.cfg, &mut s);
            return Some(Nudge::Arrived { name });
        }
        let quiet = s.last_activity.elapsed();
        let due = if anyone_in_view {
            s.wait_in_view
        } else {
            s.wait_alone
        };
        if quiet < due {
            return None;
        }
        let list = if anyone_in_view {
            SUGGESTIONS_IN_VIEW
        } else {
            SUGGESTIONS_ALONE
        };
        let suggestion = list[next_rand(&mut s.rng) as usize % list.len()];
        s.last_activity = Instant::now();
        reschedule(&self.cfg, &mut s);
        Some(Nudge::Idle {
            quiet_for_minutes: quiet.as_secs() / 60,
            suggestion,
        })
    }

    /// The agent's session has run long enough and nothing is going on: time for a fresh one.
    pub fn session_over(&self) -> bool {
        let s = self.state.lock().unwrap();
        self.cfg.session_minutes > 0
            && s.session_started.elapsed() >= Duration::from_secs(self.cfg.session_minutes * 60)
            && s.last_activity.elapsed() >= SESSION_END_QUIET
    }

    /// A new agent session started.
    pub fn session_started(&self) {
        self.state.lock().unwrap().session_started = Instant::now();
    }
}

fn reschedule(cfg: &LifeConfig, s: &mut State) {
    let pick = |rng: &mut u64, [lo, hi]: [u64; 2]| {
        let (lo, hi) = (lo.min(hi), lo.max(hi));
        Duration::from_secs(lo + next_rand(rng) % (hi - lo + 1))
    };
    s.wait_in_view = pick(&mut s.rng, cfg.nudge_in_view_secs);
    s.wait_alone = pick(&mut s.rng, cfg.nudge_alone_secs);
}

/// xorshift64: plenty for picking intervals and suggestions.
fn next_rand(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(in_view: [u64; 2], alone: [u64; 2]) -> LifeConfig {
        LifeConfig {
            nudge_in_view_secs: in_view,
            nudge_alone_secs: alone,
            ..Default::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn nudges_sooner_with_company_and_never_in_focus() {
        let life = Life::new(cfg([60, 60], [600, 600]));
        assert!(life.poll(true).is_none());
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(life.poll(false).is_none(), "alone waits longer");
        assert!(matches!(life.poll(true), Some(Nudge::Idle { .. })));
        assert!(life.poll(true).is_none(), "rescheduled after a nudge");

        life.focus(Duration::from_secs(3600));
        life.arrived("Justin");
        tokio::time::advance(Duration::from_secs(1800)).await;
        assert!(life.poll(true).is_none());
        tokio::time::advance(Duration::from_secs(1801)).await;
        assert!(life.focused().is_none(), "focus expires");

        life.arrived("Justin");
        life.arrived("Justin");
        assert!(matches!(life.poll(false), Some(Nudge::Arrived { name }) if name == "Justin"));
        assert!(life.poll(false).is_none());

        // Nobody picked it up for a while: old news.
        life.arrived("Mono");
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(!matches!(life.poll(false), Some(Nudge::Arrived { .. })));
    }

    #[tokio::test(start_paused = true)]
    async fn session_ends_only_when_quiet() {
        let life = Life::new(LifeConfig {
            session_minutes: 1,
            ..cfg([1000, 1000], [1000, 1000])
        });
        tokio::time::advance(Duration::from_secs(50)).await;
        life.activity();
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(!life.session_over(), "conversation going on");
        tokio::time::advance(Duration::from_secs(70)).await;
        assert!(life.session_over());
        life.session_started();
        assert!(!life.session_over());
    }
}
