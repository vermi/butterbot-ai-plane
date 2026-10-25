//! The status LED: plays a rhythm (see `bbproto::ctrl::Rhythm`) on its own thread, waking only
//! when the LED changes.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use bbproto::ctrl::Rhythm;

use crate::pins::exp;
use crate::tca::Tca;
use crate::util::{self, millis, Stack};

/// The LED service ticks at 10 ms at best; flashes shorter than this would not show.
const MIN_MS: u16 = 20;

pub const fn rhythm(
    on_ms: u16,
    off_ms: u16,
    count: u8,
    period_ms: u16,
    base: f32,
    flash: f32,
) -> Rhythm {
    Rhythm {
        on_ms,
        off_ms,
        count,
        period_ms,
        base,
        flash,
    }
}

/// Flashes long enough to see, all inside the period.
pub fn sane(mut r: Rhythm) -> Rhythm {
    r.base = r.base.clamp(0.0, 1.0);
    r.flash = r.flash.clamp(0.0, 1.0);
    if r.count == 0 {
        return r;
    }
    r.count = r.count.min(5);
    r.on_ms = r.on_ms.clamp(MIN_MS, 5000);
    r.off_ms = if r.count > 1 {
        r.off_ms.clamp(MIN_MS, 5000)
    } else {
        r.off_ms.min(5000)
    };
    let busy = (r.on_ms as u32 + r.off_ms as u32) * r.count as u32;
    r.period_ms = (r.period_ms as u32).clamp(busy, 60_000) as u16;
    r
}

fn level(v: f32) -> u8 {
    if v <= 0.0 {
        0
    } else if v < 0.5 {
        1
    } else {
        2
    }
}

struct State {
    rhythm: Rhythm,
    since: u64,
}

pub struct Led {
    state: Mutex<State>,
    changed: Condvar,
}

impl Led {
    pub fn start(tca: Arc<Tca>, initial: Rhythm) -> Arc<Led> {
        let led = Arc::new(Led {
            state: Mutex::new(State {
                rhythm: sane(initial),
                since: millis(),
            }),
            changed: Condvar::new(),
        });
        let l = led.clone();
        util::spawn(c"led", 3072, 4, Stack::Psram, move || l.run(&tca));
        led
    }

    pub fn show(&self, r: Rhythm) {
        let r = sane(r);
        let mut s = self.state.lock().unwrap();
        if s.rhythm == r {
            return;
        }
        s.rhythm = r;
        s.since = millis();
        self.changed.notify_one();
    }

    fn run(&self, tca: &Tca) {
        let mut s = self.state.lock().unwrap();
        loop {
            let r = s.rhythm;
            let (lit, next) = if r.count == 0 {
                (false, 1000)
            } else {
                let t = ((millis() - s.since) % r.period_ms.max(1) as u64) as u32;
                let slot = r.on_ms as u32 + r.off_ms as u32;
                if t < slot * r.count as u32 {
                    let in_slot = t % slot;
                    let lit = in_slot < r.on_ms as u32;
                    (
                        lit,
                        if lit {
                            r.on_ms as u32 - in_slot
                        } else {
                            slot - in_slot
                        },
                    )
                } else {
                    (false, r.period_ms as u32 - t)
                }
            };
            tca.set_level(exp::LED, level(if lit { r.flash } else { r.base }));
            s = self
                .changed
                .wait_timeout(s, Duration::from_millis(next.max(1) as u64))
                .unwrap()
                .0;
        }
    }
}
