//! The SC7A20 accelerometer (a LIS3DH clone at I2C 0x18): 100 Hz into its FIFO, read every
//! 100 ms. Gives the pose (is the robot upright?) and, while the floor sensor says the robot is
//! off the ground, the stock motion events: pickup, shake, fall (on a side) and upside down.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use esp_idf_svc::sys::EspError;

use crate::i2c::{Bus, Dev};
use crate::pins;
use crate::util::{self, millis, sleep_ms, Stack};

const WHO_AM_I: u8 = 0x0F;
const SC7A20_ID: u8 = 0x11;
const CTRL_REG1: u8 = 0x20;
const CTRL_REG4: u8 = 0x23;
const CTRL_REG5: u8 = 0x24;
const INT1_SRC: u8 = 0x31;
const OUT_X_L: u8 = 0x28;
const FIFO_CTRL: u8 = 0x2E;
const FIFO_SRC: u8 = 0x2F;
/// Multi-byte reads need the address' top bit set.
const AUTO_INC: u8 = 0x80;

const BATCH_MS: u64 = 100;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sample {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionKind {
    Fall,
    UpsideDown,
    Pickup,
    Shake,
}

impl MotionKind {
    pub fn name(self) -> &'static str {
        match self {
            MotionKind::Fall => "fall",
            MotionKind::UpsideDown => "upside_down",
            MotionKind::Pickup => "pickup",
            MotionKind::Shake => "shake",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum MotionEvent {
    Motion(MotionKind),
    /// Upright and still again after being lifted: the floor sensor may rebaseline.
    SetDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Pose {
    Straight,
    Left,
    Right,
    Forward,
    Backward,
    UpsideDown,
    Moving,
}

const AXIS_MIN: f32 = 0.65;

fn pose(s: Sample) -> Pose {
    let m2 = s.x * s.x + s.y * s.y + s.z * s.z;
    if !(0.7 * 0.7..=1.3 * 1.3).contains(&m2) {
        return Pose::Moving;
    }
    let (ax, ay, az) = (s.x.abs(), s.y.abs(), s.z.abs());
    if ax > ay && ax > az {
        if s.x > AXIS_MIN {
            return Pose::Straight;
        }
        if s.x < -AXIS_MIN {
            return Pose::UpsideDown;
        }
    } else if ay > ax && ay > az {
        if s.y > AXIS_MIN {
            return Pose::Left;
        }
        if s.y < -AXIS_MIN {
            return Pose::Right;
        }
    } else if az > ax && az > ay {
        if s.z > AXIS_MIN {
            return Pose::Backward;
        }
        if s.z < -AXIS_MIN {
            return Pose::Forward;
        }
    }
    Pose::Moving
}

struct Accel {
    dev: Dev,
}

impl Accel {
    fn new(bus: &Bus) -> Result<Accel, String> {
        let dev = bus
            .device(pins::ADDR_ACCEL, 400_000)
            .map_err(|e| e.to_string())?;
        let id = dev.read_u8(WHO_AM_I).map_err(|e| e.to_string())?;
        if id != SC7A20_ID {
            return Err(format!("accelerometer id {id:#x}"));
        }
        let a = Accel { dev };
        a.init().map_err(|e| e.to_string())?;
        Ok(a)
    }

    fn init(&self) -> Result<(), EspError> {
        // Block data update, +-2 g, normal (10-bit) mode.
        self.dev.write_reg(CTRL_REG4, &[0x80])?;
        self.dev.read_u8(INT1_SRC)?;
        // 100 Hz, x/y/z on.
        self.dev.write_reg(CTRL_REG1, &[0x57])?;
        // FIFO on, stream mode.
        self.dev.write_reg(CTRL_REG5, &[0x40])?;
        self.dev.write_reg(FIFO_CTRL, &[0x80])?;
        sleep_ms(5);
        Ok(())
    }

    fn sample(&self) -> Option<Sample> {
        let mut b = [0u8; 6];
        self.dev.read_reg(OUT_X_L | AUTO_INC, &mut b).ok()?;
        // Normal mode: 10 bits, left-aligned, 4 mg per digit at +-2 g.
        let g = |lo: u8, hi: u8| (i16::from_le_bytes([lo, hi]) >> 6) as f32 * 4.0 / 1000.0;
        Some(Sample {
            x: g(b[0], b[1]),
            y: g(b[2], b[3]),
            z: g(b[4], b[5]),
        })
    }

    /// What the FIFO holds (up to 32 samples).
    fn drain(&self, out: &mut Vec<Sample>) {
        out.clear();
        let Ok(src) = self.dev.read_u8(FIFO_SRC) else {
            return;
        };
        let n = (src & 0x1f) as usize + if src & 0x40 != 0 { 1 } else { 0 };
        for _ in 0..n.min(32) {
            if let Some(s) = self.sample() {
                out.push(s);
            }
        }
    }
}

/// Shake: high variance over the last 64 samples.
struct Shake {
    window: [Sample; 64],
    at: usize,
    count: usize,
    shaking: bool,
}

impl Shake {
    const THRESHOLD: f32 = 0.4;

    fn new() -> Shake {
        Shake {
            window: [Sample::default(); 64],
            at: 0,
            count: 0,
            shaking: false,
        }
    }

    fn add(&mut self, s: Sample) {
        self.window[self.at] = s;
        self.at = (self.at + 1) % self.window.len();
        self.count = (self.count + 1).min(self.window.len());
    }

    fn detected(&mut self) -> bool {
        if self.count == 0 {
            return false;
        }
        let w = &self.window[..self.count];
        let n = w.len() as f32;
        let var = |f: fn(&Sample) -> f32| {
            let (s, s2) = w
                .iter()
                .fold((0.0, 0.0), |(a, b), x| (a + f(x), b + f(x) * f(x)));
            s2 / n - (s / n) * (s / n)
        };
        let high = var(|s| s.x) > Self::THRESHOLD
            || var(|s| s.y) > Self::THRESHOLD
            || var(|s| s.z) > Self::THRESHOLD;
        let fire = high && !self.shaking;
        self.shaking = high;
        fire
    }
}

fn mean(v: &[Sample]) -> Sample {
    let n = v.len().max(1) as f32;
    let s = v.iter().fold(Sample::default(), |a, b| Sample {
        x: a.x + b.x,
        y: a.y + b.y,
        z: a.z + b.z,
    });
    Sample {
        x: s.x / n,
        y: s.y / n,
        z: s.z / n,
    }
}

fn range(v: &[Sample]) -> f32 {
    let r = |f: fn(&Sample) -> f32| {
        let (lo, hi) = v.iter().fold((f32::MAX, f32::MIN), |(lo, hi), s| {
            (lo.min(f(s)), hi.max(f(s)))
        });
        hi - lo
    };
    if v.is_empty() {
        return 0.0;
    }
    r(|s| s.x).max(r(|s| s.y)).max(r(|s| s.z))
}

fn diff(a: Sample, b: Sample) -> f32 {
    (a.x - b.x)
        .abs()
        .max((a.y - b.y).abs())
        .max((a.z - b.z).abs())
}

pub struct Motion {
    accel: Accel,
    lifted: AtomicBool,
    pose: AtomicU8,
}

/// While lifted: the same pose for this long is an event.
const CANDIDATE_HOLD_MS: u64 = 2000;
const GROSS_MOTION: f32 = 0.3;
const MEAN_TOLERANCE: f32 = 0.02;
/// Batches upright and still that end a lift.
const STILL_BATCHES: u8 = 6;

impl Motion {
    pub fn new(bus: &Bus) -> Result<Motion, String> {
        Ok(Motion {
            accel: Accel::new(bus)?,
            lifted: AtomicBool::new(false),
            pose: AtomicU8::new(Pose::Straight as u8),
        })
    }

    /// The floor sensor's view: off the ground or not.
    pub fn set_lifted(&self, lifted: bool) {
        self.lifted.store(lifted, Ordering::Relaxed);
    }

    /// On a side, its nose, its back or upside down (moving does not count).
    pub fn tipped(&self) -> bool {
        let p = self.pose.load(Ordering::Relaxed);
        p != Pose::Straight as u8 && p != Pose::Moving as u8
    }

    pub fn start<E: From<MotionEvent> + Send + 'static>(self: &Arc<Self>, tx: Sender<E>) {
        let m = self.clone();
        util::spawn(c"motion", 4096, 5, Stack::Psram, move || m.run(&tx));
    }

    fn run<E: From<MotionEvent>>(&self, tx: &Sender<E>) {
        let mut batch = Vec::with_capacity(32);
        let mut shake = Shake::new();
        let mut was_lifted = false;
        let mut candidate = MotionKind::Shake;
        let mut candidate_since = 0;
        let mut last_pose = Pose::Moving;
        let mut last_event: Option<MotionKind> = None;
        let mut pickup_done = false;
        let mut still = 0u8;
        let mut saw_motion = false;
        let mut loop_ref: Option<Sample> = None;
        let mut streak_ref = Sample::default();
        loop {
            sleep_ms(BATCH_MS);
            self.accel.drain(&mut batch);
            if batch.is_empty() {
                continue;
            }
            let m = mean(&batch);
            let current = pose(m);
            self.pose.store(current as u8, Ordering::Relaxed);

            let lifted = self.lifted.load(Ordering::Relaxed);
            if !lifted {
                was_lifted = false;
                continue;
            }
            if !was_lifted {
                was_lifted = true;
                shake = Shake::new();
                last_pose = Pose::Moving;
                last_event = None;
                candidate = MotionKind::Shake;
                candidate_since = millis();
                pickup_done = false;
                still = 0;
                saw_motion = false;
                loop_ref = None;
            }

            let first = pose(batch[0]);
            for s in &batch {
                shake.add(*s);
            }
            if shake.detected() {
                last_pose = Pose::Moving;
                candidate = MotionKind::Shake;
                last_event = Some(MotionKind::Shake);
                candidate_since = millis();
                let _ = tx.send(MotionEvent::Motion(MotionKind::Shake).into());
            }
            let r = range(&batch);
            let reference = *loop_ref.get_or_insert(m);
            if r >= GROSS_MOTION || diff(m, reference) >= MEAN_TOLERANCE {
                saw_motion = true;
            }
            let dev = if still == 0 { 0.0 } else { diff(m, streak_ref) };
            if first == Pose::Straight && r < GROSS_MOTION && dev <= MEAN_TOLERANCE {
                if still == 0 {
                    streak_ref = m;
                }
                still += 1;
            } else {
                still = 0;
            }
            if still >= STILL_BATCHES {
                // Set down again.
                self.lifted.store(false, Ordering::Relaxed);
                was_lifted = false;
                let _ = tx.send(MotionEvent::SetDown.into());
                continue;
            }

            let now_candidate = match first {
                Pose::Moving | Pose::Straight => MotionKind::Pickup,
                Pose::UpsideDown => MotionKind::UpsideDown,
                _ => MotionKind::Fall,
            };
            if now_candidate != candidate || (candidate == MotionKind::Fall && last_pose != first) {
                candidate_since = millis();
                last_pose = first;
                candidate = now_candidate;
            } else if millis() - candidate_since >= CANDIDATE_HOLD_MS {
                if last_event == Some(candidate) {
                    continue;
                }
                if candidate == MotionKind::Pickup && (!saw_motion || pickup_done) {
                    continue;
                }
                match candidate {
                    MotionKind::Pickup => pickup_done = true,
                    MotionKind::Fall | MotionKind::UpsideDown => pickup_done = false,
                    MotionKind::Shake => {}
                }
                let _ = tx.send(MotionEvent::Motion(candidate).into());
                last_event = Some(candidate);
            }
        }
    }
}
