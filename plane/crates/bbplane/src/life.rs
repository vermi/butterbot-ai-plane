//! The robot's own initiative while an agent runs it.
//!
//! Between commands, `hear` and `reply` also return *nudges*: someone picked the robot up or shook
//! it, someone familiar arrived, an animal came into view, or it has been quiet for a while. The agent decides what to do with them (look around, wander, start some
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
/// Being picked up or shaken not reacted to within this long is old news.
const HANDLED_FRESH: Duration = Duration::from_secs(20);
/// A sound not reacted to within this long is old news.
const HEARD_FRESH: Duration = Duration::from_secs(30);
/// A phone rings for about this long.
const CALL_FRESH: Duration = Duration::from_secs(25);
/// A battery warning not passed on within this long waits for the next one.
const BATTERY_FRESH: Duration = Duration::from_secs(300);
/// Someone keeps talking: do not end a session in the middle of a conversation.
const SESSION_END_QUIET: Duration = Duration::from_secs(90);

/// Ideas offered with an idle nudge; the agent is free to ignore them.
const SUGGESTIONS_IN_VIEW: &[&str] = &[
    "start a little chit-chat with whoever is in view",
    "comment on something you notice in the picture",
    "ask the person in view how their day is going",
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
    /// The paired phone is ringing.
    Call { from: String },
    /// Someone handled the robot: `pickup`, `putdown`, `shake`, `fall` or `upside_down`.
    Handled { how: String },
    /// The battery is low (`plugged_in` false: ask to be charged), or the robot was just plugged
    /// in after such a warning.
    Battery { percent: u32, plugged_in: bool },
    /// A recognised person came into view.
    Arrived { name: String },
    /// Something worth noticing (a cat, a dog...) came into view.
    Noticed { what: String },
    /// A sound worth noticing (a bark, the doorbell, an alarm...).
    Heard { what: String },
    /// Nobody has said anything to the robot for a while.
    Idle {
        quiet_for_minutes: u64,
        suggestion: String,
    },
}

struct State {
    focus_until: Option<Instant>,
    last_activity: Instant,
    wait_in_view: Duration,
    wait_alone: Duration,
    arrivals: VecDeque<(String, Instant)>,
    noticed: VecDeque<(String, Instant)>,
    handled: Option<(String, Instant)>,
    call: Option<(String, Instant)>,
    /// What, whether it is urgent (an alarm), when.
    heard: Option<(String, bool, Instant)>,
    battery: Option<(u32, bool, Instant)>,
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
            noticed: VecDeque::new(),
            handled: None,
            call: None,
            heard: None,
            battery: None,
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
        s.noticed.clear();
        s.handled = None;
        s.call = None;
        s.heard.take_if(|(_, urgent, _)| !*urgent);
        s.battery = None;
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

    /// The robot was picked up, shaken, dropped or turned over (the latest one counts).
    pub fn handled(&self, how: &str) {
        if !self.cfg.enabled || self.focused().is_some() {
            return;
        }
        self.state.lock().unwrap().handled = Some((how.to_string(), Instant::now()));
    }

    /// A sound worth noticing was heard. `urgent` ones (alarms) come through focus mode.
    pub fn heard(&self, what: &str, urgent: bool) {
        if !self.cfg.enabled || (!urgent && self.focused().is_some()) {
            return;
        }
        self.state.lock().unwrap().heard = Some((what.to_string(), urgent, Instant::now()));
    }

    /// The paired phone is ringing.
    pub fn call(&self, from: &str) {
        if !self.cfg.enabled || self.focused().is_some() {
            return;
        }
        self.state.lock().unwrap().call = Some((from.to_string(), Instant::now()));
    }

    /// Tell the agent about the battery (see `Nudge::Battery`).
    pub fn battery(&self, percent: u32, plugged_in: bool) {
        if !self.cfg.enabled || self.focused().is_some() {
            return;
        }
        self.state.lock().unwrap().battery = Some((percent, plugged_in, Instant::now()));
    }

    /// Something worth noticing came into view.
    pub fn noticed(&self, what: &str) {
        if !self.cfg.enabled || self.focused().is_some() {
            return;
        }
        let mut s = self.state.lock().unwrap();
        s.noticed.retain(|(w, _)| w != what);
        s.noticed.push_back((what.to_string(), Instant::now()));
    }

    /// A nudge that is due now. `anyone_in_view`: a face (known or not) is in front of the camera.
    pub fn poll(&self, anyone_in_view: bool) -> Option<Nudge> {
        if !self.cfg.enabled {
            return None;
        }
        let focused = self.focused().is_some();
        let mut s = self.state.lock().unwrap();
        s.heard.take_if(|(_, _, at)| at.elapsed() >= HEARD_FRESH);
        // Alarms come through focus mode.
        if let Some((what, _, _)) = s.heard.take_if(|(_, urgent, _)| *urgent || !focused) {
            s.last_activity = Instant::now();
            reschedule(&self.cfg, &mut s);
            return Some(Nudge::Heard { what });
        }
        if focused {
            return None;
        }
        if let Some((from, at)) = s.call.take()
            && at.elapsed() < CALL_FRESH
        {
            s.last_activity = Instant::now();
            reschedule(&self.cfg, &mut s);
            return Some(Nudge::Call { from });
        }
        if let Some((how, at)) = s.handled.take()
            && at.elapsed() < HANDLED_FRESH
        {
            s.last_activity = Instant::now();
            reschedule(&self.cfg, &mut s);
            return Some(Nudge::Handled { how });
        }
        if let Some((percent, plugged_in, at)) = s.battery.take()
            && at.elapsed() < BATTERY_FRESH
        {
            s.last_activity = Instant::now();
            reschedule(&self.cfg, &mut s);
            return Some(Nudge::Battery {
                percent,
                plugged_in,
            });
        }
        s.arrivals.retain(|(_, at)| at.elapsed() < ARRIVAL_FRESH);
        if let Some((name, _)) = s.arrivals.pop_front() {
            s.last_activity = Instant::now();
            reschedule(&self.cfg, &mut s);
            return Some(Nudge::Arrived { name });
        }
        s.noticed.retain(|(_, at)| at.elapsed() < ARRIVAL_FRESH);
        if let Some((what, _)) = s.noticed.pop_front() {
            s.last_activity = Instant::now();
            reschedule(&self.cfg, &mut s);
            return Some(Nudge::Noticed { what });
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
        let suggestion = if anyone_in_view {
            // Something they care about, twice as often as any one other idea.
            let news = |rng: &mut u64| match self.cfg.interests.as_slice() {
                [] => "share something interesting you read about recently".to_string(),
                topics => format!(
                    "tell them about an interesting recent development in {} (look it up with web_search first)",
                    topics[next_rand(rng) as usize % topics.len()]
                ),
            };
            let n = SUGGESTIONS_IN_VIEW.len() as u64 + 2;
            match next_rand(&mut s.rng) % n {
                i if (i as usize) < SUGGESTIONS_IN_VIEW.len() => {
                    SUGGESTIONS_IN_VIEW[i as usize].to_string()
                }
                _ => news(&mut s.rng),
            }
        } else {
            SUGGESTIONS_ALONE[next_rand(&mut s.rng) as usize % SUGGESTIONS_ALONE.len()].to_string()
        };
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
    async fn idle_chat_brings_up_their_interests() {
        let life = Life::new(LifeConfig {
            interests: vec!["quantum computing".into()],
            ..cfg([60, 60], [60, 60])
        });
        let mut ideas = Vec::new();
        for _ in 0..40 {
            tokio::time::advance(Duration::from_secs(61)).await;
            if let Some(Nudge::Idle { suggestion, .. }) = life.poll(true) {
                ideas.push(suggestion);
            }
        }
        assert_eq!(ideas.len(), 40);
        assert!(ideas.iter().any(|i| i.contains("quantum computing")));
        assert!(ideas.iter().all(|i| !i.contains("joke")));
    }

    #[tokio::test(start_paused = true)]
    async fn being_handled_comes_first_and_goes_stale() {
        let life = Life::new(cfg([600, 600], [600, 600]));
        life.arrived("Justin");
        life.noticed("dog");
        life.handled("pickup");
        life.handled("shake"); // the latest counts
        assert!(matches!(life.poll(true), Some(Nudge::Handled { how }) if how == "shake"));
        assert!(matches!(life.poll(true), Some(Nudge::Arrived { .. })));
        assert!(matches!(life.poll(true), Some(Nudge::Noticed { what }) if what == "dog"));
        assert!(life.poll(true).is_none());

        life.handled("pickup");
        tokio::time::advance(Duration::from_secs(21)).await;
        assert!(life.poll(true).is_none(), "put down again long ago");

        life.focus(Duration::from_secs(60));
        life.handled("pickup");
        assert!(life.poll(true).is_none(), "quiet in focus mode");
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
